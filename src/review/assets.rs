use flate2::read::GzDecoder;
use futures_util::StreamExt;
use reqwest::Client;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    env, fs,
    io::{self, Cursor},
    path::{Component, Path, PathBuf},
    time::Duration,
};
use tar::Archive;
use tempfile::tempdir_in;

const REVIEW_ASSETS_ENV: &str = "TACT_REVIEW_ASSETS";
const MAX_ARCHIVE_BYTES: usize = 32 * 1024 * 1024;
const MAX_SIDECAR_BYTES: usize = 64 * 1024;
const CONTENT_FILES: [&str; 5] = [
    "index.html",
    "app.js",
    "app.css",
    "LICENSE.md",
    "THIRD_PARTY_NOTICES.md",
];
const REQUIRED_FILES: [&str; 6] = [
    "index.html",
    "app.js",
    "app.css",
    "LICENSE.md",
    "THIRD_PARTY_NOTICES.md",
    "manifest.json",
];

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum AssetAvailability {
    Ready(ReviewAssets),
    DownloadRequired,
    DevelopmentInstallRequired { path: PathBuf },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReviewAssets {
    path: PathBuf,
}

impl ReviewAssets {
    pub(crate) fn availability() -> Result<AssetAvailability, AssetError> {
        if let Some(path) = env::var_os(REVIEW_ASSETS_ENV).map(PathBuf::from) {
            validate_directory(&path)?;
            return Ok(AssetAvailability::Ready(Self { path }));
        }

        let path = install_path(&tact_home()?);
        if path.is_dir() {
            validate_directory(&path)?;
            return Ok(AssetAvailability::Ready(Self { path }));
        }
        if official_build() {
            return Ok(AssetAvailability::DownloadRequired);
        }
        Ok(AssetAvailability::DevelopmentInstallRequired { path })
    }

    pub(crate) async fn download() -> Result<Self, AssetError> {
        if !official_build() {
            return Err(AssetError::DevelopmentDownload);
        }

        let tact_home = tact_home()?;
        let review_root = tact_home.join("review");
        fs::create_dir_all(&review_root).map_err(|source| AssetError::CreateDirectory {
            path: review_root.clone(),
            source,
        })?;
        let archive_name = format!("tact-review-v{}.tar.gz", env!("CARGO_PKG_VERSION"));
        let base = format!(
            "https://github.com/clabby/tact/releases/download/v{}",
            env!("CARGO_PKG_VERSION")
        );
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(30))
            .user_agent(concat!("tact/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(AssetError::Client)?;
        let archive = download(
            &client,
            &format!("{base}/{archive_name}"),
            MAX_ARCHIVE_BYTES,
        )
        .await?;
        let checksum = download(
            &client,
            &format!("{base}/{archive_name}.sha256"),
            MAX_SIDECAR_BYTES,
        )
        .await?;
        verify_checksum(&archive, &checksum, &archive_name)?;

        let temporary = tempdir_in(&review_root).map_err(AssetError::TemporaryDirectory)?;
        extract(&archive, temporary.path())?;
        let extracted = temporary.path().join("review");
        validate_directory(&extracted)?;
        let destination = install_path(&tact_home);
        fs::rename(&extracted, &destination).map_err(|source| AssetError::Install {
            path: destination.clone(),
            source,
        })?;
        Ok(Self { path: destination })
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }
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

fn official_build() -> bool {
    env!("TACT_RELEASE_BUILD") == "true"
}

fn validate_directory(path: &Path) -> Result<(), AssetError> {
    for name in REQUIRED_FILES {
        let file = path.join(name);
        if !file.is_file() {
            return Err(AssetError::MissingFile(file));
        }
    }

    let manifest_path = path.join("manifest.json");
    let manifest = fs::read(&manifest_path).map_err(|source| AssetError::ReadManifest {
        path: manifest_path.clone(),
        source,
    })?;
    let manifest: AssetManifest =
        serde_json::from_slice(&manifest).map_err(|source| AssetError::Manifest {
            path: manifest_path,
            source,
        })?;
    if manifest.version != 1 {
        return Err(AssetError::ManifestVersion(manifest.version));
    }
    for expected in CONTENT_FILES {
        let Some(file) = manifest.files.iter().find(|file| file.name == expected) else {
            return Err(AssetError::MissingManifestEntry(expected.to_owned()));
        };
        let contents = fs::read(path.join(expected)).map_err(|source| AssetError::ReadAsset {
            path: path.join(expected),
            source,
        })?;
        if contents.len() != file.bytes || hex_digest(&contents) != file.sha256 {
            return Err(AssetError::AssetChecksum(expected.to_owned()));
        }
    }
    Ok(())
}

async fn download(client: &Client, url: &str, limit: usize) -> Result<Vec<u8>, AssetError> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(AssetError::Download)?
        .error_for_status()
        .map_err(AssetError::Download)?;
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(AssetError::DownloadTooLarge(limit));
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(AssetError::Download)?;
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(AssetError::DownloadTooLarge(limit));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn verify_checksum(archive: &[u8], sidecar: &[u8], name: &str) -> Result<(), AssetError> {
    let sidecar = std::str::from_utf8(sidecar).map_err(AssetError::ChecksumEncoding)?;
    let mut fields = sidecar.split_whitespace();
    let expected = fields.next().ok_or(AssetError::ChecksumFile)?;
    let listed_name = fields.next().ok_or(AssetError::ChecksumFile)?;
    if fields.next().is_some() || listed_name.trim_start_matches('*') != name {
        return Err(AssetError::ChecksumFile);
    }
    if expected != hex_digest(archive) {
        return Err(AssetError::ArchiveChecksum);
    }
    Ok(())
}

fn extract(bytes: &[u8], destination: &Path) -> Result<(), AssetError> {
    let mut archive = Archive::new(GzDecoder::new(Cursor::new(bytes)));
    let mut seen = Vec::new();
    for entry in archive.entries().map_err(AssetError::Archive)? {
        let mut entry = entry.map_err(AssetError::Archive)?;
        let path = entry.path().map_err(AssetError::Archive)?.into_owned();
        if path == Path::new("review") && entry.header().entry_type().is_dir() {
            continue;
        }
        let mut components = path.components();
        if components.next() != Some(Component::Normal("review".as_ref())) {
            return Err(AssetError::UnsafeArchivePath(path));
        }
        let Some(Component::Normal(name)) = components.next() else {
            return Err(AssetError::UnsafeArchivePath(path));
        };
        if components.next().is_some()
            || !REQUIRED_FILES.iter().any(|expected| name == *expected)
            || !entry.header().entry_type().is_file()
        {
            return Err(AssetError::UnsafeArchivePath(path));
        }
        if seen.contains(&path) {
            return Err(AssetError::DuplicateArchivePath(path));
        }
        seen.push(path.clone());
        entry.unpack_in(destination).map_err(AssetError::Archive)?;
    }
    Ok(())
}

fn hex_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Deserialize)]
struct AssetManifest {
    version: u32,
    files: Vec<AssetFile>,
}

#[derive(Deserialize)]
struct AssetFile {
    name: String,
    bytes: usize,
    sha256: String,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum AssetError {
    #[error("could not determine the Tact directory; set TACT_HOME")]
    HomeUnavailable,
    #[error("development builds do not download review assets")]
    DevelopmentDownload,
    #[error("review asset file is missing: {0}")]
    MissingFile(PathBuf),
    #[error("review asset manifest is missing `{0}`")]
    MissingManifestEntry(String),
    #[error("unsupported review asset manifest version {0}")]
    ManifestVersion(u32),
    #[error("review asset `{0}` does not match its manifest checksum")]
    AssetChecksum(String),
    #[error("failed to read review manifest {path}: {source}")]
    ReadManifest { path: PathBuf, source: io::Error },
    #[error("failed to parse review manifest {path}: {source}")]
    Manifest {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("failed to read review asset {path}: {source}")]
    ReadAsset { path: PathBuf, source: io::Error },
    #[error("failed to create review asset directory {path}: {source}")]
    CreateDirectory { path: PathBuf, source: io::Error },
    #[error("failed to create temporary review directory: {0}")]
    TemporaryDirectory(io::Error),
    #[error("failed to create review HTTP client: {0}")]
    Client(reqwest::Error),
    #[error("failed to download review assets: {0}")]
    Download(reqwest::Error),
    #[error("review asset download exceeds the {0}-byte limit")]
    DownloadTooLarge(usize),
    #[error("review checksum is not valid UTF-8: {0}")]
    ChecksumEncoding(std::str::Utf8Error),
    #[error("review checksum file is malformed")]
    ChecksumFile,
    #[error("review archive does not match its checksum")]
    ArchiveChecksum,
    #[error("failed to read review archive: {0}")]
    Archive(io::Error),
    #[error("review archive contains unsafe path {0}")]
    UnsafeArchivePath(PathBuf),
    #[error("review archive contains duplicate path {0}")]
    DuplicateArchivePath(PathBuf),
    #[error("failed to install review assets at {path}: {source}")]
    Install { path: PathBuf, source: io::Error },
}

#[cfg(test)]
mod tests {
    use super::{
        AssetError, CONTENT_FILES, extract, hex_digest, validate_directory, verify_checksum,
    };
    use flate2::{Compression, write::GzEncoder};
    use std::{fs, path::Path};
    use tar::Builder;

    fn write_valid_assets(directory: &Path) {
        let files = CONTENT_FILES.map(|name| {
            let contents = format!("contents of {name}");
            fs::write(directory.join(name), &contents).unwrap();
            serde_json::json!({
                "name": name,
                "bytes": contents.len(),
                "sha256": hex_digest(contents.as_bytes()),
            })
        });
        fs::write(
            directory.join("manifest.json"),
            serde_json::to_vec(&serde_json::json!({ "version": 1, "files": files })).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn validates_every_distributed_asset_against_the_manifest() {
        let directory = tempfile::tempdir().unwrap();
        write_valid_assets(directory.path());

        validate_directory(directory.path()).unwrap();

        fs::write(directory.path().join("app.js"), "changed").unwrap();
        assert!(matches!(
            validate_directory(directory.path()),
            Err(AssetError::AssetChecksum(name)) if name == "app.js"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn validates_assets_through_a_directory_symlink() {
        use std::os::unix::fs::symlink;

        let assets = tempfile::tempdir().unwrap();
        write_valid_assets(assets.path());

        let install_root = tempfile::tempdir().unwrap();
        let installed = install_root.path().join("v-test");
        symlink(assets.path(), &installed).unwrap();

        validate_directory(&installed).unwrap();
    }

    #[test]
    fn extracts_the_directory_entry_created_by_release_packaging() {
        let source = tempfile::tempdir().unwrap();
        write_valid_assets(source.path());
        let encoder = GzEncoder::new(Vec::new(), Compression::default());
        let mut archive = Builder::new(encoder);
        archive.append_dir("review", source.path()).unwrap();
        for name in super::REQUIRED_FILES {
            archive
                .append_path_with_name(source.path().join(name), Path::new("review").join(name))
                .unwrap();
        }
        let encoder = archive.into_inner().unwrap();
        let bytes = encoder.finish().unwrap();

        let destination = tempfile::tempdir().unwrap();
        extract(&bytes, destination.path()).unwrap();
        validate_directory(&destination.path().join("review")).unwrap();
    }

    #[test]
    fn checksum_sidecar_must_name_the_downloaded_archive() {
        let archive = b"archive";
        let checksum = format!("{}  other.tar.gz\n", hex_digest(archive));

        assert!(matches!(
            verify_checksum(archive, checksum.as_bytes(), "review.tar.gz"),
            Err(AssetError::ChecksumFile)
        ));
    }
}
