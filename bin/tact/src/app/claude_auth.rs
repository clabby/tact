//! Private native capabilities for the upstream Claude subscription lifecycle.
//!
//! Tact zeroizes its keys, plaintext store buffers and login input. The upstream
//! manager owns ordinary strings for OAuth state and tokens; reqwest owns copies
//! of request bodies and headers, and AES-GCM owns its expanded cipher state.
//! Those dependency-owned copies are not covered by Tact's zeroization guarantee.
//! Credentials never enter agent journals.

use super::secret::SecretString;
use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use futures_util::StreamExt;
use nanocodex::claude::{
    ClaudeAuthFuture, ClaudeClient, SubscriptionIdentity,
    subscription::{
        ClaudeLoginMode, ClaudeSubscription, ClaudeSubscriptionCommit, ClaudeSubscriptionConfig,
        ClaudeSubscriptionError, ClaudeSubscriptionHost, ClaudeSubscriptionHostError,
        ClaudeSubscriptionHttpRequest, ClaudeSubscriptionHttpResponse, ClaudeSubscriptionStatus,
        ClaudeSubscriptionStoreValue,
    },
};
use std::{
    fs::{self, File, OpenOptions},
    io::{IsTerminal, Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

const STORE_KEY: &str = "claude-subscription";
const MAGIC: &[u8] = b"tact-claude-auth-v1\0";
const MAX_STORE_BYTES: u64 = 128 * 1024;
type HostResult<T> = std::result::Result<T, ClaudeSubscriptionHostError>;
type Result<T> = std::result::Result<T, ClaudeAuthError>;

#[derive(Debug, thiserror::Error)]
pub(crate) enum ClaudeAuthError {
    #[error(transparent)]
    Subscription(#[from] ClaudeSubscriptionError),
    #[error(transparent)]
    Host(#[from] ClaudeSubscriptionHostError),
    #[error("{0}")]
    Input(&'static str),
}

/// One instance supplies the shared subscription manager for a complete task tree.
pub(crate) struct ClaudeAuth {
    host: Arc<NativeHost>,
    subscription: Arc<ClaudeSubscription>,
}

impl ClaudeAuth {
    pub(crate) fn open(path: PathBuf) -> Result<Self> {
        Self::with_config(path, ClaudeSubscriptionConfig::default())
    }

    pub(crate) fn with_config(path: PathBuf, config: ClaudeSubscriptionConfig) -> Result<Self> {
        if path.as_os_str().is_empty() {
            return Err(ClaudeAuthError::Input("Claude credential path is empty"));
        }
        let host = Arc::new(NativeHost {
            path,
            http: subscription_http()?,
        });
        let subscription = Arc::new(ClaudeSubscription::new(host.clone(), STORE_KEY, config)?);
        Ok(Self { host, subscription })
    }

    pub(crate) fn subscription(&self) -> Arc<ClaudeSubscription> {
        self.subscription.clone()
    }

    pub(crate) fn client(&self, endpoint: Option<&str>) -> Result<ClaudeClient> {
        let install_id = self.host.with_store(|store| Ok(store.install_id.clone()))?;
        let client = match endpoint {
            Some(endpoint) => ClaudeClient::with_auth_provider(
                self.host.http.clone(),
                endpoint,
                self.subscription(),
            )
            .subscription_compatibility(),
            None => ClaudeClient::subscription(self.host.http.clone(), self.subscription()),
        };
        Ok(client.with_subscription_identity(SubscriptionIdentity {
            install_id: Some(install_id),
            ..Default::default()
        }))
    }

    pub(crate) async fn login(&self, open_automatically: bool) -> Result<ClaudeSubscriptionStatus> {
        let login = self
            .subscription
            .begin_login(ClaudeLoginMode::Manual)
            .await?;
        eprintln!(
            "Open this URL to sign in with Claude:\n\n{}\n",
            login.authorization_url
        );
        if open_automatically
            && super::browser::open(&login.authorization_url)
                .await
                .is_err()
        {
            eprintln!("Could not open a browser automatically. Open the URL above manually.");
        }
        eprintln!("Paste the returned code#state and press Enter (input is hidden):");
        let code = tokio::task::spawn_blocking(read_login_code)
            .await
            .map_err(|_| ClaudeAuthError::Input("could not read Claude login response"))??;
        Ok(self
            .subscription
            .complete_login(code.expose_secret())
            .await?)
    }

    pub(crate) async fn status(&self) -> Result<ClaudeSubscriptionStatus> {
        Ok(self.subscription.status().await?)
    }
    pub(crate) async fn logout(&self) -> Result<()> {
        Ok(self.subscription.logout().await?)
    }
}

fn subscription_http() -> HostResult<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .build()
        .map_err(|_| ClaudeSubscriptionHostError)
}

fn new_install_id() -> HostResult<String> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(|_| ClaudeSubscriptionHostError)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn read_login_code() -> Result<SecretString> {
    const MAX_CODE: usize = 8192;
    let mut value = Zeroizing::new(String::with_capacity(8192));
    if std::io::stdin().is_terminal() {
        use crossterm::{
            event::{self, Event, KeyCode, KeyModifiers},
            terminal,
        };
        struct RestoreTerminal;
        impl Drop for RestoreTerminal {
            fn drop(&mut self) {
                let _ = terminal::disable_raw_mode();
            }
        }
        terminal::enable_raw_mode()
            .map_err(|_| ClaudeAuthError::Input("could not hide Claude login input"))?;
        let _restore = RestoreTerminal;
        loop {
            match event::read()
                .map_err(|_| ClaudeAuthError::Input("could not read Claude login input"))?
            {
                Event::Key(key) if key.kind != event::KeyEventKind::Release => match key.code {
                    KeyCode::Enter => break,
                    KeyCode::Char('c' | 'd') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Err(ClaudeAuthError::Input("Claude login input cancelled"));
                    }
                    KeyCode::Backspace => {
                        value.pop();
                    }
                    KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        if character.len_utf8() > MAX_CODE.saturating_sub(value.len()) {
                            return Err(ClaudeAuthError::Input("Claude login input is too large"));
                        }
                        value.push(character)
                    }
                    _ => {}
                },
                Event::Paste(paste) => {
                    let paste = Zeroizing::new(paste);
                    if paste.len() > MAX_CODE.saturating_sub(value.len()) {
                        return Err(ClaudeAuthError::Input("Claude login input is too large"));
                    }
                    value.push_str(&paste);
                }
                _ => {}
            }
            if value.len() > MAX_CODE {
                return Err(ClaudeAuthError::Input("Claude login input is too large"));
            }
        }
        eprintln!();
    } else {
        let mut input = std::io::stdin().lock();
        let mut bytes = Zeroizing::new(Vec::with_capacity(MAX_CODE + 1));
        loop {
            let mut byte = [0];
            match input.read(&mut byte) {
                Ok(0) => break,
                Ok(_) if byte[0] == b'\n' => break,
                Ok(_) => bytes.push(byte[0]),
                Err(_) => return Err(ClaudeAuthError::Input("could not read Claude login input")),
            }
            if bytes.len() > MAX_CODE {
                return Err(ClaudeAuthError::Input("Claude login input is too large"));
            }
        }
        value.push_str(
            std::str::from_utf8(&bytes)
                .map_err(|_| ClaudeAuthError::Input("invalid Claude login input"))?,
        );
    }
    Ok(SecretString::new(value.trim().to_owned()))
}

#[derive(Clone)]
struct NativeHost {
    path: PathBuf,
    http: reqwest::Client,
}

#[derive(Zeroize, ZeroizeOnDrop)]
struct PrivateStore {
    revision: u64,
    install_id: String,
    payload: Option<String>,
}

impl std::fmt::Debug for PrivateStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PrivateStore([REDACTED])")
    }
}

impl PrivateStore {
    fn decode(bytes: &[u8]) -> HostResult<Self> {
        if bytes.len() < 40 {
            return Err(ClaudeSubscriptionHostError);
        }
        let revision = u64::from_le_bytes(
            bytes[..8]
                .try_into()
                .map_err(|_| ClaudeSubscriptionHostError)?,
        );
        let install_id = std::str::from_utf8(&bytes[8..40])
            .map_err(|_| ClaudeSubscriptionHostError)?
            .to_owned();
        if !install_id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ClaudeSubscriptionHostError);
        }
        let payload = if bytes.len() == 40 {
            None
        } else {
            Some(
                std::str::from_utf8(&bytes[40..])
                    .map_err(|_| ClaudeSubscriptionHostError)?
                    .to_owned(),
            )
        };
        Ok(Self {
            revision,
            install_id,
            payload,
        })
    }

    fn encode(&self) -> Zeroizing<Vec<u8>> {
        let payload = self.payload.as_deref().unwrap_or_default();
        let mut bytes = Zeroizing::new(Vec::with_capacity(40 + payload.len()));
        bytes.extend_from_slice(&self.revision.to_le_bytes());
        bytes.extend_from_slice(self.install_id.as_bytes());
        bytes.extend_from_slice(payload.as_bytes());
        bytes
    }
}

struct StoreLock(File);

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

impl NativeHost {
    fn with_store<T>(
        &self,
        action: impl FnOnce(&mut PrivateStore) -> HostResult<T>,
    ) -> HostResult<T> {
        let directory = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        private_directory(directory)?;
        let key_path = self.path.with_added_extension("key");
        let lock_path = self.path.with_added_extension("lock");
        let lock = private_open(&lock_path, true)?;
        fs2::FileExt::lock_exclusive(&lock).map_err(|_| ClaudeSubscriptionHostError)?;
        let _lock = StoreLock(lock);
        // RAII releases this lock on every return; the OS releases it on process
        // exit. First key publication uses the same lock as all loads and commits.
        let existing_key = private_read(&key_path, 32)?;
        let is_new = existing_key.is_none();
        let key = match existing_key {
            Some(bytes) if bytes.len() == 32 => bytes,
            Some(_) => return Err(ClaudeSubscriptionHostError),
            None => {
                if fs::symlink_metadata(&self.path).is_ok() {
                    return Err(ClaudeSubscriptionHostError);
                }
                let mut key = Zeroizing::new(vec![0; 32]);
                getrandom::fill(&mut key).map_err(|_| ClaudeSubscriptionHostError)?;
                atomic_private_write(&key_path, &key)?;
                key
            }
        };
        let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| ClaudeSubscriptionHostError)?;
        let stored = private_read(&self.path, MAX_STORE_BYTES)?;
        // The key and store form one installation. Losing either half must not
        // reset a revision that another process may still use as a CAS fence.
        if stored.is_none() && !is_new {
            return Err(ClaudeSubscriptionHostError);
        }
        let mut store = match stored {
            Some(bytes) => {
                if !bytes.starts_with(MAGIC) || bytes.len() < MAGIC.len() + 12 + 16 {
                    return Err(ClaudeSubscriptionHostError);
                }
                let plaintext = Zeroizing::new(
                    cipher
                        .decrypt(
                            Nonce::from_slice(&bytes[MAGIC.len()..MAGIC.len() + 12]),
                            Payload {
                                msg: &bytes[MAGIC.len() + 12..],
                                aad: MAGIC,
                            },
                        )
                        .map_err(|_| ClaudeSubscriptionHostError)?,
                );
                PrivateStore::decode(&plaintext)?
            }
            None => PrivateStore {
                revision: 0,
                install_id: new_install_id()?,
                payload: None,
            },
        };
        let prior_revision = store.revision;
        let result = action(&mut store)?;
        if is_new || store.revision != prior_revision {
            let plaintext = store.encode();
            let mut nonce = [0; 12];
            getrandom::fill(&mut nonce).map_err(|_| ClaudeSubscriptionHostError)?;
            let encrypted = cipher
                .encrypt(
                    Nonce::from_slice(&nonce),
                    Payload {
                        msg: &plaintext,
                        aad: MAGIC,
                    },
                )
                .map_err(|_| ClaudeSubscriptionHostError)?;
            let mut bytes = MAGIC.to_vec();
            bytes.extend_from_slice(&nonce);
            bytes.extend_from_slice(&encrypted);
            if bytes.len() as u64 > MAX_STORE_BYTES {
                return Err(ClaudeSubscriptionHostError);
            }
            atomic_private_write(&self.path, &bytes)?;
        }
        Ok(result)
    }
}

impl ClaudeSubscriptionHost for NativeHost {
    fn load<'a>(
        &'a self,
        key: &'a str,
    ) -> ClaudeAuthFuture<'a, HostResult<ClaudeSubscriptionStoreValue>> {
        Box::pin(async move {
            if key != STORE_KEY {
                return Err(ClaudeSubscriptionHostError);
            }
            let host = self.clone();
            tokio::task::spawn_blocking(move || {
                host.with_store(|store| {
                    Ok(ClaudeSubscriptionStoreValue {
                        revision: store.revision,
                        payload: store.payload.clone(),
                    })
                })
            })
            .await
            .map_err(|_| ClaudeSubscriptionHostError)?
        })
    }

    fn compare_and_swap<'a>(
        &'a self,
        key: &'a str,
        expected_revision: u64,
        payload: &'a str,
    ) -> ClaudeAuthFuture<'a, HostResult<ClaudeSubscriptionCommit>> {
        Box::pin(async move {
            if key != STORE_KEY || payload.len() > 64 * 1024 {
                return Err(ClaudeSubscriptionHostError);
            }
            let host = self.clone();
            let payload = Zeroizing::new(payload.to_owned());
            tokio::task::spawn_blocking(move || {
                host.with_store(|store| {
                    if store.revision != expected_revision {
                        return Ok(ClaudeSubscriptionCommit::Conflict(store.revision));
                    }
                    store.revision = store
                        .revision
                        .checked_add(1)
                        .ok_or(ClaudeSubscriptionHostError)?;
                    store.payload.zeroize();
                    store.payload = Some(payload.to_string());
                    Ok(ClaudeSubscriptionCommit::Committed(store.revision))
                })
            })
            .await
            .map_err(|_| ClaudeSubscriptionHostError)?
        })
    }

    fn request(
        &self,
        request: ClaudeSubscriptionHttpRequest,
    ) -> ClaudeAuthFuture<'_, HostResult<ClaudeSubscriptionHttpResponse>> {
        Box::pin(async move {
            let deadline = Duration::from_millis(request.timeout_millis());
            tokio::time::timeout(deadline, async {
                let method = reqwest::Method::from_bytes(request.method().as_bytes())
                    .map_err(|_| ClaudeSubscriptionHostError)?;
                let response = self
                    .http
                    .request(method, request.url())
                    .headers(request.headers().clone())
                    .header(reqwest::header::CONTENT_TYPE, request.content_type())
                    .body(request.body().to_owned())
                    .timeout(deadline)
                    .send()
                    .await
                    .map_err(|_| ClaudeSubscriptionHostError)?;
                let status = response.status().as_u16();
                let bound = request.max_response_bytes();
                if response
                    .content_length()
                    .is_some_and(|length| length > bound as u64)
                {
                    return Err(ClaudeSubscriptionHostError);
                }
                let mut stream = response.bytes_stream();
                let mut bytes = Zeroizing::new(Vec::with_capacity(bound));
                while let Some(chunk) = stream.next().await {
                    let chunk = chunk.map_err(|_| ClaudeSubscriptionHostError)?;
                    if chunk.len() > bound.saturating_sub(bytes.len()) {
                        return Err(ClaudeSubscriptionHostError);
                    }
                    bytes.extend_from_slice(&chunk);
                }
                let body = std::str::from_utf8(&bytes)
                    .map_err(|_| ClaudeSubscriptionHostError)?
                    .to_owned();
                Ok(ClaudeSubscriptionHttpResponse { status, body })
            })
            .await
            .map_err(|_| ClaudeSubscriptionHostError)?
        })
    }
}

fn private_directory(path: &Path) -> HostResult<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(path)
        .map_err(|_| ClaudeSubscriptionHostError)?;
    let metadata = fs::symlink_metadata(path).map_err(|_| ClaudeSubscriptionHostError)?;
    if !metadata.is_dir() {
        return Err(ClaudeSubscriptionHostError);
    }
    check_private(&metadata)
}

fn check_private(_metadata: &fs::Metadata) -> HostResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if _metadata.permissions().mode() & 0o077 != 0
            || _metadata.uid() != nix::unistd::geteuid().as_raw()
        {
            return Err(ClaudeSubscriptionHostError);
        }
        Ok(())
    }
    #[cfg(not(unix))]
    Err(ClaudeSubscriptionHostError)
}

fn private_open(path: &Path, create: bool) -> HostResult<File> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_file() {
                return Err(ClaudeSubscriptionHostError);
            }
            check_private(&metadata)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && create => {}
        Err(_) => return Err(ClaudeSubscriptionHostError),
    }
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(create)
        .create(create)
        .truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK);
    }
    let file = options
        .open(path)
        .map_err(|_| ClaudeSubscriptionHostError)?;
    let metadata = file.metadata().map_err(|_| ClaudeSubscriptionHostError)?;
    if !metadata.is_file() {
        return Err(ClaudeSubscriptionHostError);
    }
    check_private(&metadata)?;
    Ok(file)
}

fn private_read(path: &Path, max_bytes: u64) -> HostResult<Option<Zeroizing<Vec<u8>>>> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(ClaudeSubscriptionHostError),
        Ok(metadata) if !metadata.is_file() => return Err(ClaudeSubscriptionHostError),
        Ok(_) => {}
    }
    let mut bytes = Zeroizing::new(Vec::with_capacity((max_bytes + 1) as usize));
    private_open(path, false)?
        .take(max_bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ClaudeSubscriptionHostError)?;
    if bytes.len() as u64 > max_bytes {
        return Err(ClaudeSubscriptionHostError);
    }
    Ok(Some(bytes))
}

fn atomic_private_write(path: &Path, bytes: &[u8]) -> HostResult<()> {
    let directory = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    // Every published file must retain the private ownership and mode contract.
    if fs::symlink_metadata(path).is_ok() {
        let _ = private_open(path, false)?;
    }
    let mut temporary =
        tempfile::NamedTempFile::new_in(directory).map_err(|_| ClaudeSubscriptionHostError)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|_| ClaudeSubscriptionHostError)?;
    }
    temporary
        .write_all(bytes)
        .map_err(|_| ClaudeSubscriptionHostError)?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|_| ClaudeSubscriptionHostError)?;
    temporary
        .persist(path)
        .map_err(|_| ClaudeSubscriptionHostError)?;
    #[cfg(unix)]
    File::open(directory)
        .and_then(|file| file.sync_all())
        .map_err(|_| ClaudeSubscriptionHostError)?;
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn host(path: &Path) -> NativeHost {
        crate::install_tls_provider();
        NativeHost {
            path: path.to_owned(),
            http: subscription_http().unwrap(),
        }
    }

    #[tokio::test]
    async fn encrypted_store_reopens_and_fences_stale_writers() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("private/auth");
        let first = host(&path);
        let second = host(&path);
        assert_eq!(first.load(STORE_KEY).await.unwrap().revision, 0);
        let identity = first.with_store(|s| Ok(s.install_id.clone())).unwrap();
        assert_eq!(
            first
                .compare_and_swap(STORE_KEY, 0, "secret-sentinel")
                .await
                .unwrap(),
            ClaudeSubscriptionCommit::Committed(1)
        );
        assert_eq!(
            second
                .compare_and_swap(STORE_KEY, 0, "stale-secret")
                .await
                .unwrap(),
            ClaudeSubscriptionCommit::Conflict(1)
        );
        let stored = second.load(STORE_KEY).await.unwrap();
        assert_eq!(stored.payload.as_deref(), Some("secret-sentinel"));
        assert_eq!(
            second.with_store(|s| Ok(s.install_id.clone())).unwrap(),
            identity
        );
        assert!(
            !fs::read(&path)
                .unwrap()
                .windows(15)
                .any(|b| b == b"secret-sentinel")
        );
        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        for file in [
            &path,
            &path.with_added_extension("key"),
            &path.with_added_extension("lock"),
        ] {
            assert_eq!(
                fs::metadata(file).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[tokio::test]
    async fn logout_keeps_revision_and_installation_and_invalidates_pending_login() {
        crate::install_tls_provider();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("private/auth");
        let auth = ClaudeAuth::open(path.clone()).unwrap();
        let identity = auth.host.with_store(|s| Ok(s.install_id.clone())).unwrap();
        auth.subscription()
            .begin_login(ClaudeLoginMode::Manual)
            .await
            .unwrap();
        let pending = auth.host.load(STORE_KEY).await.unwrap();
        auth.logout().await.unwrap();
        let reopened = ClaudeAuth::open(path).unwrap();
        let logged_out = reopened.host.load(STORE_KEY).await.unwrap();
        assert!(logged_out.revision > pending.revision);
        assert_eq!(
            reopened
                .host
                .with_store(|s| Ok(s.install_id.clone()))
                .unwrap(),
            identity
        );
        assert_eq!(
            reopened
                .host
                .compare_and_swap(
                    STORE_KEY,
                    pending.revision,
                    pending.payload.as_deref().unwrap()
                )
                .await
                .unwrap(),
            ClaudeSubscriptionCommit::Conflict(logged_out.revision)
        );
        assert_eq!(
            reopened.status().await.unwrap(),
            ClaudeSubscriptionStatus::SignedOut
        );
        assert!(Arc::ptr_eq(&auth.subscription(), &auth.subscription()));
    }

    #[tokio::test]
    async fn refuses_public_files_symlinks_corruption_and_missing_store() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("private/auth");
        let host = host(&path);
        host.load(STORE_KEY).await.unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(host.load(STORE_KEY).await.is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let original = fs::read(&path).unwrap();
        let mut corrupt = original.clone();
        *corrupt.last_mut().unwrap() ^= 1;
        fs::write(&path, corrupt).unwrap();
        assert!(host.load(STORE_KEY).await.is_err());
        fs::write(&path, original).unwrap();
        let target = path.with_added_extension("target");
        fs::rename(&path, &target).unwrap();
        symlink(&target, &path).unwrap();
        assert!(host.load(STORE_KEY).await.is_err());
        fs::remove_file(&path).unwrap();
        assert!(host.load(STORE_KEY).await.is_err());
        assert!(path.with_added_extension("key").exists());
    }

    #[tokio::test]
    async fn independent_hosts_only_commit_one_revision() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("private/auth");
        let first = host(&path);
        let second = host(&path);
        let (a, b) = tokio::join!(
            first.compare_and_swap(STORE_KEY, 0, "first"),
            second.compare_and_swap(STORE_KEY, 0, "second")
        );
        let results = [a.unwrap(), b.unwrap()];
        assert_eq!(
            results
                .iter()
                .filter(|r| **r == ClaudeSubscriptionCommit::Committed(1))
                .count(),
            1
        );
        assert_eq!(
            results
                .iter()
                .filter(|r| **r == ClaudeSubscriptionCommit::Conflict(1))
                .count(),
            1
        );
    }

    #[test]
    fn process_cas_child() {
        let Some(path) = std::env::var_os("TACT_TEST_CLAUDE_CAS_PATH") else {
            return;
        };
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let result = host(Path::new(&path))
                .compare_and_swap(STORE_KEY, 0, "synthetic-payload")
                .await
                .unwrap();
            assert!(matches!(
                result,
                ClaudeSubscriptionCommit::Committed(1) | ClaudeSubscriptionCommit::Conflict(1)
            ));
        });
    }

    #[test]
    fn process_locks_coordinate_independent_writers() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("private/auth");
        let children: Vec<_> = (0..4)
            .map(|_| {
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "app::claude_auth::tests::process_cas_child"])
                    .env("TACT_TEST_CLAUDE_CAS_PATH", &path)
                    .stdout(std::process::Stdio::null())
                    .spawn()
                    .unwrap()
            })
            .collect();
        for mut child in children {
            assert!(child.wait().unwrap().success());
        }
        assert_eq!(host(&path).with_store(|s| Ok(s.revision)).unwrap(), 1);
    }

    #[tokio::test]
    async fn token_transport_rejects_redirects_and_bounds_streamed_bodies() {
        use axum::{
            body::Body,
            http::{Response, StatusCode},
            routing::post,
        };
        use std::sync::atomic::{AtomicUsize, Ordering};
        crate::install_tls_provider();
        for oversized in [false, true] {
            let requests = Arc::new(AtomicUsize::new(0));
            let count = requests.clone();
            let router = axum::Router::new().route(
                "/token",
                post(move || {
                    count.fetch_add(1, Ordering::SeqCst);
                    async move {
                        if oversized {
                            let chunks = futures_util::stream::iter([
                                Ok::<_, std::io::Error>(vec![b'x'; 40_000]),
                                Ok(vec![b'x'; 40_000]),
                            ]);
                            Response::new(Body::from_stream(chunks))
                        } else {
                            Response::builder()
                                .status(StatusCode::TEMPORARY_REDIRECT)
                                .header("location", "/token")
                                .body(Body::empty())
                                .unwrap()
                        }
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                axum::serve(listener, router).await.unwrap();
            });
            let directory = tempfile::tempdir().unwrap();
            let auth = ClaudeAuth::with_config(
                directory.path().join("private/auth"),
                ClaudeSubscriptionConfig {
                    token_url: format!("http://{address}/token"),
                    allow_loopback_http: true,
                    ..Default::default()
                },
            )
            .unwrap();
            let login = auth
                .subscription()
                .begin_login(ClaudeLoginMode::Manual)
                .await
                .unwrap();
            let url = url::Url::parse(&login.authorization_url).unwrap();
            let state = url
                .query_pairs()
                .find(|(name, _)| name == "state")
                .unwrap()
                .1
                .into_owned();
            let error = auth
                .subscription()
                .complete_login(&format!("synthetic-code#{state}"))
                .await
                .unwrap_err();
            if oversized {
                assert_eq!(error, ClaudeSubscriptionError::Host);
            }
            assert_eq!(requests.load(Ordering::SeqCst), 1);
            server.abort();
        }
    }

    #[test]
    fn application_secret_types_redact_and_zeroize() {
        fn assert_secret<T: Zeroize + ZeroizeOnDrop>() {}
        assert_secret::<PrivateStore>();
        assert_secret::<SecretString>();
        let mut store = PrivateStore {
            revision: 1,
            install_id: "public-installation".into(),
            payload: Some("secret-sentinel".into()),
        };
        assert_eq!(format!("{store:?}"), "PrivateStore([REDACTED])");
        store.zeroize();
        assert!(store.payload.as_deref().unwrap_or_default().is_empty());
        assert_eq!(
            format!("{:?}", ClaudeSubscriptionHostError),
            "ClaudeSubscriptionHostError"
        );
    }
}
