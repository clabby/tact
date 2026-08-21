use crate::app::{
    installation::current as installation,
    update::{UpdateError, download_verified_release_artifact},
};
use flate2::read::GzDecoder;
use fs2::FileExt;
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tempfile::tempdir_in;

const REVIEW_ASSETS_ENV: &str = "TACT_REVIEW_ASSETS";
const MANIFEST_NAME: &str = "manifest.json";
const BUNDLE_SCHEMA_VERSION: u32 = 2;
const REVIEW_API_VERSION: u32 = 4;
const MAX_ARCHIVE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_EXPANDED_BYTES: u64 = 32 * 1024 * 1024;
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum AssetAvailability {
    Ready(ReviewAssets),
    DownloadRequired,
    DevelopmentInstallRequired { path: PathBuf },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReviewAssets {
    path: PathBuf,
    manifest: Arc<ValidatedManifest>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ResolvedReviewAsset {
    pub(crate) path: PathBuf,
    pub(crate) content_type: String,
}

impl ReviewAssets {
    pub(crate) fn availability() -> Result<AssetAvailability, AssetError> {
        if let Some(path) = env::var_os(REVIEW_ASSETS_ENV).map(PathBuf::from) {
            return Self::from_directory(path, InstallKind::DevelopmentOverride)
                .map(AssetAvailability::Ready);
        }

        let path = install_path(&tact_home()?);
        if path.exists() {
            let can_download = !installation().is_development();
            let kind = if can_download {
                InstallKind::Managed
            } else {
                InstallKind::DevelopmentOverride
            };
            match Self::from_directory(path, kind) {
                Ok(assets) => return Ok(AssetAvailability::Ready(assets)),
                Err(_) if can_download => {
                    return Ok(AssetAvailability::DownloadRequired);
                }
                Err(error) => return Err(error),
            }
        }
        if !installation().is_development() {
            return Ok(AssetAvailability::DownloadRequired);
        }
        Ok(AssetAvailability::DevelopmentInstallRequired { path })
    }

    pub(crate) async fn download() -> Result<Self, AssetError> {
        if installation().is_development() {
            return Err(AssetError::DevelopmentDownload);
        }

        let tact_home = tact_home()?;
        let review_root = tact_home.join("review");
        fs::create_dir_all(&review_root).map_err(|source| AssetError::CreateDirectory {
            path: review_root.clone(),
            source,
        })?;
        let _lock = acquire_install_lock(&review_root).await?;
        let destination = install_path(&tact_home);

        if destination.exists() {
            match Self::from_directory(destination.clone(), InstallKind::Managed) {
                Ok(assets) => return Ok(assets),
                Err(_) => quarantine(&destination)?,
            }
        }

        let version =
            Version::parse(env!("CARGO_PKG_VERSION")).map_err(AssetError::PackageVersion)?;
        let archive_name = format!("tact-review-v{version}.tar.gz");
        let archive =
            download_verified_release_artifact(&version, &archive_name, MAX_ARCHIVE_BYTES).await?;
        let temporary = tempdir_in(&review_root).map_err(AssetError::TemporaryDirectory)?;
        extract(archive.path(), temporary.path())?;
        let extracted = temporary.path().join("review");
        let assets = Self::from_directory(extracted.clone(), InstallKind::Managed)?;
        fs::rename(&extracted, &destination).map_err(|source| AssetError::Install {
            path: destination.clone(),
            source,
        })?;
        sync_directory(&review_root)?;

        Ok(Self {
            path: destination,
            manifest: assets.manifest,
        })
    }

    fn from_directory(path: PathBuf, kind: InstallKind) -> Result<Self, AssetError> {
        let manifest = validate_directory(&path, kind)?;
        Ok(Self {
            path,
            manifest: Arc::new(manifest),
        })
    }

    pub(crate) fn resolve(&self, request_path: &str) -> Option<ResolvedReviewAsset> {
        let request_path = request_path.strip_prefix('/').unwrap_or(request_path);
        if !safe_relative_path(Path::new(request_path)) {
            return None;
        }
        let file = self.manifest.files.get(request_path)?;
        Some(ResolvedReviewAsset {
            path: self.path.join(request_path),
            content_type: file.content_type.clone(),
        })
    }

    pub(crate) fn entrypoint(&self) -> &str {
        &self.manifest.entrypoint
    }

    #[cfg(test)]
    pub(super) fn for_test(path: PathBuf) -> Self {
        let files = [
            ("index.html", "text/html; charset=utf-8"),
            ("app.js", "text/javascript; charset=utf-8"),
            ("app.css", "text/css; charset=utf-8"),
        ]
        .into_iter()
        .map(|(path, content_type)| {
            (
                path.to_owned(),
                ValidatedFile {
                    content_type: content_type.to_owned(),
                    bytes: 0,
                    sha256: Sha256::digest([]).into(),
                },
            )
        })
        .collect();
        Self {
            path,
            manifest: Arc::new(ValidatedManifest {
                entrypoint: "index.html".to_owned(),
                files,
            }),
        }
    }
}

#[derive(Clone, Copy)]
enum InstallKind {
    DevelopmentOverride,
    Managed,
}

impl InstallKind {
    fn allows_symlinks(self) -> bool {
        matches!(self, Self::DevelopmentOverride)
    }
}

struct InstallLock(File);

impl Drop for InstallLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

async fn acquire_install_lock(review_root: &Path) -> Result<InstallLock, AssetError> {
    let path = review_root.join(format!("v{}.lock", env!("CARGO_PKG_VERSION")));
    tokio::task::spawn_blocking(move || {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|source| AssetError::Lock {
                path: path.clone(),
                source,
            })?;
        file.lock_exclusive().map_err(|source| AssetError::Lock {
            path: path.clone(),
            source,
        })?;
        Ok(InstallLock(file))
    })
    .await
    .map_err(AssetError::LockTask)?
}

fn tact_home() -> Result<PathBuf, AssetError> {
    if let Some(path) = env::var_os("TACT_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|home| home.join(".tact"))
        .ok_or(AssetError::HomeUnavailable)
}

fn install_path(tact_home: &Path) -> PathBuf {
    tact_home
        .join("review")
        .join(format!("v{}", env!("CARGO_PKG_VERSION")))
}

fn quarantine(path: &Path) -> Result<(), AssetError> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let name = format!(
        "{}.corrupt.{}.{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id(),
        timestamp
    );
    let quarantine = path.with_file_name(name);
    fs::rename(path, &quarantine).map_err(|source| AssetError::Quarantine {
        path: path.to_owned(),
        quarantine,
        source,
    })
}

fn sync_directory(path: &Path) -> Result<(), AssetError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| AssetError::SyncDirectory {
            path: path.to_owned(),
            source,
        })
}

fn validate_directory(path: &Path, kind: InstallKind) -> Result<ValidatedManifest, AssetError> {
    if !kind.allows_symlinks() {
        reject_symlink(path)?;
    }
    let manifest_path = path.join(MANIFEST_NAME);
    if !kind.allows_symlinks() {
        reject_symlink(&manifest_path)?;
    }
    let manifest_bytes = read_limited(&manifest_path, MAX_MANIFEST_BYTES)?;
    let manifest: AssetManifest =
        serde_json::from_slice(&manifest_bytes).map_err(|source| AssetError::Manifest {
            path: manifest_path,
            source,
        })?;
    let manifest = validate_manifest(manifest, kind)?;

    let mut total_bytes = 0_u64;
    for (name, expected) in &manifest.files {
        let asset_path = path.join(name);
        if !kind.allows_symlinks() {
            reject_path_symlinks(path, Path::new(name))?;
        }
        let metadata = fs::metadata(&asset_path).map_err(|source| AssetError::ReadAsset {
            path: asset_path.clone(),
            source,
        })?;
        if !metadata.is_file() {
            return Err(AssetError::InvalidAssetType(asset_path));
        }
        if metadata.len() > MAX_FILE_BYTES {
            return Err(AssetError::ExpandedFileTooLarge {
                path: PathBuf::from(name),
                limit: MAX_FILE_BYTES,
            });
        }
        if metadata.len() != expected.bytes {
            return Err(AssetError::AssetSize {
                path: PathBuf::from(name),
                expected: expected.bytes,
                actual: metadata.len(),
            });
        }
        let actual_digest = hash_file(&asset_path)?;
        if actual_digest != expected.sha256 {
            return Err(AssetError::AssetDigest(PathBuf::from(name)));
        }
        total_bytes = total_bytes.saturating_add(metadata.len());
        if total_bytes > MAX_EXPANDED_BYTES {
            return Err(AssetError::ExpandedArchiveTooLarge(MAX_EXPANDED_BYTES));
        }
    }
    Ok(manifest)
}

fn validate_manifest(
    manifest: AssetManifest,
    kind: InstallKind,
) -> Result<ValidatedManifest, AssetError> {
    if manifest.schema_version != BUNDLE_SCHEMA_VERSION {
        return Err(AssetError::ManifestVersion(manifest.schema_version));
    }
    if manifest.review_api.min > REVIEW_API_VERSION || manifest.review_api.max < REVIEW_API_VERSION
    {
        return Err(AssetError::ReviewApi {
            min: manifest.review_api.min,
            max: manifest.review_api.max,
        });
    }
    let current_version = env!("CARGO_PKG_VERSION");
    let compatible_tact = manifest.tact.version == current_version
        || (kind.allows_symlinks() && manifest.tact.version == "development");
    if !compatible_tact {
        return Err(AssetError::TactVersion {
            expected: current_version,
            actual: manifest.tact.version,
        });
    }
    if !safe_relative_path(Path::new(&manifest.entrypoint)) {
        return Err(AssetError::UnsafeManifestPath(manifest.entrypoint));
    }

    let mut files = BTreeMap::new();
    for file in manifest.files {
        if !safe_relative_path(Path::new(&file.path)) || file.path == MANIFEST_NAME {
            return Err(AssetError::UnsafeManifestPath(file.path));
        }
        if !valid_content_type(&file.content_type) {
            return Err(AssetError::ContentType(file.path));
        }
        let path = file.path.clone();
        let file = ValidatedFile {
            content_type: file.content_type,
            bytes: file.bytes,
            sha256: parse_sha256(&path, &file.sha256)?,
        };
        if files.insert(path.clone(), file).is_some() {
            return Err(AssetError::DuplicateManifestPath(path));
        }
    }
    if !files.contains_key(&manifest.entrypoint) {
        return Err(AssetError::MissingEntrypoint(manifest.entrypoint));
    }
    Ok(ValidatedManifest {
        entrypoint: manifest.entrypoint,
        files,
    })
}

fn parse_sha256(path: &str, value: &str) -> Result<[u8; 32], AssetError> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(AssetError::AssetChecksum(PathBuf::from(path)));
    }
    let mut checksum = [0_u8; 32];
    for (index, pair) in value.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        let pair = std::str::from_utf8(pair)
            .map_err(|_| AssetError::AssetChecksum(PathBuf::from(path)))?;
        checksum[index] = u8::from_str_radix(pair, 16)
            .map_err(|_| AssetError::AssetChecksum(PathBuf::from(path)))?;
    }
    Ok(checksum)
}

fn hash_file(path: &Path) -> Result<[u8; 32], AssetError> {
    let mut file = File::open(path).map_err(|source| AssetError::ReadAsset {
        path: path.to_owned(),
        source,
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|source| AssetError::ReadAsset {
                path: path.to_owned(),
                source,
            })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().into())
}

fn valid_content_type(value: &str) -> bool {
    !value.is_empty() && value.is_ascii() && value.bytes().all(|byte| (b' '..=b'~').contains(&byte))
}

fn reject_symlink(path: &Path) -> Result<(), AssetError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| AssetError::ReadAsset {
        path: path.to_owned(),
        source,
    })?;
    if metadata.file_type().is_symlink() {
        return Err(AssetError::ManagedSymlink(path.to_owned()));
    }
    Ok(())
}

fn reject_path_symlinks(root: &Path, relative: &Path) -> Result<(), AssetError> {
    let mut path = root.to_owned();
    for component in relative.components() {
        path.push(component);
        reject_symlink(&path)?;
    }
    Ok(())
}

fn read_limited(path: &Path, limit: u64) -> Result<Vec<u8>, AssetError> {
    let file = File::open(path).map_err(|source| AssetError::ReadManifest {
        path: path.to_owned(),
        source,
    })?;
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| AssetError::ReadManifest {
            path: path.to_owned(),
            source,
        })?;
    if bytes.len() as u64 > limit {
        return Err(AssetError::ManifestTooLarge(limit));
    }
    Ok(bytes)
}

fn extract(archive_path: &Path, destination: &Path) -> Result<(), AssetError> {
    let input = File::open(archive_path).map_err(AssetError::Archive)?;
    let mut archive = tar::Archive::new(GzDecoder::new(input));
    let mut seen = BTreeSet::new();
    let mut expanded_bytes = 0_u64;
    for entry in archive.entries().map_err(AssetError::Archive)? {
        let mut entry = entry.map_err(AssetError::Archive)?;
        let path = entry.path().map_err(AssetError::Archive)?.into_owned();
        if !seen.insert(path.clone()) {
            return Err(AssetError::DuplicateArchivePath(path));
        }
        if path == Path::new("review") && entry.header().entry_type().is_dir() {
            continue;
        }
        if !safe_archive_path(&path) {
            return Err(AssetError::UnsafeArchivePath(path));
        }
        if entry.header().entry_type().is_dir() {
            fs::create_dir_all(destination.join(&path)).map_err(AssetError::Archive)?;
            continue;
        }
        if !entry.header().entry_type().is_file() {
            return Err(AssetError::InvalidArchiveEntry(path));
        }
        let size = entry.header().size().map_err(AssetError::Archive)?;
        let limit = if path == Path::new("review").join(MANIFEST_NAME) {
            MAX_MANIFEST_BYTES
        } else {
            MAX_FILE_BYTES
        };
        if size > limit {
            return Err(AssetError::ExpandedFileTooLarge { path, limit });
        }
        expanded_bytes = expanded_bytes.saturating_add(size);
        if expanded_bytes > MAX_EXPANDED_BYTES {
            return Err(AssetError::ExpandedArchiveTooLarge(MAX_EXPANDED_BYTES));
        }

        let output_path = destination.join(&path);
        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent).map_err(AssetError::Archive)?;
        }
        let mut output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&output_path)
            .map_err(AssetError::Archive)?;
        let written = io::copy(&mut entry, &mut output).map_err(AssetError::Archive)?;
        if written != size {
            return Err(AssetError::ArchiveSize {
                path,
                size,
                written,
            });
        }
        output.flush().map_err(AssetError::Archive)?;
    }
    Ok(())
}

fn safe_archive_path(path: &Path) -> bool {
    let mut components = path.components();
    components.next() == Some(Component::Normal("review".as_ref()))
        && components.clone().next().is_some()
        && components.all(|component| matches!(component, Component::Normal(_)))
}

fn safe_relative_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

#[derive(Deserialize, Serialize)]
struct AssetManifest {
    schema_version: u32,
    review_api: ApiCompatibility,
    tact: TactCompatibility,
    entrypoint: String,
    files: Vec<AssetFile>,
}

#[derive(Deserialize, Serialize)]
struct ApiCompatibility {
    min: u32,
    max: u32,
}

#[derive(Deserialize, Serialize)]
struct TactCompatibility {
    version: String,
}

#[derive(Deserialize, Serialize)]
struct AssetFile {
    path: String,
    content_type: String,
    bytes: u64,
    sha256: String,
}

#[derive(Debug, Eq, PartialEq)]
struct ValidatedManifest {
    entrypoint: String,
    files: BTreeMap<String, ValidatedFile>,
}

#[derive(Debug, Eq, PartialEq)]
struct ValidatedFile {
    content_type: String,
    bytes: u64,
    sha256: [u8; 32],
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum AssetError {
    #[error("could not determine the Tact directory; set TACT_HOME")]
    HomeUnavailable,
    #[error(
        "this development build cannot download release review assets; set TACT_REVIEW_ASSETS to an explicit development bundle"
    )]
    DevelopmentDownload,
    #[error("the built-in package version is invalid: {0}")]
    PackageVersion(semver::Error),
    #[error("authenticated release artifact download failed: {0}")]
    ReleaseArtifact(#[from] UpdateError),
    #[error("failed to parse review manifest {path}: {source}")]
    Manifest {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("unsupported review asset manifest version {0}")]
    ManifestVersion(u32),
    #[error(
        "review assets require API versions {min} through {max}, but this binary uses API version {REVIEW_API_VERSION}"
    )]
    ReviewApi { min: u32, max: u32 },
    #[error("review assets target Tact `{actual}`, but this binary is Tact `{expected}`")]
    TactVersion {
        expected: &'static str,
        actual: String,
    },
    #[error("review manifest contains unsafe asset path `{0}`")]
    UnsafeManifestPath(String),
    #[error("review manifest contains duplicate asset path `{0}`")]
    DuplicateManifestPath(String),
    #[error("review manifest entry `{0}` has an invalid content type")]
    ContentType(String),
    #[error("review manifest entry `{0}` has an invalid SHA-256 checksum")]
    AssetChecksum(PathBuf),
    #[error("review asset `{path}` is {actual} bytes, but its manifest declares {expected}")]
    AssetSize {
        path: PathBuf,
        expected: u64,
        actual: u64,
    },
    #[error("review asset `{0}` does not match its manifest checksum")]
    AssetDigest(PathBuf),
    #[error("review manifest entrypoint `{0}` is not a listed asset")]
    MissingEntrypoint(String),
    #[error("review asset manifest exceeds the {0}-byte limit")]
    ManifestTooLarge(u64),
    #[error("review asset file `{path}` exceeds the {limit}-byte expanded limit")]
    ExpandedFileTooLarge { path: PathBuf, limit: u64 },
    #[error("review assets exceed the {0}-byte total expanded limit")]
    ExpandedArchiveTooLarge(u64),
    #[error("managed review assets cannot contain symlink `{0}`")]
    ManagedSymlink(PathBuf),
    #[error("review asset `{0}` is not a regular file or directory")]
    InvalidAssetType(PathBuf),
    #[error("failed to read review manifest {path}: {source}")]
    ReadManifest { path: PathBuf, source: io::Error },
    #[error("failed to read review asset {path}: {source}")]
    ReadAsset { path: PathBuf, source: io::Error },
    #[error("failed to create review asset directory {path}: {source}")]
    CreateDirectory { path: PathBuf, source: io::Error },
    #[error("failed to create temporary review directory: {0}")]
    TemporaryDirectory(io::Error),
    #[error("failed to acquire review asset lock {path}: {source}")]
    Lock { path: PathBuf, source: io::Error },
    #[error("review asset lock task failed: {0}")]
    LockTask(tokio::task::JoinError),
    #[error("failed to quarantine corrupt review assets from {path} to {quarantine}: {source}")]
    Quarantine {
        path: PathBuf,
        quarantine: PathBuf,
        source: io::Error,
    },
    #[error("failed to read review archive: {0}")]
    Archive(io::Error),
    #[error("review archive contains unsafe path `{0}`")]
    UnsafeArchivePath(PathBuf),
    #[error("review archive contains duplicate path `{0}`")]
    DuplicateArchivePath(PathBuf),
    #[error("review archive contains unsupported entry `{0}`")]
    InvalidArchiveEntry(PathBuf),
    #[error("review archive entry `{path}` declared {size} bytes but contained {written}")]
    ArchiveSize {
        path: PathBuf,
        size: u64,
        written: u64,
    },
    #[error("failed to install review assets at {path}: {source}")]
    Install { path: PathBuf, source: io::Error },
    #[error("failed to sync review asset directory {path}: {source}")]
    SyncDirectory { path: PathBuf, source: io::Error },
}

#[cfg(test)]
mod tests {
    use super::{
        ApiCompatibility, AssetError, AssetFile, AssetManifest, InstallKind, ReviewAssets,
        TactCompatibility, extract, validate_directory,
    };
    use flate2::{Compression, write::GzEncoder};
    use sha2::{Digest, Sha256};
    use std::{
        ffi::OsString,
        fs,
        io::{self, Read, Write},
        path::Path,
        sync::Mutex,
    };
    use tar::{Builder, EntryType, Header};

    static ENVIRONMENT: Mutex<()> = Mutex::new(());

    struct EnvironmentGuard {
        name: &'static str,
        previous: Option<OsString>,
    }

    impl EnvironmentGuard {
        fn set(name: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let previous = std::env::var_os(name);
            unsafe { std::env::set_var(name, value) };
            Self { name, previous }
        }

        fn remove(name: &'static str) -> Self {
            let previous = std::env::var_os(name);
            unsafe { std::env::remove_var(name) };
            Self { name, previous }
        }
    }

    impl Drop for EnvironmentGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.previous {
                    Some(value) => std::env::set_var(self.name, value),
                    None => std::env::remove_var(self.name),
                }
            }
        }
    }

    fn write_valid_assets(directory: &Path) {
        let assets = [
            ("index.html", "text/html; charset=utf-8"),
            ("chunks/app.js", "text/javascript; charset=utf-8"),
            ("app.css", "text/css; charset=utf-8"),
        ];
        let files = assets
            .into_iter()
            .map(|(path, content_type)| {
                let contents = format!("contents of {path}");
                let output = directory.join(path);
                fs::create_dir_all(output.parent().unwrap()).unwrap();
                fs::write(&output, &contents).unwrap();
                AssetFile {
                    path: path.to_owned(),
                    content_type: content_type.to_owned(),
                    bytes: contents.len() as u64,
                    sha256: format!("{:x}", Sha256::digest(contents.as_bytes())),
                }
            })
            .collect();
        let manifest = AssetManifest {
            schema_version: 2,
            review_api: ApiCompatibility { min: 4, max: 4 },
            tact: TactCompatibility {
                version: env!("CARGO_PKG_VERSION").to_owned(),
            },
            entrypoint: "index.html".to_owned(),
            files,
        };
        fs::write(
            directory.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
    }

    fn archive(entries: &[(&str, EntryType, &[u8])]) -> Vec<u8> {
        let encoder = GzEncoder::new(Vec::new(), Compression::default());
        let mut builder = Builder::new(encoder);
        for (path, entry_type, contents) in entries {
            let mut header = Header::new_gnu();
            header.set_entry_type(*entry_type);
            header.set_size(contents.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, path, *contents).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    fn archive_with_raw_path(path: &str, contents: &[u8]) -> Vec<u8> {
        let mut header = Header::new_gnu();
        header.set_entry_type(EntryType::Regular);
        header.set_size(contents.len() as u64);
        header.set_mode(0o644);
        let path_bytes = path.as_bytes();
        assert!(path_bytes.len() <= 100);
        header.as_mut_bytes()[..100].fill(0);
        header.as_mut_bytes()[..path_bytes.len()].copy_from_slice(path_bytes);
        header.set_cksum();

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(header.as_bytes()).unwrap();
        encoder.write_all(contents).unwrap();
        let padding = (512 - contents.len() % 512) % 512;
        encoder.write_all(&vec![0; padding + 1024]).unwrap();
        encoder.finish().unwrap()
    }

    fn extract_bytes(bytes: &[u8]) -> Result<tempfile::TempDir, AssetError> {
        let archive = tempfile::NamedTempFile::new().unwrap();
        fs::write(archive.path(), bytes).unwrap();
        let output = tempfile::tempdir().unwrap();
        extract(archive.path(), output.path())?;
        Ok(output)
    }

    #[test]
    fn serves_only_safe_manifest_assets() {
        let directory = tempfile::tempdir().unwrap();
        write_valid_assets(directory.path());
        let assets = ReviewAssets::from_directory(
            directory.path().to_owned(),
            InstallKind::DevelopmentOverride,
        )
        .unwrap();

        let resolved = assets.resolve("/chunks/app.js").unwrap();
        assert_eq!(resolved.path, directory.path().join("chunks/app.js"));
        assert_eq!(resolved.content_type, "text/javascript; charset=utf-8");
        assert!(assets.resolve("../manifest.json").is_none());
        assert!(assets.resolve("manifest.json").is_none());

        fs::write(directory.path().join("unlisted.js"), "surprise").unwrap();
        validate_directory(directory.path(), InstallKind::DevelopmentOverride).unwrap();
        assert!(assets.resolve("unlisted.js").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn availability_accepts_the_documented_development_symlink_install() {
        let _environment = ENVIRONMENT.lock().unwrap();
        let tact_home = tempfile::tempdir().unwrap();
        let bundle = tempfile::tempdir().unwrap();
        write_valid_assets(bundle.path());
        let manifest_path = bundle.path().join("manifest.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        manifest["tact"]["version"] = "development".into();
        fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let destination = super::install_path(tact_home.path());
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(bundle.path(), &destination).unwrap();
        let _home = EnvironmentGuard::set("TACT_HOME", tact_home.path());
        let _override = EnvironmentGuard::remove(super::REVIEW_ASSETS_ENV);

        assert!(matches!(
            ReviewAssets::availability(),
            Ok(super::AssetAvailability::Ready(_))
        ));
    }

    #[test]
    fn rejects_traversal_and_incompatible_manifest_entries() {
        let directory = tempfile::tempdir().unwrap();
        write_valid_assets(directory.path());
        let manifest_path = directory.path().join("manifest.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        manifest["files"][1]["path"] = "../app.js".into();
        fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert!(matches!(
            validate_directory(directory.path(), InstallKind::DevelopmentOverride),
            Err(AssetError::UnsafeManifestPath(_))
        ));

        write_valid_assets(directory.path());
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        let duplicate = manifest["files"][0].clone();
        manifest["files"].as_array_mut().unwrap().push(duplicate);
        fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert!(matches!(
            validate_directory(directory.path(), InstallKind::DevelopmentOverride),
            Err(AssetError::DuplicateManifestPath(_))
        ));

        write_valid_assets(directory.path());
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        manifest["review_api"]["min"] = 5.into();
        fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert!(matches!(
            validate_directory(directory.path(), InstallKind::DevelopmentOverride),
            Err(AssetError::ReviewApi { .. })
        ));
    }

    #[test]
    fn rejects_asset_contents_that_do_not_match_the_manifest() {
        let directory = tempfile::tempdir().unwrap();
        write_valid_assets(directory.path());
        fs::write(directory.path().join("chunks/app.js"), "corrupted").unwrap();

        assert!(validate_directory(directory.path(), InstallKind::Managed).is_err());
    }

    #[test]
    fn rejects_archive_traversal_duplicates_and_symlinks() {
        let traversal = archive_with_raw_path("review/../escape", b"bad");
        assert!(matches!(
            extract_bytes(&traversal),
            Err(AssetError::UnsafeArchivePath(_))
        ));

        let duplicate = archive(&[
            ("review/app.js", EntryType::Regular, b"first"),
            ("review/app.js", EntryType::Regular, b"second"),
        ]);
        assert!(matches!(
            extract_bytes(&duplicate),
            Err(AssetError::DuplicateArchivePath(_))
        ));

        let symlink = archive(&[("review/app.js", EntryType::Symlink, b"")]);
        assert!(matches!(
            extract_bytes(&symlink),
            Err(AssetError::InvalidArchiveEntry(_))
        ));
    }

    #[test]
    fn enforces_expanded_file_and_total_limits_before_writing() {
        let encoder = GzEncoder::new(Vec::new(), Compression::default());
        let mut builder = Builder::new(encoder);
        let mut header = Header::new_gnu();
        header.set_entry_type(EntryType::Regular);
        header.set_size(super::MAX_FILE_BYTES + 1);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(
                &mut header,
                "review/huge.js",
                io::repeat(0).take(super::MAX_FILE_BYTES + 1),
            )
            .unwrap();
        let bytes = builder.into_inner().unwrap().finish().unwrap();
        assert!(matches!(
            extract_bytes(&bytes),
            Err(AssetError::ExpandedFileTooLarge { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn allows_override_symlink_but_rejects_managed_symlink() {
        use std::os::unix::fs::symlink;

        let assets = tempfile::tempdir().unwrap();
        write_valid_assets(assets.path());
        let root = tempfile::tempdir().unwrap();
        let link = root.path().join("assets");
        symlink(assets.path(), &link).unwrap();

        validate_directory(&link, InstallKind::DevelopmentOverride).unwrap();
        assert!(matches!(
            validate_directory(&link, InstallKind::Managed),
            Err(AssetError::ManagedSymlink(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn managed_install_rejects_symlinked_asset_ancestors() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        write_valid_assets(directory.path());
        let chunks = directory.path().join("chunks");
        let external = tempfile::tempdir().unwrap();
        fs::rename(chunks.join("app.js"), external.path().join("app.js")).unwrap();
        fs::remove_dir(&chunks).unwrap();
        symlink(external.path(), &chunks).unwrap();

        validate_directory(directory.path(), InstallKind::DevelopmentOverride).unwrap();
        assert!(matches!(
            validate_directory(directory.path(), InstallKind::Managed),
            Err(AssetError::ManagedSymlink(path)) if path == chunks
        ));
    }
}
