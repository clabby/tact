//! Linked machines: other computers running `tact serve` whose sessions this hub relays.
//!
//! Each machine is one file, `$TACT_HOME/web/machines/<name>.toml`, holding the peer's address and
//! its web token. Only the `tact machine` command writes these files, and the web server reads them
//! on every request, so a change takes effect without a restart. The tokens grant full control of
//! the peers, so the directory and files are private to the user and the tokens never reach the
//! browser, the configuration file, or any output.

use super::{token::MachineToken, wire::PROTOCOL_VERSION};
use crate::app::{error::MachineError, secret::SecretString};
use reqwest::{
    Client, StatusCode,
    header::{self, HeaderValue},
};
use serde::Deserialize;
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::time::timeout;
use url::{Host, Url};
use zeroize::Zeroizing;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const KEEP_ALIVE: Duration = Duration::from_secs(30);
const MAX_NAME_BYTES: usize = 32;
/// Linking waits this long for the peer's whole answer.
const VERIFY_TIMEOUT: Duration = Duration::from_secs(10);
/// A peer's `/api/instance` answer is a few hundred bytes.
const MAX_VERIFY_BYTES: usize = 64 * 1024;

/// A linked machine.
#[derive(Debug)]
pub(crate) struct Machine {
    pub(crate) name: String,
    /// The peer's origin, for example `https://devbox.tail1234.ts.net`, without a trailing slash.
    pub(crate) origin: String,
    pub(crate) token: MachineToken,
}

impl Machine {
    /// The `Cookie` header value that authenticates to the peer, as its sign-in would set it.
    ///
    /// The value is marked sensitive, so its `Debug` output is redacted and HTTP/2 never indexes
    /// it. The header value and the copies the HTTP client makes while sending are owned by
    /// `http` and `reqwest` and are not zeroized; only the token this crate owns is.
    pub(crate) fn cookie(&self) -> HeaderValue {
        let cookie = Zeroizing::new(format!("tact={}", self.token.expose()));
        let mut value =
            HeaderValue::from_str(&cookie).expect("base64url cookies are valid header values");
        value.set_sensitive(true);
        value
    }
}

/// The directory of machine files.
pub(crate) struct Registry {
    web_directory: PathBuf,
    directory: PathBuf,
    /// Plain HTTP would send the peer token in clear text to whoever answers on the address, so
    /// only tests, whose peers are local mock servers, turn this off.
    require_https: bool,
}

/// The file format. Names come from the file name, so they cannot disagree.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MachineFile {
    url: String,
    /// The TOML parser keeps transient, non-zeroized copies of the text it reads; only the copies
    /// this crate owns are zeroized.
    token: SecretString,
}

#[derive(Deserialize)]
struct InstanceInfo {
    protocol_version: u32,
}

/// What linking learned about the peer.
#[derive(Debug)]
pub(crate) struct Linked {
    pub(crate) protocol_version: u32,
}

impl Linked {
    /// Whether this hub's web interface speaks the peer's protocol. The browser refuses to stream
    /// from a peer that does not.
    pub(crate) fn compatible(&self) -> bool {
        self.protocol_version == PROTOCOL_VERSION
    }
}

/// The client for every request to a peer. Requests carry the peer token, so they go only to the
/// stored address over verified TLS: never through an environment proxy, and never on to wherever a
/// redirect points. There is no read or total timeout because some relayed requests run an agent
/// and send nothing until it finishes; callers bound waiting where it matters.
pub(crate) fn peer_client() -> reqwest::Result<Client> {
    peer_client_builder().https_only(true).build()
}

fn peer_client_builder() -> reqwest::ClientBuilder {
    Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .no_gzip()
        .no_brotli()
        .no_zstd()
        .no_deflate()
        .connect_timeout(CONNECT_TIMEOUT)
        .tcp_keepalive(KEEP_ALIVE)
}

/// A peer client that also speaks plain HTTP, for tests whose peers are local mock servers.
#[cfg(test)]
pub(crate) fn test_peer_client() -> Client {
    crate::install_tls_provider();
    peer_client_builder().build().unwrap()
}

impl Registry {
    /// The registry in `web_directory`, normally `$TACT_HOME/web`.
    pub(crate) fn new(web_directory: &Path) -> Self {
        Self {
            web_directory: web_directory.to_owned(),
            directory: web_directory.join("machines"),
            require_https: true,
        }
    }

    /// A registry that also accepts `http` addresses, for tests.
    #[cfg(test)]
    pub(crate) fn allowing_http(web_directory: &Path) -> Self {
        Self {
            require_https: false,
            ..Self::new(web_directory)
        }
    }

    /// Validates a machine description as given on the command line.
    pub(crate) fn machine(
        &self,
        name: &str,
        url: &str,
        token: &SecretString,
    ) -> Result<Machine, MachineError> {
        if !valid_name(name) {
            return Err(MachineError::InvalidName);
        }
        Ok(Machine {
            name: name.to_owned(),
            origin: self.origin(url)?,
            token: MachineToken::parse(token.expose_secret()).ok_or(MachineError::InvalidToken)?,
        })
    }

    /// The machine named `name`, read from disk now. An invalid name or a missing file is `None`.
    pub(crate) fn load(&self, name: &str) -> Result<Option<Machine>, MachineError> {
        if !valid_name(name) {
            return Ok(None);
        }
        let path = self.path(name);
        let contents = match fs::read_to_string(&path) {
            Ok(contents) => Zeroizing::new(contents),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(MachineError::Io {
                    action: "read",
                    path,
                    source,
                });
            }
        };
        let file: MachineFile =
            toml::from_str(&contents).map_err(|_| MachineError::Malformed(path.clone()))?;
        self.machine(name, &file.url, &file.token)
            .map(Some)
            .map_err(|_| MachineError::Malformed(path))
    }

    /// Every readable, well-formed machine, by name.
    pub(crate) fn all(&self) -> Vec<Machine> {
        let Ok(entries) = fs::read_dir(&self.directory) else {
            return Vec::new();
        };
        let mut machines: Vec<Machine> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let file_name = entry.file_name();
                let name = file_name.to_str()?.strip_suffix(".toml")?;
                self.load(name).ok().flatten()
            })
            .collect();
        machines.sort_by(|left, right| left.name.cmp(&right.name));
        machines
    }

    /// Verifies that the peer accepts the token, then saves the machine.
    ///
    /// Nothing is written unless the peer answered as a Tact server that accepts the token.
    pub(crate) async fn link(
        &self,
        client: &Client,
        machine: &Machine,
        replace: bool,
    ) -> Result<Linked, MachineError> {
        // Equal tokens mean a synced `$TACT_HOME`, or a peer that is this machine.
        let local = MachineToken::read(&self.web_directory).map_err(|source| MachineError::Io {
            action: "read this machine's web token in",
            path: self.web_directory.clone(),
            source,
        })?;
        if local.is_some_and(|local| local.matches(machine.token.expose())) {
            return Err(MachineError::LocalToken);
        }
        if !replace && self.path(&machine.name).exists() {
            return Err(MachineError::Exists(machine.name.clone()));
        }
        let answer = async {
            let response = client
                .get(format!("{}/api/instance", machine.origin))
                .header(header::COOKIE, machine.cookie())
                .header(header::ACCEPT, "application/json")
                .send()
                .await
                .map_err(MachineError::Unreachable)?;
            match response.status() {
                StatusCode::OK => {}
                StatusCode::UNAUTHORIZED => return Err(MachineError::Unauthorized),
                status => return Err(MachineError::UnexpectedStatus(status.as_u16())),
            }
            read_limited(response, MAX_VERIFY_BYTES)
                .await
                .map_err(MachineError::Unreachable)?
                .ok_or(MachineError::NotTact)
        };
        let body = timeout(VERIFY_TIMEOUT, answer)
            .await
            .map_err(|_| MachineError::TimedOut(VERIFY_TIMEOUT))??;
        let info: InstanceInfo =
            serde_json::from_slice(&body).map_err(|_| MachineError::NotTact)?;
        self.save(machine, replace)?;
        Ok(Linked {
            protocol_version: info.protocol_version,
        })
    }

    pub(crate) fn remove(&self, name: &str) -> Result<(), MachineError> {
        if !valid_name(name) {
            return Err(MachineError::InvalidName);
        }
        let path = self.path(name);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                Err(MachineError::Unknown(name.to_owned()))
            }
            Err(source) => Err(MachineError::Io {
                action: "remove",
                path,
                source,
            }),
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.directory.join(format!("{name}.toml"))
    }

    /// Writes the file atomically: a private temporary file is synced, then moved into place. A
    /// new machine is linked into place, which fails if another one appeared meanwhile.
    fn save(&self, machine: &Machine, replace: bool) -> Result<(), MachineError> {
        let path = self.path(&machine.name);
        let error = |action, path: &Path| {
            let path = path.to_owned();
            move |source| MachineError::Io {
                action,
                path,
                source,
            }
        };
        create_private_directory(&self.directory)
            .map_err(error("create the directory", &self.directory))?;
        let temporary =
            self.directory
                .join(format!(".{}.toml.{}", machine.name, std::process::id()));
        let contents = Zeroizing::new(format!(
            "url = \"{}\"\ntoken = \"{}\"\n",
            machine.origin,
            machine.token.expose()
        ));
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let written = options.open(&temporary).and_then(|mut file| {
            file.write_all(contents.as_bytes())?;
            file.sync_all()
        });
        if let Err(source) = written {
            drop(fs::remove_file(&temporary));
            return Err(error("write", &temporary)(source));
        }
        let placed = if replace {
            fs::rename(&temporary, &path)
        } else {
            let linked = fs::hard_link(&temporary, &path);
            drop(fs::remove_file(&temporary));
            linked
        };
        match placed {
            Ok(()) => {}
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
                return Err(MachineError::Exists(machine.name.clone()));
            }
            Err(source) => {
                drop(fs::remove_file(&temporary));
                return Err(error("write", &path)(source));
            }
        }
        fs::File::open(&self.directory)
            .and_then(|directory| directory.sync_all())
            .map_err(error("sync", &self.directory))
    }

    /// The origin of a peer address: `https`, a host that is not this computer, and an optional
    /// port, with nothing else.
    fn origin(&self, url: &str) -> Result<String, MachineError> {
        let url = Url::parse(url).map_err(|_| MachineError::InvalidUrl("is not a URL"))?;
        let scheme_allowed = match url.scheme() {
            "https" => true,
            "http" => !self.require_https,
            _ => false,
        };
        if !scheme_allowed {
            return Err(MachineError::InvalidUrl("must use https"));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(MachineError::InvalidUrl(
                "must not contain a user name or password",
            ));
        }
        if url.path() != "/" {
            return Err(MachineError::InvalidUrl("must not have a path"));
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err(MachineError::InvalidUrl(
                "must not have a query or fragment",
            ));
        }
        let loopback = match url.host() {
            None => return Err(MachineError::InvalidUrl("has no host")),
            Some(Host::Domain(domain)) => {
                let domain = domain.trim_end_matches('.');
                domain == "localhost" || domain.ends_with(".localhost")
            }
            Some(Host::Ipv4(address)) => address.is_loopback() || address.is_unspecified(),
            Some(Host::Ipv6(address)) => {
                let mapped = address.to_ipv4_mapped();
                address.is_loopback()
                    || address.is_unspecified()
                    || mapped.is_some_and(|mapped| mapped.is_loopback() || mapped.is_unspecified())
            }
        };
        if loopback && self.require_https {
            return Err(MachineError::InvalidUrl(
                "must name another machine, not this one",
            ));
        }
        Ok(url.origin().ascii_serialization())
    }
}

/// A peer's response body, or `None` once it grows past `limit` bytes.
pub(crate) async fn read_limited(
    mut response: reqwest::Response,
    limit: usize,
) -> reqwest::Result<Option<Vec<u8>>> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len() + chunk.len() > limit {
            return Ok(None);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Some(body))
}

fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=MAX_NAME_BYTES).contains(&bytes.len())
        && bytes[0] != b'-'
        && bytes
            .iter()
            .all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'-'))
}

fn create_private_directory(directory: &Path) -> io::Result<()> {
    fs::create_dir_all(directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Machine, MachineError, Registry, peer_client, test_peer_client};
    use crate::{
        app::secret::SecretString,
        web::{
            testing::{Upstream, with_proxy_environment},
            token::MachineToken,
        },
    };
    use axum::{
        Json, Router,
        http::{HeaderMap, StatusCode, header},
        response::IntoResponse,
        routing::get,
    };
    use std::{fs, time::Duration};
    use tempfile::TempDir;

    const TOKEN: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    const OTHER_TOKEN: &str = "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBA";

    /// A peer that accepts `token` and nothing else.
    async fn peer(token: &'static str) -> Upstream {
        Upstream::spawn(Router::new().route(
            "/api/instance",
            get(move |headers: HeaderMap| async move {
                let cookie = format!("tact={token}");
                if headers
                    .get(header::COOKIE)
                    .is_some_and(|value| value == cookie.as_str())
                {
                    Json(serde_json::json!({"protocol_version": 9, "live": 0})).into_response()
                } else {
                    StatusCode::UNAUTHORIZED.into_response()
                }
            }),
        ))
        .await
    }

    fn registry() -> (TempDir, Registry) {
        let home = tempfile::tempdir().unwrap();
        let registry = Registry::allowing_http(&home.path().join("web"));
        (home, registry)
    }

    fn machine(registry: &Registry, name: &str, origin: &str, token: &str) -> Machine {
        registry
            .machine(name, origin, &SecretString::new(token.to_owned()))
            .unwrap()
    }

    fn files(registry: &Registry) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(&registry.directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[tokio::test]
    async fn linking_saves_a_private_file_once_the_peer_accepts_the_token() {
        let (_home, registry) = registry();
        let peer = peer(TOKEN).await;

        let linked = registry
            .link(
                &test_peer_client(),
                &machine(&registry, "devbox", &peer.origin, TOKEN),
                false,
            )
            .await
            .unwrap();

        assert!(linked.compatible());
        assert_eq!(peer.hits(), 1);
        assert_eq!(files(&registry), ["devbox.toml"]);
        let loaded = registry.load("devbox").unwrap().unwrap();
        assert_eq!(loaded.origin, peer.origin);
        assert_eq!(loaded.token.expose(), TOKEN);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode =
                |path: &std::path::Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&registry.directory), 0o700);
            assert_eq!(mode(&registry.directory.join("devbox.toml")), 0o600);
        }
    }

    #[tokio::test]
    async fn relinking_requires_replace_and_swaps_the_file_atomically() {
        let (_home, registry) = registry();
        let (first, second) = (peer(TOKEN).await, peer(OTHER_TOKEN).await);
        let client = test_peer_client();
        registry
            .link(
                &client,
                &machine(&registry, "devbox", &first.origin, TOKEN),
                false,
            )
            .await
            .unwrap();
        let replacement = machine(&registry, "devbox", &second.origin, OTHER_TOKEN);

        let refused = registry.link(&client, &replacement, false).await;

        assert!(
            matches!(refused, Err(MachineError::Exists(_))),
            "{refused:?}"
        );
        assert_eq!(second.hits(), 0);
        assert_eq!(
            registry.load("devbox").unwrap().unwrap().token.expose(),
            TOKEN
        );
        registry.link(&client, &replacement, true).await.unwrap();
        let loaded = registry.load("devbox").unwrap().unwrap();
        assert_eq!(
            (loaded.origin.as_str(), loaded.token.expose()),
            (second.origin.as_str(), OTHER_TOKEN)
        );
        assert_eq!(
            files(&registry),
            ["devbox.toml"],
            "no temporary file remains"
        );
    }

    #[tokio::test]
    async fn a_refused_or_redirected_link_saves_nothing() {
        let (_home, registry) = registry();
        let refusing = peer(OTHER_TOKEN).await;
        let redirecting = Upstream::spawn(Router::new().route(
            "/api/instance",
            get(|| async {
                (
                    StatusCode::FOUND,
                    [(header::LOCATION, "https://example.com/")],
                )
            }),
        ))
        .await;
        let client = test_peer_client();

        let refused = registry
            .link(
                &client,
                &machine(&registry, "devbox", &refusing.origin, TOKEN),
                false,
            )
            .await;
        let redirected = registry
            .link(
                &client,
                &machine(&registry, "devbox", &redirecting.origin, TOKEN),
                false,
            )
            .await;

        assert!(
            matches!(refused, Err(MachineError::Unauthorized)),
            "{refused:?}"
        );
        assert!(
            matches!(redirected, Err(MachineError::UnexpectedStatus(302))),
            "{redirected:?}"
        );
        assert!(registry.load("devbox").unwrap().is_none());
        assert!(registry.all().is_empty());
    }

    #[tokio::test]
    async fn this_machines_own_token_is_refused_without_contacting_the_peer() {
        let (home, registry) = registry();
        let local = MachineToken::load_or_create(&home.path().join("web")).unwrap();
        let peer = peer(TOKEN).await;

        let linked = registry
            .link(
                &test_peer_client(),
                &machine(&registry, "devbox", &peer.origin, local.expose()),
                false,
            )
            .await;

        assert!(
            matches!(linked, Err(MachineError::LocalToken)),
            "{linked:?}"
        );
        assert_eq!(peer.hits(), 0);
        assert!(registry.load("devbox").unwrap().is_none());
    }

    #[test]
    fn addresses_must_be_bare_https_origins_of_another_machine() {
        let home = tempfile::tempdir().unwrap();
        let registry = Registry::new(&home.path().join("web"));
        let token = SecretString::new(TOKEN.to_owned());
        let origin = |url: &str| {
            registry
                .machine("devbox", url, &token)
                .map(|machine| machine.origin)
        };

        for rejected in [
            "http://devbox.example.net",
            "ftp://devbox.example.net",
            "https://localhost",
            "https://app.localhost:8443",
            "https://127.0.0.1",
            "https://0.0.0.0",
            "https://[::1]",
            "https://[::ffff:127.0.0.1]",
            "https://[::ffff:0.0.0.0]",
            "https://user:secret@devbox.example.net",
            "https://user@devbox.example.net",
            "https://devbox.example.net/tact",
            "https://devbox.example.net/?tact=1",
            "https://devbox.example.net/#tact",
            "devbox.example.net",
        ] {
            assert!(
                matches!(origin(rejected), Err(MachineError::InvalidUrl(_))),
                "{rejected}"
            );
        }
        assert_eq!(
            origin("https://Devbox.tail1234.ts.net/").unwrap(),
            "https://devbox.tail1234.ts.net"
        );
        assert_eq!(
            origin("https://devbox.example.net:8443").unwrap(),
            "https://devbox.example.net:8443"
        );
    }

    #[test]
    fn names_and_tokens_are_validated() {
        let (_home, registry) = registry();
        let valid = SecretString::new(TOKEN.to_owned());
        for name in [
            "",
            "-devbox",
            "Devbox",
            "dev_box",
            "dev/box",
            "..",
            "a".repeat(33).as_str(),
        ] {
            assert!(
                matches!(
                    registry.machine(name, "https://devbox.example.net", &valid),
                    Err(MachineError::InvalidName)
                ),
                "{name:?}"
            );
            assert!(registry.load(name).unwrap().is_none(), "{name:?}");
        }
        assert!(
            registry
                .machine(&"a".repeat(32), "https://devbox.example.net", &valid)
                .is_ok()
        );
        for token in [
            "",
            "short",
            &TOKEN[1..],
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA+",
            &format!("{TOKEN}A"),
        ] {
            let token = SecretString::new(token.to_owned());
            assert!(matches!(
                registry.machine("devbox", "https://devbox.example.net", &token),
                Err(MachineError::InvalidToken)
            ),);
        }
    }

    #[test]
    fn malformed_files_and_debug_output_never_reveal_the_token() {
        let (_home, registry) = registry();
        fs::create_dir_all(&registry.directory).unwrap();
        fs::write(
            registry.directory.join("devbox.toml"),
            format!("url = \"https://devbox.example.net\"\ntoken = \"{TOKEN}\"\nextra = 1\n"),
        )
        .unwrap();

        let error = registry.load("devbox").unwrap_err();

        assert!(matches!(error, MachineError::Malformed(_)));
        assert!(!format!("{error} {error:?}").contains(TOKEN));
        let machine = machine(&registry, "devbox", "https://devbox.example.net", TOKEN);
        assert!(!format!("{machine:?}").contains(TOKEN));
    }

    #[test]
    fn removing_unlinks_the_file() {
        let (_home, registry) = registry();
        fs::create_dir_all(&registry.directory).unwrap();
        fs::write(
            registry.directory.join("devbox.toml"),
            format!("url = \"https://devbox.example.net\"\ntoken = \"{TOKEN}\"\n"),
        )
        .unwrap();
        assert_eq!(registry.all().len(), 1);

        registry.remove("devbox").unwrap();

        assert!(registry.all().is_empty());
        assert!(matches!(
            registry.remove("devbox"),
            Err(MachineError::Unknown(_))
        ));
        assert!(matches!(
            registry.remove("../token"),
            Err(MachineError::InvalidName)
        ));
    }
    #[tokio::test]
    async fn the_production_client_refuses_plain_http_without_connecting() {
        let (_home, registry) = registry();
        let peer = peer(TOKEN).await;
        crate::install_tls_provider();

        let linked = registry
            .link(
                &peer_client().unwrap(),
                &machine(&registry, "devbox", &peer.origin, TOKEN),
                false,
            )
            .await;

        assert!(
            matches!(linked, Err(MachineError::Unreachable(_))),
            "{linked:?}"
        );
        assert_eq!(peer.hits(), 0);
    }

    #[tokio::test]
    async fn the_production_client_ignores_environment_proxies() {
        let home = tempfile::tempdir().unwrap();
        let registry = Registry::new(&home.path().join("web"));
        let proxy = peer(TOKEN).await;
        crate::install_tls_provider();
        let client = with_proxy_environment(&proxy.origin, || peer_client().unwrap());

        let linked = registry
            .link(
                &client,
                &machine(&registry, "devbox", "https://devbox.invalid", TOKEN),
                false,
            )
            .await;

        assert!(
            matches!(linked, Err(MachineError::Unreachable(_))),
            "{linked:?}"
        );
        assert_eq!(proxy.hits(), 0, "the token never goes through a proxy");
    }

    #[tokio::test]
    async fn the_verification_answer_is_bounded_in_size_and_time() {
        let (_home, registry) = registry();
        let padding = "x".repeat(64 * 1024);
        let oversized = Upstream::spawn(Router::new().route(
            "/api/instance",
            get(move || async move {
                Json(serde_json::json!({"protocol_version": 9, "padding": padding}))
            }),
        ))
        .await;
        let silent = Upstream::spawn(
            Router::new().route("/api/instance", get(std::future::pending::<StatusCode>)),
        )
        .await;
        let client = test_peer_client();

        let linked = registry
            .link(
                &client,
                &machine(&registry, "devbox", &oversized.origin, TOKEN),
                false,
            )
            .await;
        assert!(matches!(linked, Err(MachineError::NotTact)), "{linked:?}");

        let silent_machine = machine(&registry, "devbox", &silent.origin, TOKEN);
        let link = registry.link(&client, &silent_machine, false);
        let waiting = async {
            while silent.hits() == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            // The request is in flight; a paused clock now skips straight to the timeout.
            tokio::time::pause();
            std::future::pending::<()>().await;
        };
        let linked = tokio::select! {
            linked = link => linked,
            () = waiting => unreachable!(),
        };
        assert!(
            matches!(linked, Err(MachineError::TimedOut(_))),
            "{linked:?}"
        );
        assert!(registry.all().is_empty());
    }

    #[test]
    fn the_peer_cookie_is_sensitive() {
        let (_home, registry) = registry();

        let cookie = machine(&registry, "devbox", "https://devbox.example.net", TOKEN).cookie();

        assert_eq!(cookie, format!("tact={TOKEN}").as_str());
        assert!(cookie.is_sensitive());
        assert_eq!(format!("{cookie:?}"), "Sensitive");
    }
}
