use semver::Version;
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
};

static INSTALLATION: OnceLock<InstallationKind> = OnceLock::new();

#[derive(Deserialize)]
struct CargoInstallMetadata {
    v1: BTreeMap<String, Vec<String>>,
}

/// How this build was produced, which decides how it is updated and what it shows.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum InstallationKind {
    /// An official release archive, signed and published with a version tag.
    ReleaseArchive,
    /// The pre-release archive CI publishes for one commit on `main`. It updates like a release
    /// archive, and `revision` is the commit's twelve-digit abbreviated hash.
    PreRelease {
        revision: String,
    },
    CratesIo {
        root: PathBuf,
    },
    /// Managed by an external package manager, declared at build time through
    /// the `TACT_PACKAGE_MANAGER` environment variable. Updates are delegated
    /// to the named manager instead of replacing the binary in place.
    External {
        manager: String,
    },
    Development,
}

impl InstallationKind {
    pub(crate) fn is_development(&self) -> bool {
        matches!(self, Self::Development)
    }

    /// The tag of the GitHub Release that published this build: `v<version>` for an official
    /// release and `dev-<revision>` for a pre-release. A build not made from a release archive has
    /// no release of its own and names the official release of its version.
    pub(crate) fn release_tag(&self) -> String {
        match self {
            Self::PreRelease { revision } => format!("dev-{revision}"),
            _ => format!("v{}", env!("CARGO_PKG_VERSION")),
        }
    }

    /// The directory under `<home>/web/assets` that holds this build's web bundle. A release
    /// build keeps the bundle it downloaded for its release tag. A source build has its own
    /// directory, where `just install-dev` links the bundle it built, so developing never replaces
    /// a downloaded bundle and installing a release never discards a development one.
    pub(crate) fn web_bundle_directory(&self) -> String {
        match self {
            Self::Development => "development".to_owned(),
            _ => self.release_tag(),
        }
    }
}

pub(crate) fn current() -> &'static InstallationKind {
    INSTALLATION.get_or_init(|| {
        let channel = match env!("TACT_RELEASE_CHANNEL") {
            "release" => ReleaseChannel::Release,
            "pre-release" => ReleaseChannel::PreRelease {
                revision: env!("TACT_GIT_SHA"),
            },
            _ => ReleaseChannel::Development,
        };
        let package_manager =
            Some(env!("TACT_PACKAGE_MANAGER")).filter(|manager| !manager.is_empty());
        let executable = env::current_exe().ok();
        detect(
            channel,
            package_manager,
            executable.as_deref(),
            installed_packages,
        )
    })
}

/// What the build declared itself to be through `TACT_RELEASE_BUILD`.
enum ReleaseChannel<'a> {
    Release,
    PreRelease { revision: &'a str },
    Development,
}

fn detect(
    channel: ReleaseChannel<'_>,
    package_manager: Option<&str>,
    executable: Option<&Path>,
    installed_packages: impl FnOnce(&Path) -> Option<String>,
) -> InstallationKind {
    if let Some(root) =
        executable.and_then(|executable| crates_io_install_root(executable, installed_packages))
    {
        return InstallationKind::CratesIo { root };
    }
    if let Some(manager) = package_manager {
        return InstallationKind::External {
            manager: manager.to_owned(),
        };
    }
    match channel {
        ReleaseChannel::Release => InstallationKind::ReleaseArchive,
        ReleaseChannel::PreRelease { revision } => InstallationKind::PreRelease {
            revision: revision.to_owned(),
        },
        ReleaseChannel::Development => InstallationKind::Development,
    }
}

fn crates_io_install_root(
    executable: &Path,
    installed_packages: impl FnOnce(&Path) -> Option<String>,
) -> Option<PathBuf> {
    let root = candidate_cargo_install_root(executable)?;
    match cargo_metadata_tact_ownership(&root) {
        Some(true) => return Some(root),
        Some(false) => return None,
        None => {}
    }
    cargo_list_owns_tact(&installed_packages(&root)?).then_some(root)
}

fn installed_packages(root: &Path) -> Option<String> {
    let output = Command::new("cargo")
        .args(["install", "--list", "--root"])
        .arg(root)
        .args(["--color", "never"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

fn cargo_metadata_tact_ownership(root: &Path) -> Option<bool> {
    let contents = fs::read_to_string(root.join(".crates.toml")).ok()?;
    let metadata: CargoInstallMetadata = toml::from_str(&contents).ok()?;
    Some(metadata.v1.iter().any(|(package, binaries)| {
        crates_io_tact_package(package) && binaries.iter().any(|binary| binary == "tact")
    }))
}

fn crates_io_tact_package(package: &str) -> bool {
    let Some(package) = package.strip_prefix("tact ") else {
        return false;
    };
    let Some((version, source)) = package.split_once(" (") else {
        return false;
    };
    Version::parse(version).is_ok()
        && source == "registry+https://github.com/rust-lang/crates.io-index)"
}

fn candidate_cargo_install_root(executable: &Path) -> Option<PathBuf> {
    let filename = executable.file_name()?.to_str()?;
    if !matches!(filename, "tact" | "tact.exe") {
        return None;
    }
    let bin = executable.parent()?;
    (bin.file_name()? == "bin").then(|| bin.parent().map(Path::to_path_buf))?
}

fn cargo_list_owns_tact(output: &str) -> bool {
    let mut lines = output.lines().peekable();
    while let Some(header) = lines.next() {
        if header.starts_with(char::is_whitespace) {
            continue;
        }
        let Some(header) = header.strip_suffix(':') else {
            continue;
        };
        let mut fields = header.split_whitespace();
        let is_registry_tact = fields.next() == Some("tact")
            && fields
                .next()
                .and_then(|version| version.strip_prefix('v'))
                .is_some_and(|version| Version::parse(version).is_ok())
            && fields.next().is_none();
        let mut owns_binary = false;
        while lines
            .peek()
            .is_some_and(|line| line.starts_with(char::is_whitespace))
        {
            owns_binary |= lines.next().is_some_and(|line| line.trim() == "tact");
        }
        if is_registry_tact && owns_binary {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::{
        InstallationKind, ReleaseChannel, candidate_cargo_install_root, cargo_list_owns_tact,
        cargo_metadata_tact_ownership, detect,
    };
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    #[test]
    fn registry_metadata_identifies_a_crates_io_installation() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("bin/tact");
        fs::create_dir(root.path().join("bin")).unwrap();
        fs::write(
            root.path().join(".crates.toml"),
            r#"[v1]
"tact 1.2.3 (registry+https://github.com/rust-lang/crates.io-index)" = ["tact"]
"helper 1.0.0 (registry+https://github.com/rust-lang/crates.io-index)" = ["helper"]
"#,
        )
        .unwrap();

        assert_eq!(
            detect(ReleaseChannel::Development, None, Some(&executable), |_| {
                None
            }),
            InstallationKind::CratesIo {
                root: root.path().to_path_buf(),
            }
        );
        assert_eq!(
            detect(ReleaseChannel::Release, None, Some(&executable), |_| None),
            InstallationKind::CratesIo {
                root: root.path().to_path_buf(),
            }
        );
        // Cargo's ownership records outrank build-time declarations, like the
        // release flag above: a cargo-managed binary must be updated through
        // Cargo regardless of how it was produced.
        assert_eq!(
            detect(
                ReleaseChannel::Development,
                Some("nix"),
                Some(&executable),
                |_| None
            ),
            InstallationKind::CratesIo {
                root: root.path().to_path_buf(),
            }
        );
    }

    #[test]
    fn release_archives_pre_releases_and_repository_builds_are_distinct() {
        assert_eq!(
            detect(
                ReleaseChannel::Release,
                None,
                Some(Path::new("/work/target/release/tact")),
                |_| None
            ),
            InstallationKind::ReleaseArchive,
        );
        assert_eq!(
            detect(
                ReleaseChannel::PreRelease {
                    revision: "0123456789ab",
                },
                None,
                Some(Path::new("/work/target/release/tact")),
                |_| None
            ),
            InstallationKind::PreRelease {
                revision: "0123456789ab".to_owned(),
            },
        );
        assert_eq!(
            detect(
                ReleaseChannel::Development,
                None,
                Some(Path::new("/work/target/debug/tact")),
                |_| None
            ),
            InstallationKind::Development,
        );
    }

    #[test]
    fn releases_are_tagged_by_version_and_pre_releases_by_revision() {
        let pre_release = InstallationKind::PreRelease {
            revision: "0123456789ab".to_owned(),
        };
        assert_eq!(pre_release.release_tag(), "dev-0123456789ab");
        assert!(!pre_release.is_development());
        assert_eq!(pre_release.web_bundle_directory(), "dev-0123456789ab");
        assert_eq!(
            InstallationKind::Development.web_bundle_directory(),
            "development"
        );
        assert_eq!(
            InstallationKind::ReleaseArchive.release_tag(),
            format!("v{}", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn package_manager_declarations_identify_external_installations() {
        let executable = Path::new("/nix/store/cafebabe-tact-1.2.3/bin/tact");
        let external = InstallationKind::External {
            manager: "nix".to_owned(),
        };

        assert_eq!(
            detect(
                ReleaseChannel::Development,
                Some("nix"),
                Some(executable),
                |_| None
            ),
            external
        );
        assert_eq!(
            detect(
                ReleaseChannel::Release,
                Some("nix"),
                Some(executable),
                |_| None
            ),
            external
        );
        assert!(!external.is_development());
    }

    #[test]
    fn cargo_list_is_a_fallback_when_registry_metadata_is_unavailable() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("bin/tact");
        fs::create_dir(root.path().join("bin")).unwrap();

        assert_eq!(
            detect(ReleaseChannel::Development, None, Some(&executable), |_| {
                Some("tact v1.2.3:\n    tact\n".to_owned())
            }),
            InstallationKind::CratesIo {
                root: root.path().to_path_buf(),
            }
        );
    }

    #[test]
    fn cargo_list_only_matches_tact_owning_the_tact_binary() {
        assert!(cargo_list_owns_tact(
            "bat v0.26.1:\n    bat\ntact v1.2.3:\n    tact\n"
        ));
        assert!(!cargo_list_owns_tact(
            "tact v1.2.3 (/work/tact):\n    tact\n"
        ));
        assert!(!cargo_list_owns_tact(
            "tact v1.2.3 (git+https://example.com/tact):\n    tact\n"
        ));
        assert!(!cargo_list_owns_tact("tact v1.2.3:\n    helper\n"));
    }

    #[test]
    fn registry_metadata_rejects_non_crates_io_installations() {
        for source in [
            "path+file:///work/tact",
            "git+https://example.com/tact",
            "registry+https://example.com/index",
        ] {
            let root = tempfile::tempdir().unwrap();
            fs::write(
                root.path().join(".crates.toml"),
                format!("[v1]\n\"tact 1.2.3 ({source})\" = [\"tact\"]\n"),
            )
            .unwrap();

            assert_eq!(cargo_metadata_tact_ownership(root.path()), Some(false));
        }
    }

    #[test]
    fn cargo_install_root_requires_the_expected_bin_layout() {
        assert_eq!(
            candidate_cargo_install_root(Path::new("/opt/tools/bin/tact")),
            Some(PathBuf::from("/opt/tools"))
        );
        assert_eq!(
            candidate_cargo_install_root(Path::new("/work/target/debug/tact")),
            None
        );
        assert_eq!(
            candidate_cargo_install_root(Path::new("/opt/tools/bin/other")),
            None
        );
    }
}
