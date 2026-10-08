//! Release discovery and verified replacement of the running executable.

use crate::app::installation::{InstallationKind, current as installation};
use flate2::read::GzDecoder;
use minisign_verify::{PublicKey, Signature};
use reqwest::Client;
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    env, fmt,
    io::{self, Cursor, Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    time::Duration,
};
use tempfile::{NamedTempFile, TempDir, tempdir};
use thiserror::Error;

const GITHUB_API: &str = "https://api.github.com/repos/clabby/tact";
const GITHUB_DOWNLOADS: &str = "https://github.com/clabby/tact/releases/download";
const CRATES_IO_API: &str = "https://crates.io/api/v1/crates/tact";

/// Pre-release tags and asset names carry this many hexadecimal digits of the commit hash.
const REVISION_DIGITS: usize = 12;
/// The shortest commit abbreviation `tact update` accepts, matching Git's own minimum.
const MIN_REVISION_DIGITS: usize = 7;
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_METADATA_BYTES: u64 = 1024 * 1024;
const MAX_CRATE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_ARCHIVE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_SIDECAR_BYTES: u64 = 64 * 1024;

#[derive(Debug, Error)]
pub(crate) enum UpdateError {
    #[error("this build target is not supported by tact releases: {target}")]
    UnsupportedTarget { target: String },
    #[error("the built-in tact version is invalid: {0}")]
    CurrentVersion(#[source] semver::Error),
    #[error("GitHub returned an invalid release version `{version}`: {source}")]
    ReleaseVersion {
        version: String,
        #[source]
        source: semver::Error,
    },
    #[error("GitHub returned the unrecognized release tag `{tag}`")]
    ReleaseTagFormat { tag: String },
    #[error(
        "`{revision}` is not a commit: give between 7 and 40 hexadecimal digits of a commit on main"
    )]
    InvalidRevision { revision: String },
    #[error("clabby/tact has no commit `{revision}`")]
    RevisionNotFound { revision: String },
    #[error("GitHub returned invalid commit metadata for `{revision}`: {source}")]
    CommitMetadata {
        revision: String,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "there is no pre-release build of commit {revision}: only a bounded number of the most \
         recent merges to main keep one, and a merge's build appears when its release workflow \
         finishes"
    )]
    PreReleaseNotFound { revision: String },
    #[error(
        "pre-release builds cannot replace a tact managed by Cargo or a package manager; \
         install tact from a release archive or build it from source instead"
    )]
    PreReleaseManaged,
    #[error("failed to create the update HTTP client: {0}")]
    Client(#[source] reqwest::Error),
    #[error("failed to {operation}: {source}")]
    Http {
        operation: &'static str,
        #[source]
        source: reqwest::Error,
    },
    #[error("GitHub returned invalid release metadata: {0}")]
    GithubMetadata(#[source] serde_json::Error),
    #[error("{name} exceeds the {limit}-byte download limit")]
    DownloadTooLarge { name: String, limit: u64 },
    #[error("release {tag} is missing `{name}`")]
    MissingAsset { tag: ReleaseTag, name: String },
    #[error("release {tag} contains more than one `{name}` asset")]
    DuplicateAsset { tag: ReleaseTag, name: String },
    #[error("crates.io returned invalid metadata for tact v{version}: {source}")]
    RegistryMetadata {
        version: Version,
        #[source]
        source: serde_json::Error,
    },
    #[error("crates.io returned an invalid SHA-256 checksum for tact v{version}")]
    RegistryChecksumFormat { version: Version },
    #[error("the tact v{version} crate does not match the checksum reported by crates.io")]
    RegistryChecksumMismatch { version: Version },
    #[error("could not read the tact v{version} crate archive: {source}")]
    CrateArchive {
        version: Version,
        #[source]
        source: io::Error,
    },
    #[error("the tact v{version} crate package is missing Cargo.toml")]
    MissingManifest { version: Version },
    #[error("the tact v{version} crate package contains duplicate Cargo.toml entries")]
    DuplicateManifest { version: Version },
    #[error("the tact v{version} crate package has an unsafe archive path `{path}`")]
    UnsafeCratePath { version: Version, path: PathBuf },
    #[error("could not parse signing metadata for tact v{version}: {source}")]
    SigningMetadata {
        version: Version,
        #[source]
        source: Box<toml::de::Error>,
    },
    #[error("tact v{version} does not contain cargo-binstall signing metadata")]
    MissingSigningMetadata { version: Version },
    #[error("tact v{version} uses unsupported signing algorithm `{algorithm}`")]
    UnsupportedSigningAlgorithm { version: Version, algorithm: String },
    #[error("tact v{version} contains an invalid minisign public key: {source}")]
    PublicKey {
        version: Version,
        #[source]
        source: minisign_verify::Error,
    },
    #[error("release checksum file `{name}` is malformed")]
    ChecksumFile { name: String },
    #[error("downloaded release archive does not match `{name}`")]
    ArchiveChecksumMismatch { name: String },
    #[error("release signature `{name}` is not valid UTF-8")]
    SignatureEncoding { name: String },
    #[error("release signature `{name}` is malformed: {source}")]
    Signature {
        name: String,
        #[source]
        source: minisign_verify::Error,
    },
    #[error("release signature verification failed for `{name}`: {source}")]
    SignatureVerification {
        name: String,
        #[source]
        source: minisign_verify::Error,
    },
    #[error("failed to create temporary update storage: {0}")]
    TemporaryStorage(#[source] io::Error),
    #[error("failed to write downloaded update data: {0}")]
    TemporaryWrite(#[source] io::Error),
    #[error("could not read release archive `{name}`: {source}")]
    ReleaseArchive {
        name: String,
        #[source]
        source: io::Error,
    },
    #[error("release archive `{name}` contains an unsafe or unexpected path `{path}`")]
    UnexpectedArchivePath { name: String, path: PathBuf },
    #[error("release archive `{name}` contains duplicate tact binaries")]
    DuplicateBinary { name: String },
    #[error("release archive `{name}` does not contain the expected tact binary")]
    MissingBinary { name: String },
    #[error("release archive `{name}` contains a non-file tact entry")]
    InvalidBinaryEntry { name: String },
    #[error("failed to replace the running tact executable: {0}")]
    Replace(#[source] io::Error),
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum UpdateStatus {
    UpToDate { current: ReleaseTag },
    Updated { from: ReleaseTag, to: ReleaseTag },
    UseCargo { command: String },
    UsePackageManager { manager: String },
}

/// The identity of a published build: what its GitHub Release is tagged and how its assets are
/// named.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ReleaseTag {
    /// An official release, tagged `v<version>`.
    Official(Version),
    /// The pre-release build of one commit on `main`, tagged `dev-<revision>`. `revision` is the
    /// first [`REVISION_DIGITS`] lowercase hexadecimal digits of the commit hash.
    PreRelease(String),
}

impl ReleaseTag {
    pub(crate) fn parse(tag: &str) -> Result<Self, UpdateError> {
        if let Some(revision) = tag.strip_prefix("dev-") {
            return match Self::pre_release(revision) {
                Some(tag) if revision.len() == REVISION_DIGITS => Ok(tag),
                _ => Err(UpdateError::ReleaseTagFormat {
                    tag: tag.to_owned(),
                }),
            };
        }
        let version = tag.strip_prefix('v').unwrap_or(tag);
        Version::parse(version)
            .map(Self::Official)
            .map_err(|source| UpdateError::ReleaseVersion {
                version: tag.to_owned(),
                source,
            })
    }

    /// The pre-release of the commit with hash `commit`, which may be abbreviated to at least
    /// [`REVISION_DIGITS`] digits.
    fn pre_release(commit: &str) -> Option<Self> {
        let revision = commit.get(..REVISION_DIGITS)?;
        revision
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
            .then(|| Self::PreRelease(revision.to_owned()))
    }

    /// The tag of the release that published the running build.
    pub(crate) fn running() -> Result<Self, UpdateError> {
        Self::parse(&installation().release_tag())
    }

    pub(crate) fn name(&self) -> String {
        match self {
            Self::Official(version) => format!("v{version}"),
            Self::PreRelease(revision) => format!("dev-{revision}"),
        }
    }
}

impl fmt::Display for ReleaseTag {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Official(version) => write!(formatter, "v{version}"),
            Self::PreRelease(revision) => write!(formatter, "pre-release {revision}"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SupportedTarget {
    LinuxX86_64,
    LinuxAarch64,
    MacosX86_64,
    MacosAarch64,
}

impl SupportedTarget {
    fn current() -> Result<Self, UpdateError> {
        Self::from_triple(env!("TACT_BUILD_TARGET"))
    }

    fn from_triple(target: &str) -> Result<Self, UpdateError> {
        match target {
            "x86_64-unknown-linux-gnu" => Ok(Self::LinuxX86_64),
            "aarch64-unknown-linux-gnu" => Ok(Self::LinuxAarch64),
            "x86_64-apple-darwin" => Ok(Self::MacosX86_64),
            "aarch64-apple-darwin" => Ok(Self::MacosAarch64),
            _ => Err(UpdateError::UnsupportedTarget {
                target: target.to_owned(),
            }),
        }
    }

    const fn triple(self) -> &'static str {
        match self {
            Self::LinuxX86_64 => "x86_64-unknown-linux-gnu",
            Self::LinuxAarch64 => "aarch64-unknown-linux-gnu",
            Self::MacosX86_64 => "x86_64-apple-darwin",
            Self::MacosAarch64 => "aarch64-apple-darwin",
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Debug, Deserialize)]
struct GithubReleaseResponse {
    tag_name: String,
    assets: Vec<GithubAsset>,
}

#[derive(Debug)]
struct Release {
    tag: ReleaseTag,
    assets: Vec<GithubAsset>,
}

impl Release {
    fn parse(response: GithubReleaseResponse) -> Result<Self, UpdateError> {
        Ok(Self {
            tag: ReleaseTag::parse(&response.tag_name)?,
            assets: response.assets,
        })
    }

    fn asset(&self, name: &str) -> Result<&GithubAsset, UpdateError> {
        let mut matches = self.assets.iter().filter(|asset| asset.name == name);
        let asset = matches.next().ok_or_else(|| UpdateError::MissingAsset {
            tag: self.tag.clone(),
            name: name.to_owned(),
        })?;
        if matches.next().is_some() {
            return Err(UpdateError::DuplicateAsset {
                tag: self.tag.clone(),
                name: name.to_owned(),
            });
        }
        Ok(asset)
    }

    /// The archive for `target` with its checksum and, for an official release, its signature.
    fn assets_for(&self, target: SupportedTarget) -> Result<ReleaseAssets<'_>, UpdateError> {
        let archive_name = format!("tact-{}-{}.tar.gz", target.triple(), self.tag.name());
        let checksum_name = format!("{archive_name}.sha256");
        Ok(ReleaseAssets {
            archive: self.asset(&archive_name)?,
            checksum: self.asset(&checksum_name)?,
            signature: match self.tag {
                ReleaseTag::Official(_) => Some(self.asset(&format!("{archive_name}.sig"))?),
                ReleaseTag::PreRelease(_) => None,
            },
        })
    }
}

#[derive(Clone, Copy)]
struct ReleaseAssets<'a> {
    archive: &'a GithubAsset,
    checksum: &'a GithubAsset,
    signature: Option<&'a GithubAsset>,
}

#[derive(Deserialize)]
struct CommitResponse {
    sha: String,
}

#[derive(Deserialize)]
struct RegistryResponse {
    version: RegistryVersion,
}

#[derive(Deserialize)]
struct RegistryVersion {
    checksum: String,
}

#[derive(Deserialize)]
struct CrateManifest {
    package: ManifestPackage,
}

#[derive(Deserialize)]
struct ManifestPackage {
    #[serde(default)]
    metadata: ManifestMetadata,
}

#[derive(Default, Deserialize)]
struct ManifestMetadata {
    binstall: Option<BinstallMetadata>,
}

#[derive(Deserialize)]
struct BinstallMetadata {
    signing: Option<SigningMetadata>,
}

#[derive(Deserialize)]
struct SigningMetadata {
    algorithm: String,
    pubkey: String,
}

/// Downloads an archive of release `tag` and verifies it as [`Updater::download_verified_artifact`]
/// does.
pub(crate) async fn download_verified_release_artifact(
    tag: &ReleaseTag,
    archive_name: &str,
    max_archive_bytes: u64,
) -> Result<NamedTempFile, UpdateError> {
    let base = format!("{GITHUB_DOWNLOADS}/{}", tag.name());
    let asset = |name: String| GithubAsset {
        browser_download_url: format!("{base}/{name}"),
        name,
    };
    let assets = OwnedReleaseAssets {
        archive: asset(archive_name.to_owned()),
        checksum: asset(format!("{archive_name}.sha256")),
        signature: match tag {
            ReleaseTag::Official(_) => Some(asset(format!("{archive_name}.sig"))),
            ReleaseTag::PreRelease(_) => None,
        },
    };
    Updater::new()?
        .download_verified_artifact(tag, assets.as_borrowed(), max_archive_bytes)
        .await
}

/// The newer official release this installation can update to, if any. An archive installation,
/// official or pre-release, only reports a release whose archive assets and signing key are
/// available. A pre-release counts as its Cargo version, so only a later version notifies it.
pub(crate) async fn check_for_update() -> Result<Option<Version>, UpdateError> {
    let installation = installation();
    if installation.is_development() {
        return Ok(None);
    }
    let build_target = env!("TACT_BUILD_TARGET");
    let artifact_target = update_artifact_target(installation, build_target)?;
    let updater = Updater::new()?;
    let (version, release) = updater.latest_release().await?;
    if version <= updater.current {
        return Ok(None);
    }
    if let Some(target) = artifact_target {
        release.assets_for(target)?;
        updater.signing_key(&version).await?;
    }
    Ok(Some(version))
}

fn update_artifact_target(
    installation: &InstallationKind,
    target: &str,
) -> Result<Option<SupportedTarget>, UpdateError> {
    match installation {
        InstallationKind::ReleaseArchive | InstallationKind::PreRelease { .. } => {
            SupportedTarget::from_triple(target).map(Some)
        }
        InstallationKind::CratesIo { .. }
        | InstallationKind::External { .. }
        | InstallationKind::Development => Ok(None),
    }
}

/// Replaces the running executable with the latest verified official release, or reports how this
/// installation must be updated instead. A pre-release build is replaced even by the release of
/// its own version, since that returns it to the release channel.
pub(crate) async fn install_latest() -> Result<UpdateStatus, UpdateError> {
    if let InstallationKind::CratesIo { root } = installation() {
        return Ok(UpdateStatus::UseCargo {
            command: cargo_update_command(root, default_cargo_install_root().as_deref()),
        });
    }
    if let InstallationKind::External { manager } = installation() {
        return Ok(UpdateStatus::UsePackageManager {
            manager: manager.clone(),
        });
    }
    let target = SupportedTarget::current()?;
    let updater = Updater::new()?;
    let (version, release) = updater.latest_release().await?;
    let leaving_pre_release = matches!(installation(), InstallationKind::PreRelease { .. });
    let newer = if leaving_pre_release {
        version >= updater.current
    } else {
        version > updater.current
    };
    if !newer {
        return Ok(UpdateStatus::UpToDate {
            current: ReleaseTag::running()?,
        });
    }
    updater.replace_executable(target, release).await
}

/// Replaces the running executable with the pre-release build of the commit that `revision`
/// abbreviates. The build is checked against its published checksum only: pre-releases have no
/// crates.io package to hold an independent signing key, so they are exactly as trustworthy as the
/// clabby/tact GitHub Releases they come from.
pub(crate) async fn install_pre_release(revision: &str) -> Result<UpdateStatus, UpdateError> {
    let revision = normalize_revision(revision)?;
    ensure_pre_release_installable(installation())?;
    let target = SupportedTarget::current()?;
    let updater = Updater::new()?;
    let commit = updater.commit(&revision).await?;
    let tag = ReleaseTag::pre_release(&commit).ok_or(UpdateError::InvalidRevision {
        revision: commit.clone(),
    })?;
    let running = ReleaseTag::running()?;
    if tag == running {
        return Ok(UpdateStatus::UpToDate { current: running });
    }
    let release = updater.pre_release(&tag).await?;
    updater.replace_executable(target, release).await
}

/// Whether a pre-release may replace this installation. Cargo and package managers own their
/// binaries and keep records that a replacement would leave stale; every other build, including
/// one built from source, is replaced the same way `tact update` replaces it with a release.
fn ensure_pre_release_installable(installation: &InstallationKind) -> Result<(), UpdateError> {
    match installation {
        InstallationKind::CratesIo { .. } | InstallationKind::External { .. } => {
            Err(UpdateError::PreReleaseManaged)
        }
        InstallationKind::ReleaseArchive
        | InstallationKind::PreRelease { .. }
        | InstallationKind::Development => Ok(()),
    }
}

/// A commit abbreviation as the GitHub commits API takes it: lowercase hexadecimal.
fn normalize_revision(revision: &str) -> Result<String, UpdateError> {
    let revision = revision.trim().to_ascii_lowercase();
    let valid = (MIN_REVISION_DIGITS..=40).contains(&revision.len())
        && revision.bytes().all(|byte| byte.is_ascii_hexdigit());
    if valid {
        Ok(revision)
    } else {
        Err(UpdateError::InvalidRevision { revision })
    }
}

struct OwnedReleaseAssets {
    archive: GithubAsset,
    checksum: GithubAsset,
    signature: Option<GithubAsset>,
}

impl OwnedReleaseAssets {
    fn as_borrowed(&self) -> ReleaseAssets<'_> {
        ReleaseAssets {
            archive: &self.archive,
            checksum: &self.checksum,
            signature: self.signature.as_ref(),
        }
    }
}

/// One update operation's HTTP client and the version of the running build. Every download is
/// bounded by a byte limit enforced both on the advertised length and while streaming.
struct Updater {
    client: Client,
    current: Version,
}

/// What a download fetches, which names the operation in HTTP errors.
#[derive(Clone, Copy)]
enum Download {
    Metadata,
    Archive,
}

impl Download {
    const fn request_operation(self) -> &'static str {
        match self {
            Self::Metadata => "download update data",
            Self::Archive => "download the release archive",
        }
    }

    const fn read_operation(self) -> &'static str {
        match self {
            Self::Metadata => "read update data",
            Self::Archive => "read the release archive",
        }
    }
}

impl Updater {
    fn new() -> Result<Self, UpdateError> {
        let current =
            Version::parse(env!("CARGO_PKG_VERSION")).map_err(UpdateError::CurrentVersion)?;
        let client = Client::builder()
            .user_agent(concat!("tact/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(HTTP_TIMEOUT)
            .build()
            .map_err(UpdateError::Client)?;
        Ok(Self { client, current })
    }

    /// The latest official release and its version. GitHub's latest release never includes a
    /// pre-release.
    async fn latest_release(&self) -> Result<(Version, Release), UpdateError> {
        let bytes = self
            .fetch_bytes(
                &format!("{GITHUB_API}/releases/latest"),
                "GitHub release metadata",
                MAX_METADATA_BYTES,
            )
            .await?;
        let response = serde_json::from_slice(&bytes).map_err(UpdateError::GithubMetadata)?;
        let release = Release::parse(response)?;
        match &release.tag {
            ReleaseTag::Official(version) => Ok((version.clone(), release)),
            ReleaseTag::PreRelease(_) => Err(UpdateError::ReleaseTagFormat {
                tag: release.tag.name(),
            }),
        }
    }

    /// The full hash of the commit that the abbreviation `revision` names.
    async fn commit(&self, revision: &str) -> Result<String, UpdateError> {
        let bytes = self
            .fetch_bytes(
                &format!("{GITHUB_API}/commits/{revision}"),
                "GitHub commit metadata",
                MAX_METADATA_BYTES,
            )
            .await
            .map_err(|error| match error {
                error if is_not_found(&error) => UpdateError::RevisionNotFound {
                    revision: revision.to_owned(),
                },
                error => error,
            })?;
        let response: CommitResponse =
            serde_json::from_slice(&bytes).map_err(|source| UpdateError::CommitMetadata {
                revision: revision.to_owned(),
                source,
            })?;
        let commit = response.sha;
        if commit.len() == 40 && commit.starts_with(revision) {
            Ok(commit)
        } else {
            Err(UpdateError::RevisionNotFound {
                revision: revision.to_owned(),
            })
        }
    }

    async fn pre_release(&self, tag: &ReleaseTag) -> Result<Release, UpdateError> {
        let bytes = self
            .fetch_bytes(
                &format!("{GITHUB_API}/releases/tags/{}", tag.name()),
                "GitHub release metadata",
                MAX_METADATA_BYTES,
            )
            .await
            .map_err(|error| match (&error, tag) {
                (_, ReleaseTag::PreRelease(revision)) if is_not_found(&error) => {
                    UpdateError::PreReleaseNotFound {
                        revision: revision.clone(),
                    }
                }
                _ => error,
            })?;
        let response = serde_json::from_slice(&bytes).map_err(UpdateError::GithubMetadata)?;
        let release = Release::parse(response)?;
        if release.tag == *tag {
            Ok(release)
        } else {
            Err(UpdateError::ReleaseTagFormat {
                tag: release.tag.name(),
            })
        }
    }

    /// Downloads, verifies, and installs `release` over the running executable.
    async fn replace_executable(
        &self,
        target: SupportedTarget,
        release: Release,
    ) -> Result<UpdateStatus, UpdateError> {
        let assets = release.assets_for(target)?;
        let archive = self
            .download_verified_artifact(&release.tag, assets, MAX_ARCHIVE_BYTES)
            .await?;
        let extracted = extract_binary(&archive, &assets.archive.name, target, &release.tag)?;
        self_replace::self_replace(&extracted.path).map_err(UpdateError::Replace)?;
        Ok(UpdateStatus::Updated {
            from: ReleaseTag::running()?,
            to: release.tag,
        })
    }

    /// Downloads the archive and its sidecars, then verifies the archive against each. An
    /// official release is also checked against the signing key in its crates.io package; a
    /// pre-release has no such package and only its checksum is verified.
    async fn download_verified_artifact(
        &self,
        tag: &ReleaseTag,
        assets: ReleaseAssets<'_>,
        max_archive_bytes: u64,
    ) -> Result<NamedTempFile, UpdateError> {
        let public_key = match tag {
            ReleaseTag::Official(version) => Some(self.signing_key(version).await?),
            ReleaseTag::PreRelease(_) => None,
        };
        let archive = NamedTempFile::new().map_err(UpdateError::TemporaryStorage)?;
        let mut output = archive.reopen().map_err(UpdateError::TemporaryWrite)?;
        self.download(
            Download::Archive,
            &assets.archive.browser_download_url,
            &assets.archive.name,
            max_archive_bytes,
            |chunk| output.write_all(chunk).map_err(UpdateError::TemporaryWrite),
        )
        .await?;
        output.flush().map_err(UpdateError::TemporaryWrite)?;
        let checksum = self
            .fetch_bytes(
                &assets.checksum.browser_download_url,
                &assets.checksum.name,
                MAX_SIDECAR_BYTES,
            )
            .await?;

        verify_archive_checksum(
            &archive,
            &assets.archive.name,
            &checksum,
            &assets.checksum.name,
        )?;
        if let (Some(public_key), Some(signature)) = (public_key, assets.signature) {
            let bytes = self
                .fetch_bytes(
                    &signature.browser_download_url,
                    &signature.name,
                    MAX_SIDECAR_BYTES,
                )
                .await?;
            verify_archive_signature(
                &archive,
                &assets.archive.name,
                &bytes,
                &signature.name,
                &public_key,
            )?;
        }
        Ok(archive)
    }

    /// The minisign key that signs `version`'s release archives. It is read from the
    /// cargo-binstall metadata of the crates.io package, whose bytes must match the checksum the
    /// registry reports.
    async fn signing_key(&self, version: &Version) -> Result<PublicKey, UpdateError> {
        let metadata_url = format!("{CRATES_IO_API}/{version}");
        let metadata = self
            .fetch_bytes(
                &metadata_url,
                "crates.io version metadata",
                MAX_METADATA_BYTES,
            )
            .await?;
        let response: RegistryResponse =
            serde_json::from_slice(&metadata).map_err(|source| UpdateError::RegistryMetadata {
                version: version.clone(),
                source,
            })?;
        let expected_checksum =
            parse_hex_checksum(&response.version.checksum).ok_or_else(|| {
                UpdateError::RegistryChecksumFormat {
                    version: version.clone(),
                }
            })?;
        let crate_url = format!("{CRATES_IO_API}/{version}/download");
        let crate_bytes = self
            .fetch_bytes(&crate_url, "crates.io package", MAX_CRATE_BYTES)
            .await?;
        let actual_checksum: [u8; 32] = Sha256::digest(&crate_bytes).into();
        if actual_checksum != expected_checksum {
            return Err(UpdateError::RegistryChecksumMismatch {
                version: version.clone(),
            });
        }
        let manifest = crate_manifest(&crate_bytes, version)?;
        let manifest: CrateManifest =
            toml::from_str(&manifest).map_err(|source| UpdateError::SigningMetadata {
                version: version.clone(),
                source: Box::new(source),
            })?;
        let signing = manifest
            .package
            .metadata
            .binstall
            .and_then(|metadata| metadata.signing)
            .ok_or_else(|| UpdateError::MissingSigningMetadata {
                version: version.clone(),
            })?;
        if signing.algorithm != "minisign" {
            return Err(UpdateError::UnsupportedSigningAlgorithm {
                version: version.clone(),
                algorithm: signing.algorithm,
            });
        }
        PublicKey::from_base64(signing.pubkey.trim()).map_err(|source| UpdateError::PublicKey {
            version: version.clone(),
            source,
        })
    }

    async fn fetch_bytes(&self, url: &str, name: &str, limit: u64) -> Result<Vec<u8>, UpdateError> {
        let mut bytes = Vec::new();
        self.download(Download::Metadata, url, name, limit, |chunk| {
            bytes.extend_from_slice(chunk);
            Ok(())
        })
        .await?;
        Ok(bytes)
    }

    /// Streams `url` into `sink`, failing once more than `limit` bytes are advertised or
    /// received.
    async fn download(
        &self,
        kind: Download,
        url: &str,
        name: &str,
        limit: u64,
        mut sink: impl FnMut(&[u8]) -> Result<(), UpdateError>,
    ) -> Result<(), UpdateError> {
        let too_large = || UpdateError::DownloadTooLarge {
            name: name.to_owned(),
            limit,
        };
        let mut response = self
            .client
            .get(url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|source| UpdateError::Http {
                operation: kind.request_operation(),
                source,
            })?;
        if response
            .content_length()
            .is_some_and(|length| length > limit)
        {
            return Err(too_large());
        }
        let mut received = 0_u64;
        while let Some(chunk) = response.chunk().await.map_err(|source| UpdateError::Http {
            operation: kind.read_operation(),
            source,
        })? {
            received = received.saturating_add(chunk.len() as u64);
            if received > limit {
                return Err(too_large());
            }
            sink(&chunk)?;
        }
        Ok(())
    }
}

fn is_not_found(error: &UpdateError) -> bool {
    matches!(error, UpdateError::Http { source, .. } if source.status() == Some(reqwest::StatusCode::NOT_FOUND))
}

fn default_cargo_install_root() -> Option<PathBuf> {
    env::var_os("CARGO_INSTALL_ROOT")
        .map(PathBuf::from)
        .or_else(|| env::var_os("CARGO_HOME").map(PathBuf::from))
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")))
}

fn cargo_update_command(root: &Path, default_root: Option<&Path>) -> String {
    let mut command = "cargo install tact --locked".to_owned();
    if default_root != Some(root) {
        command.push_str(" --root ");
        command.push_str(
            &shlex::try_quote(&root.to_string_lossy())
                .map(|root| root.into_owned())
                .unwrap_or_else(|_| format!("'{}'", root.display())),
        );
    }
    command
}

fn crate_manifest(bytes: &[u8], version: &Version) -> Result<String, UpdateError> {
    let decoder = GzDecoder::new(Cursor::new(bytes));
    let mut archive = tar::Archive::new(decoder);
    let expected = PathBuf::from(format!("tact-{version}/Cargo.toml"));
    let root = PathBuf::from(format!("tact-{version}"));
    let mut manifest = None;
    let entries = archive
        .entries()
        .map_err(|source| UpdateError::CrateArchive {
            version: version.clone(),
            source,
        })?;
    for entry in entries {
        let entry = entry.map_err(|source| UpdateError::CrateArchive {
            version: version.clone(),
            source,
        })?;
        let path = entry
            .path()
            .map_err(|source| UpdateError::CrateArchive {
                version: version.clone(),
                source,
            })?
            .into_owned();
        if !safe_archive_path(&path, &root) {
            return Err(UpdateError::UnsafeCratePath {
                version: version.clone(),
                path,
            });
        }
        if path != expected {
            continue;
        }
        if manifest.is_some() {
            return Err(UpdateError::DuplicateManifest {
                version: version.clone(),
            });
        }
        let mut contents = String::new();
        entry
            .take(MAX_METADATA_BYTES)
            .read_to_string(&mut contents)
            .map_err(|source| UpdateError::CrateArchive {
                version: version.clone(),
                source,
            })?;
        manifest = Some(contents);
    }
    manifest.ok_or_else(|| UpdateError::MissingManifest {
        version: version.clone(),
    })
}

fn verify_archive_checksum(
    archive: &NamedTempFile,
    archive_name: &str,
    checksum_file: &[u8],
    checksum_name: &str,
) -> Result<(), UpdateError> {
    let checksum_file =
        std::str::from_utf8(checksum_file).map_err(|_| UpdateError::ChecksumFile {
            name: checksum_name.to_owned(),
        })?;
    let mut fields = checksum_file.split_whitespace();
    let expected =
        fields
            .next()
            .and_then(parse_hex_checksum)
            .ok_or_else(|| UpdateError::ChecksumFile {
                name: checksum_name.to_owned(),
            })?;
    let listed_name = fields
        .next()
        .map(|name| name.trim_start_matches('*'))
        .ok_or_else(|| UpdateError::ChecksumFile {
            name: checksum_name.to_owned(),
        })?;
    if listed_name != archive_name || fields.next().is_some() {
        return Err(UpdateError::ChecksumFile {
            name: checksum_name.to_owned(),
        });
    }
    let actual = hash_file(archive).map_err(UpdateError::TemporaryWrite)?;
    if actual != expected {
        return Err(UpdateError::ArchiveChecksumMismatch {
            name: archive_name.to_owned(),
        });
    }
    Ok(())
}

fn verify_archive_signature(
    archive: &NamedTempFile,
    archive_name: &str,
    signature: &[u8],
    signature_name: &str,
    public_key: &PublicKey,
) -> Result<(), UpdateError> {
    let signature = std::str::from_utf8(signature).map_err(|_| UpdateError::SignatureEncoding {
        name: signature_name.to_owned(),
    })?;
    let signature = Signature::decode(signature).map_err(|source| UpdateError::Signature {
        name: signature_name.to_owned(),
        source,
    })?;
    let mut verifier = public_key.verify_stream(&signature).map_err(|source| {
        UpdateError::SignatureVerification {
            name: archive_name.to_owned(),
            source,
        }
    })?;
    let mut input = archive.reopen().map_err(UpdateError::TemporaryWrite)?;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = input
            .read(&mut buffer)
            .map_err(UpdateError::TemporaryWrite)?;
        if read == 0 {
            break;
        }
        verifier.update(&buffer[..read]);
    }
    verifier
        .finalize()
        .map_err(|source| UpdateError::SignatureVerification {
            name: archive_name.to_owned(),
            source,
        })
}

fn extract_binary(
    archive: &NamedTempFile,
    archive_name: &str,
    target: SupportedTarget,
    tag: &ReleaseTag,
) -> Result<ExtractedBinary, UpdateError> {
    let directory = tempdir().map_err(UpdateError::TemporaryStorage)?;
    let output = directory.path().join("tact");
    let expected_root = PathBuf::from(format!("tact-{}-{}", target.triple(), tag.name()));
    let expected_binary = expected_root.join("tact");
    let input = archive.reopen().map_err(UpdateError::TemporaryWrite)?;
    let mut archive = tar::Archive::new(GzDecoder::new(input));
    let entries = archive
        .entries()
        .map_err(|source| UpdateError::ReleaseArchive {
            name: archive_name.to_owned(),
            source,
        })?;
    let mut found = false;
    for entry in entries {
        let mut entry = entry.map_err(|source| UpdateError::ReleaseArchive {
            name: archive_name.to_owned(),
            source,
        })?;
        let path = entry
            .path()
            .map_err(|source| UpdateError::ReleaseArchive {
                name: archive_name.to_owned(),
                source,
            })?
            .into_owned();
        if !safe_archive_path(&path, &expected_root) {
            return Err(UpdateError::UnexpectedArchivePath {
                name: archive_name.to_owned(),
                path,
            });
        }
        if path != expected_binary {
            continue;
        }
        if found {
            return Err(UpdateError::DuplicateBinary {
                name: archive_name.to_owned(),
            });
        }
        if !entry.header().entry_type().is_file() {
            return Err(UpdateError::InvalidBinaryEntry {
                name: archive_name.to_owned(),
            });
        }
        entry
            .unpack(&output)
            .map_err(|source| UpdateError::ReleaseArchive {
                name: archive_name.to_owned(),
                source,
            })?;
        found = true;
    }
    if !found {
        return Err(UpdateError::MissingBinary {
            name: archive_name.to_owned(),
        });
    }
    Ok(ExtractedBinary {
        path: output,
        _directory: directory,
    })
}

struct ExtractedBinary {
    path: PathBuf,
    _directory: TempDir,
}

fn safe_archive_path(path: &Path, root: &Path) -> bool {
    path.starts_with(root)
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn hash_file(file: &NamedTempFile) -> io::Result<[u8; 32]> {
    let mut input = file.reopen()?;
    input.seek(SeekFrom::Start(0))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().into())
}

fn parse_hex_checksum(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64 {
        return None;
    }
    let mut output = [0_u8; 32];
    for (index, pair) in value.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        let pair = std::str::from_utf8(pair).ok()?;
        output[index] = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(output)
}

#[cfg(test)]
mod tests {
    use super::{
        GithubAsset, GithubReleaseResponse, Release, ReleaseTag, SupportedTarget, UpdateError,
        cargo_update_command, crate_manifest, ensure_pre_release_installable, extract_binary,
        normalize_revision, parse_hex_checksum, update_artifact_target, verify_archive_checksum,
    };
    use crate::app::installation::InstallationKind;
    use flate2::{Compression, write::GzEncoder};
    use semver::Version;
    use sha2::Digest;
    use std::io::Write;
    use tar::{Builder, Header};
    use tempfile::NamedTempFile;

    fn release(version: &str, names: &[&str]) -> Release {
        Release::parse(GithubReleaseResponse {
            tag_name: version.to_owned(),
            assets: names
                .iter()
                .map(|name| GithubAsset {
                    name: (*name).to_owned(),
                    browser_download_url: format!("https://example.com/{name}"),
                })
                .collect(),
        })
        .unwrap()
    }

    fn archive(entries: &[(&str, &[u8])]) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        {
            let encoder = GzEncoder::new(file.as_file_mut(), Compression::default());
            let mut builder = Builder::new(encoder);
            for (path, contents) in entries {
                let mut header = Header::new_gnu();
                header.set_size(contents.len() as u64);
                header.set_mode(0o755);
                header.set_cksum();
                builder.append_data(&mut header, path, *contents).unwrap();
            }
            builder.into_inner().unwrap().finish().unwrap();
        }
        file
    }

    #[test]
    fn installs_one_process_tls_provider() {
        crate::install_tls_provider();

        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
    }

    #[test]
    fn parses_v_prefixed_semver_and_compares_by_semver() {
        let release = release("v1.2.3", &[]);
        let ReleaseTag::Official(version) = release.tag else {
            panic!("a v-prefixed tag is an official release");
        };
        assert_eq!(version, Version::new(1, 2, 3));
        assert!(version > Version::new(1, 2, 2));
        assert!(Version::parse("1.2.3-beta.1").unwrap() < version);
    }

    #[test]
    fn pre_release_tags_name_one_twelve_digit_revision() {
        let tag = ReleaseTag::parse("dev-0123456789ab").unwrap();
        assert_eq!(tag, ReleaseTag::PreRelease("0123456789ab".to_owned()));
        assert_eq!(tag.name(), "dev-0123456789ab");
        assert_eq!(tag.to_string(), "pre-release 0123456789ab");
        assert_eq!(
            ReleaseTag::pre_release("0123456789abcdef0123456789abcdef01234567"),
            Some(tag),
        );
        for malformed in [
            "dev-0123456789a",
            "dev-0123456789abc",
            "dev-0123456789AB",
            "dev-",
        ] {
            assert!(matches!(
                ReleaseTag::parse(malformed),
                Err(UpdateError::ReleaseTagFormat { .. })
            ));
        }
        assert_eq!(ReleaseTag::pre_release("0123456"), None);
    }

    #[test]
    fn only_cargo_and_package_manager_installs_refuse_pre_releases() {
        for installation in [
            InstallationKind::ReleaseArchive,
            InstallationKind::PreRelease {
                revision: "0123456789ab".to_owned(),
            },
            InstallationKind::Development,
        ] {
            assert!(ensure_pre_release_installable(&installation).is_ok());
        }
        for installation in [
            InstallationKind::CratesIo {
                root: "/opt/tact".into(),
            },
            InstallationKind::External {
                manager: "nix".to_owned(),
            },
        ] {
            assert!(matches!(
                ensure_pre_release_installable(&installation),
                Err(UpdateError::PreReleaseManaged)
            ));
        }
    }

    #[test]
    fn revisions_are_lowercased_hex_of_commit_length() {
        assert_eq!(normalize_revision(" ABCDEF1 \n").unwrap(), "abcdef1");
        assert!(normalize_revision(&"a".repeat(40)).is_ok());
        for invalid in ["abcdef", "main", "abcdefg", &"a".repeat(41)] {
            assert!(matches!(
                normalize_revision(invalid),
                Err(UpdateError::InvalidRevision { .. })
            ));
        }
    }

    #[test]
    fn supports_only_published_target_triples() {
        for target in [
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu",
            "x86_64-apple-darwin",
            "aarch64-apple-darwin",
        ] {
            assert!(SupportedTarget::from_triple(target).is_ok());
        }
        assert!(matches!(
            SupportedTarget::from_triple("x86_64-pc-windows-msvc"),
            Err(UpdateError::UnsupportedTarget { .. })
        ));
    }

    #[test]
    fn cargo_update_checks_do_not_require_a_release_archive_target() {
        let installation = InstallationKind::CratesIo {
            root: "/opt/tact".into(),
        };

        assert_eq!(
            update_artifact_target(&installation, "x86_64-unknown-linux-musl").unwrap(),
            None,
        );
        assert_eq!(
            update_artifact_target(
                &InstallationKind::External {
                    manager: "nix".to_owned(),
                },
                "x86_64-unknown-linux-musl",
            )
            .unwrap(),
            None,
        );
        assert!(
            update_artifact_target(
                &InstallationKind::ReleaseArchive,
                "x86_64-unknown-linux-musl",
            )
            .is_err()
        );
        assert!(
            update_artifact_target(
                &InstallationKind::PreRelease {
                    revision: "0123456789ab".to_owned(),
                },
                "x86_64-unknown-linux-musl",
            )
            .is_err()
        );
    }

    #[test]
    fn requires_one_archive_checksum_and_signature_for_target() {
        let prefix = "tact-aarch64-apple-darwin-v2.0.0.tar.gz";
        let complete = release(
            "v2.0.0",
            &[
                prefix,
                &format!("{prefix}.sha256"),
                &format!("{prefix}.sig"),
            ],
        );
        assert!(complete.assets_for(SupportedTarget::MacosAarch64).is_ok());

        let duplicate = release(
            "v2.0.0",
            &[
                prefix,
                prefix,
                &format!("{prefix}.sha256"),
                &format!("{prefix}.sig"),
            ],
        );
        assert!(matches!(
            duplicate.assets_for(SupportedTarget::MacosAarch64),
            Err(UpdateError::DuplicateAsset { .. })
        ));

        let unsigned = release("v2.0.0", &[prefix, &format!("{prefix}.sha256")]);
        assert!(matches!(
            unsigned.assets_for(SupportedTarget::MacosAarch64),
            Err(UpdateError::MissingAsset { .. })
        ));
    }

    #[test]
    fn pre_releases_need_only_an_archive_and_checksum() {
        let prefix = "tact-aarch64-apple-darwin-dev-0123456789ab.tar.gz";
        let pre_release = release("dev-0123456789ab", &[prefix, &format!("{prefix}.sha256")]);
        let assets = pre_release
            .assets_for(SupportedTarget::MacosAarch64)
            .unwrap();
        assert!(assets.signature.is_none());
    }

    #[test]
    fn checksum_sidecar_binds_hash_and_filename() {
        let mut archive = NamedTempFile::new().unwrap();
        archive.write_all(b"release bytes").unwrap();
        let digest = sha2::Sha256::digest(b"release bytes");
        let checksum = format!("{digest:x}  tact.tar.gz\n");
        verify_archive_checksum(
            &archive,
            "tact.tar.gz",
            checksum.as_bytes(),
            "tact.tar.gz.sha256",
        )
        .unwrap();
        assert!(matches!(
            verify_archive_checksum(
                &archive,
                "other.tar.gz",
                checksum.as_bytes(),
                "tact.tar.gz.sha256"
            ),
            Err(UpdateError::ChecksumFile { .. })
        ));
    }

    #[test]
    fn crate_manifest_rejects_traversal_and_duplicates() {
        let version = Version::new(1, 0, 0);
        let duplicate = archive(&[
            ("tact-1.0.0/Cargo.toml", b"first"),
            ("tact-1.0.0/Cargo.toml", b"second"),
        ]);
        let bytes = std::fs::read(duplicate.path()).unwrap();
        assert!(matches!(
            crate_manifest(&bytes, &version),
            Err(UpdateError::DuplicateManifest { .. })
        ));

        let traversal = archive(&[("other/Cargo.toml", b"bad")]);
        let bytes = std::fs::read(traversal.path()).unwrap();
        assert!(matches!(
            crate_manifest(&bytes, &version),
            Err(UpdateError::UnsafeCratePath { .. })
        ));
    }

    #[test]
    fn release_archive_extracts_only_expected_binary() {
        let valid_archive = archive(&[
            ("tact-x86_64-unknown-linux-gnu-v1.0.0/README.md", b"readme"),
            ("tact-x86_64-unknown-linux-gnu-v1.0.0/tact", b"binary"),
        ]);
        let binary = extract_binary(
            &valid_archive,
            "tact.tar.gz",
            SupportedTarget::LinuxX86_64,
            &ReleaseTag::Official(Version::new(1, 0, 0)),
        )
        .unwrap();
        assert_eq!(std::fs::read(binary.path).unwrap(), b"binary");

        let wrong_root = archive(&[("tact-aarch64-unknown-linux-gnu-v1.0.0/tact", b"bad")]);
        assert!(matches!(
            extract_binary(
                &wrong_root,
                "tact.tar.gz",
                SupportedTarget::LinuxX86_64,
                &ReleaseTag::Official(Version::new(1, 0, 0))
            ),
            Err(UpdateError::UnexpectedArchivePath { .. })
        ));

        let pre_release = archive(&[(
            "tact-x86_64-unknown-linux-gnu-dev-0123456789ab/tact",
            b"dev",
        )]);
        let binary = extract_binary(
            &pre_release,
            "tact.tar.gz",
            SupportedTarget::LinuxX86_64,
            &ReleaseTag::PreRelease("0123456789ab".to_owned()),
        )
        .unwrap();
        assert_eq!(std::fs::read(binary.path).unwrap(), b"dev");
    }

    #[test]
    fn checksum_parser_requires_exact_sha256_hex() {
        assert!(parse_hex_checksum(&"a".repeat(64)).is_some());
        assert!(parse_hex_checksum(&"a".repeat(63)).is_none());
        assert!(parse_hex_checksum(&"z".repeat(64)).is_none());
    }

    #[test]
    fn cargo_recommendation_preserves_non_default_install_root() {
        let default = std::path::Path::new("/home/user/.cargo");
        assert_eq!(
            cargo_update_command(default, Some(default)),
            "cargo install tact --locked"
        );
        assert_eq!(
            cargo_update_command(std::path::Path::new("/opt/my tools"), Some(default)),
            "cargo install tact --locked --root '/opt/my tools'"
        );
    }
}
