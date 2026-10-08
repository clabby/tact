//! The effective memory section: local store limits and the optional shared remote backend.
//!
//! A remote backend is either fully configured or absent. Its bearer token is shared through
//! `Arc` across cloned configurations and renders redacted. Workspace roots are kept as written
//! (resolved against the configuration directory) and are canonicalized only when a workspace is
//! matched, so a root that does not exist yet does not prevent loading.

use super::{
    file::{LocalMemoryConfigFile, MemoryConfigFile, RemoteMemoryConfigFile},
    mcp::validate_mcp_url,
    render::REDACTED,
    resolve_path,
};
use crate::app::{error::RemoteMemoryConfigError, secret::SecretString};
use serde::{Serialize, Serializer};
use std::{
    fs,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::Arc,
};
use tact_memory::MemoryLimits;
use zeroize::Zeroize;

/// Effective global memory configuration.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct MemoryConfig {
    enabled: bool,
    local: LocalMemoryConfig,
    #[serde(serialize_with = "serialize_remote_memory_config")]
    remote: Option<RemoteMemoryConfig>,
}

/// Effective local memory configuration.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct LocalMemoryConfig {
    max_records: usize,
    max_record_bytes: usize,
    max_total_bytes: usize,
}

/// Effective remote memory configuration.
#[derive(Clone, Debug)]
pub(crate) struct RemoteMemoryConfig {
    endpoint: String,
    namespace: String,
    bearer_token: Arc<SecretString>,
    workspace_roots: Vec<PathBuf>,
}

impl MemoryConfig {
    pub(super) fn new(
        file: MemoryConfigFile,
        config_dir: &Path,
    ) -> Result<Self, RemoteMemoryConfigError> {
        Ok(Self {
            enabled: file.enabled,
            local: LocalMemoryConfig::new(file.local),
            remote: RemoteMemoryConfig::new(file.remote, config_dir)?,
        })
    }

    pub(crate) const fn enabled(&self) -> bool {
        self.enabled
    }

    pub(crate) const fn local(&self) -> &LocalMemoryConfig {
        &self.local
    }

    pub(crate) const fn remote(&self) -> Option<&RemoteMemoryConfig> {
        self.remote.as_ref()
    }
}

impl LocalMemoryConfig {
    /// Each limit defaults independently to the production limit.
    fn new(file: LocalMemoryConfigFile) -> Self {
        let production = MemoryLimits::PRODUCTION;
        Self {
            max_records: file
                .max_records
                .map_or(production.records, NonZeroUsize::get),
            max_record_bytes: file
                .max_record_bytes
                .map_or(production.content_bytes, NonZeroUsize::get),
            max_total_bytes: file
                .max_total_bytes
                .map_or(production.total_content_bytes, NonZeroUsize::get),
        }
    }

    pub(crate) const fn limits(&self) -> MemoryLimits {
        MemoryLimits {
            records: self.max_records,
            content_bytes: self.max_record_bytes,
            total_content_bytes: self.max_total_bytes,
            ..MemoryLimits::PRODUCTION
        }
    }
}

impl RemoteMemoryConfig {
    /// Resolves the remote section, which is absent when every field is empty. Rejected endpoints
    /// are zeroized because they may embed credentials.
    fn new(
        mut file: RemoteMemoryConfigFile,
        config_dir: &Path,
    ) -> Result<Option<Self>, RemoteMemoryConfigError> {
        let bearer_token = file.bearer_token.take();
        let fields_present = [
            !file.endpoint.is_empty(),
            !file.namespace.is_empty(),
            !bearer_token.is_empty(),
            !file.workspace_roots.is_empty(),
        ];
        if !fields_present.contains(&true) {
            return Ok(None);
        }
        if fields_present.contains(&false) {
            file.endpoint.zeroize();
            return Err(RemoteMemoryConfigError::Incomplete);
        }
        if let Err(source) = validate_mcp_url(&file.endpoint) {
            file.endpoint.zeroize();
            return Err(RemoteMemoryConfigError::Endpoint(source));
        }
        if file.namespace.trim() != file.namespace {
            return Err(RemoteMemoryConfigError::NamespaceWhitespace);
        }
        if file.namespace.chars().any(char::is_control) {
            return Err(RemoteMemoryConfigError::NamespaceControl);
        }
        if !tact_memory::protocol::is_valid_namespace(&file.namespace) {
            return Err(RemoteMemoryConfigError::NamespaceInvalid);
        }
        Ok(Some(Self {
            endpoint: file.endpoint,
            namespace: file.namespace,
            bearer_token: Arc::new(SecretString::new(bearer_token.to_string())),
            workspace_roots: file
                .workspace_roots
                .into_iter()
                .map(|root| resolve_path(root, config_dir))
                .collect(),
        }))
    }

    pub(crate) fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub(crate) fn namespace(&self) -> &str {
        &self.namespace
    }

    pub(crate) fn bearer_token(&self) -> &str {
        self.bearer_token.expose_secret()
    }

    /// Checks an already-canonical workspace against configured roots and their Git worktrees.
    pub(crate) fn matches_workspace(
        &self,
        canonical_workspace: &Path,
    ) -> Result<bool, RemoteMemoryConfigError> {
        let mut matches = false;
        let mut canonical_roots = Vec::with_capacity(self.workspace_roots.len());
        for root in &self.workspace_roots {
            let canonical_root = root.canonicalize().map_err(|source| {
                RemoteMemoryConfigError::ResolveWorkspaceRoot {
                    path: root.clone(),
                    source,
                }
            })?;
            if !canonical_root.is_dir() {
                return Err(RemoteMemoryConfigError::WorkspaceRootNotDirectory(
                    root.clone(),
                ));
            }
            matches |= canonical_workspace.starts_with(&canonical_root);
            canonical_roots.push(canonical_root);
        }
        if matches {
            return Ok(true);
        }

        let Some(workspace_repository) = repository_common_directory(canonical_workspace) else {
            return Ok(false);
        };
        for root in canonical_roots {
            if workspace_repository.starts_with(&root)
                || repository_common_directory(&root).as_ref() == Some(&workspace_repository)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// The canonical common Git directory for `path`. A linked worktree resolves to its main
/// repository's directory only when that repository registered the worktree, so a `.git` file
/// cannot claim membership in an unrelated repository.
fn repository_common_directory(path: &Path) -> Option<PathBuf> {
    for directory in path.ancestors() {
        let dot_git = directory.join(".git");
        let Ok(metadata) = dot_git.symlink_metadata() else {
            continue;
        };
        if metadata.is_dir() {
            return dot_git.canonicalize().ok();
        }
        if !metadata.is_file() {
            return None;
        }

        let contents = fs::read_to_string(&dot_git).ok()?;
        let target = Path::new(contents.trim().strip_prefix("gitdir: ")?);
        let target = if target.is_absolute() {
            target.to_path_buf()
        } else {
            directory.join(target)
        };
        let git_directory = target.canonicalize().ok()?;

        let common_directory_file = git_directory.join("commondir");
        let registered_git_file = fs::read_to_string(git_directory.join("gitdir")).ok()?;
        let registered_git_file = Path::new(registered_git_file.trim());
        let registered_git_file = if registered_git_file.is_absolute() {
            registered_git_file.to_path_buf()
        } else {
            git_directory.join(registered_git_file)
        };
        if registered_git_file.canonicalize().ok()? != dot_git.canonicalize().ok()? {
            return None;
        }

        let contents = fs::read_to_string(common_directory_file).ok()?;
        return git_directory.join(contents.trim()).canonicalize().ok();
    }
    None
}

/// Renders the remote section with every key present, so an absent backend renders empty values.
fn serialize_remote_memory_config<S>(
    remote: &Option<RemoteMemoryConfig>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    #[derive(Serialize)]
    struct RenderedRemoteMemoryConfig<'a> {
        endpoint: &'a str,
        namespace: &'a str,
        bearer_token: &'a str,
        workspace_roots: &'a [PathBuf],
    }

    let rendered = match remote {
        Some(remote) => RenderedRemoteMemoryConfig {
            endpoint: &remote.endpoint,
            namespace: &remote.namespace,
            bearer_token: REDACTED,
            workspace_roots: &remote.workspace_roots,
        },
        None => RenderedRemoteMemoryConfig {
            endpoint: "",
            namespace: "",
            bearer_token: "",
            workspace_roots: &[],
        },
    };
    rendered.serialize(serializer)
}

#[cfg(test)]
mod tests {
    use crate::app::{
        config::{
            Environment,
            test_support::{
                load_config, load_config_at, load_default, load_without_environment,
                remote_memory_config,
            },
        },
        error::{ConfigError, Error, McpUrlError, RemoteMemoryConfigError},
    };
    use std::{fs, sync::Arc};
    use tact_memory::MemoryLimits;
    use tempfile::tempdir;

    #[test]
    fn memory_is_disabled_by_default() {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        let config = load_default(
            Environment {
                home: Some(home),
                ..Environment::default()
            },
            directory.path(),
        )
        .unwrap();

        assert!(!config.memory().enabled());
        assert_eq!(config.memory().local().limits(), MemoryLimits::PRODUCTION);

        let rendered: toml::Value = toml::from_str(&config.to_toml().unwrap()).unwrap();
        assert_eq!(rendered["memory"]["enabled"].as_bool(), Some(false));
    }

    #[test]
    fn memory_can_be_enabled_from_the_config_file() {
        let directory = tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        fs::write(&config_path, "[memory]\nenabled = true\n").unwrap();

        let config = load_config_at(config_path, directory.path()).unwrap();

        assert!(config.memory().enabled());

        let rendered: toml::Value = toml::from_str(&config.to_toml().unwrap()).unwrap();
        assert_eq!(rendered["memory"]["enabled"].as_bool(), Some(true));
    }

    #[test]
    fn local_memory_limits_can_be_configured_and_rendered() {
        let config = load_config(
            "[memory.local]\nmax_records = 1024\nmax_record_bytes = 2048\nmax_total_bytes = 65536\n",
        )
        .unwrap();

        let limits = config.memory().local().limits();
        assert_eq!(limits.records, 1_024);
        assert_eq!(limits.content_bytes, 2_048);
        assert_eq!(limits.total_content_bytes, 65_536);

        let rendered_toml = config.to_toml().unwrap();
        let rendered: toml::Value = toml::from_str(&rendered_toml).unwrap();
        assert_eq!(
            rendered["memory"]["local"]["max_records"].as_integer(),
            Some(1_024)
        );
        assert_eq!(
            rendered["memory"]["local"]["max_record_bytes"].as_integer(),
            Some(2_048)
        );
        assert_eq!(
            rendered["memory"]["local"]["max_total_bytes"].as_integer(),
            Some(65_536)
        );
        assert_eq!(
            load_config(&rendered_toml)
                .unwrap()
                .memory()
                .local()
                .limits(),
            limits
        );
    }

    #[test]
    fn local_memory_limits_default_independently() {
        for (field, expected) in [
            (
                "max_records",
                MemoryLimits {
                    records: 7,
                    ..MemoryLimits::PRODUCTION
                },
            ),
            (
                "max_record_bytes",
                MemoryLimits {
                    content_bytes: 7,
                    ..MemoryLimits::PRODUCTION
                },
            ),
            (
                "max_total_bytes",
                MemoryLimits {
                    total_content_bytes: 7,
                    ..MemoryLimits::PRODUCTION
                },
            ),
        ] {
            let config = load_config(&format!("[memory.local]\n{field} = 7\n")).unwrap();
            assert_eq!(config.memory().local().limits(), expected, "{field}");
        }
    }

    #[test]
    fn local_memory_limits_must_be_positive() {
        for field in ["max_records", "max_record_bytes", "max_total_bytes"] {
            let error = load_config(&format!("[memory.local]\n{field} = 0\n")).unwrap_err();
            assert!(
                matches!(error, Error::Config(ConfigError::Parse { .. })),
                "{field}"
            );
        }
    }

    #[test]
    fn complete_remote_memory_config_is_resolved_and_rendered() {
        let directory = tempdir().unwrap();
        let config_dir = directory.path().join("settings");
        let config_path = config_dir.join("config.toml");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(
            &config_path,
            format!(
                "[memory]\nenabled = true\n{}",
                remote_memory_config(
                    "https://memory.example/v1",
                    "personal-author-namespace",
                    "TACT_MEMORY_TOKEN",
                    "../allowed/./root",
                )
            ),
        )
        .unwrap();

        let config = load_config_at(config_path, directory.path()).unwrap();
        let remote = config.memory().remote().unwrap();
        let expected_root = config_dir.join("../allowed/./root");

        assert_eq!(remote.endpoint(), "https://memory.example/v1");
        assert_eq!(remote.namespace(), "personal-author-namespace");
        assert_eq!(remote.bearer_token(), "TACT_MEMORY_TOKEN");
        let cloned = config.clone();
        assert!(Arc::ptr_eq(
            &remote.bearer_token,
            &cloned.memory().remote().unwrap().bearer_token
        ));
        let rendered_toml = config.to_toml().unwrap();
        let rendered: toml::Value = toml::from_str(&rendered_toml).unwrap();
        assert_eq!(
            rendered["memory"]["remote"]["workspace_roots"][0].as_str(),
            expected_root.to_str()
        );

        assert_eq!(
            rendered["memory"]["remote"]["bearer_token"].as_str(),
            Some("[REDACTED]")
        );
        let debug = format!("{config:?}");
        assert!(!debug.contains("TACT_MEMORY_TOKEN"));
    }

    #[test]
    fn partial_remote_memory_config_is_rejected() {
        let cases = [
            "endpoint = \"https://memory.example/v1\"",
            "namespace = \"personal-author-namespace\"",
            "bearer_token = \"TACT_MEMORY_TOKEN\"",
            "workspace_roots = [\"allowed\"]",
            "endpoint = \"https://memory.example/v1\"\nnamespace = \"personal-author-namespace\"\nbearer_token = \"TACT_MEMORY_TOKEN\"",
        ];

        for fields in cases {
            let error = load_config(&format!("[memory.remote]\n{fields}\n")).unwrap_err();
            assert!(matches!(
                error,
                Error::Config(ConfigError::RemoteMemory(
                    RemoteMemoryConfigError::Incomplete
                ))
            ));
        }
    }

    #[test]
    fn remote_memory_names_reject_whitespace_and_namespace_controls() {
        let error = load_config(&remote_memory_config(
            "https://memory.example/v1",
            " personal",
            "TACT_MEMORY_TOKEN",
            "allowed",
        ))
        .unwrap_err();
        assert!(matches!(
            error,
            Error::Config(ConfigError::RemoteMemory(
                RemoteMemoryConfigError::NamespaceWhitespace
            ))
        ));

        let error = load_config(&remote_memory_config(
            "https://memory.example/v1",
            "personal\\u0007namespace",
            "TACT_MEMORY_TOKEN",
            "allowed",
        ))
        .unwrap_err();
        assert!(matches!(
            error,
            Error::Config(ConfigError::RemoteMemory(
                RemoteMemoryConfigError::NamespaceControl
            ))
        ));

        for namespace in ["personal/team".to_owned(), "x".repeat(129)] {
            let error = load_config(&remote_memory_config(
                "https://memory.example/v1",
                &namespace,
                "TACT_MEMORY_TOKEN",
                "allowed",
            ))
            .unwrap_err();
            assert!(matches!(
                error,
                Error::Config(ConfigError::RemoteMemory(
                    RemoteMemoryConfigError::NamespaceInvalid
                ))
            ));
        }
    }

    #[test]
    fn remote_memory_endpoints_require_http_without_leaking_userinfo() {
        let error = load_config(&remote_memory_config(
            "file:///tmp/memory.sock",
            "personal",
            "TACT_MEMORY_TOKEN",
            "allowed",
        ))
        .unwrap_err();
        assert!(matches!(
            error,
            Error::Config(ConfigError::RemoteMemory(
                RemoteMemoryConfigError::Endpoint(McpUrlError::UnsupportedScheme)
            ))
        ));

        let error = load_config(&remote_memory_config(
            "https://user:not-a-real-secret@memory.example/v1",
            "personal",
            "TACT_MEMORY_TOKEN",
            "allowed",
        ))
        .unwrap_err();
        let rendered = format!("{error:?} {error}");
        assert!(matches!(
            error,
            Error::Config(ConfigError::RemoteMemory(
                RemoteMemoryConfigError::Endpoint(McpUrlError::Credentials)
            ))
        ));
        assert!(!rendered.contains("not-a-real-secret"));
    }

    #[test]
    fn remote_memory_scope_matches_path_components() {
        let directory = tempdir().unwrap();
        let allowed = directory.path().join("allowed");
        let child = allowed.join("child");
        let similarly_named = directory.path().join("allowed-other");
        fs::create_dir_all(&child).unwrap();
        fs::create_dir_all(&similarly_named).unwrap();
        let config_path = directory.path().join("config.toml");
        fs::write(
            &config_path,
            remote_memory_config(
                "https://memory.example/v1",
                "personal",
                "TACT_MEMORY_TOKEN",
                "allowed",
            ),
        )
        .unwrap();
        let config = load_config_at(config_path, directory.path()).unwrap();
        let remote = config.memory().remote().unwrap();

        assert!(
            remote
                .matches_workspace(&allowed.canonicalize().unwrap())
                .unwrap()
        );
        assert!(
            remote
                .matches_workspace(&child.canonicalize().unwrap())
                .unwrap()
        );
        assert!(
            !remote
                .matches_workspace(&similarly_named.canonicalize().unwrap())
                .unwrap()
        );
    }

    #[test]
    fn remote_memory_roots_are_validated_only_when_matching() {
        let directory = tempdir().unwrap();
        let config_dir = directory.path().join("settings");
        let config_path = config_dir.join("config.toml");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(
            &config_path,
            remote_memory_config(
                "https://memory.example/v1",
                "personal",
                "TACT_MEMORY_TOKEN",
                "missing/../spelled-root",
            ),
        )
        .unwrap();

        let config = load_config_at(config_path, directory.path()).unwrap();
        let remote = config.memory().remote().unwrap();
        let expected_root = config_dir.join("missing/../spelled-root");
        let rendered: toml::Value = toml::from_str(&config.to_toml().unwrap()).unwrap();
        assert_eq!(
            rendered["memory"]["remote"]["workspace_roots"][0].as_str(),
            expected_root.to_str()
        );

        let error = remote
            .matches_workspace(&directory.path().canonicalize().unwrap())
            .unwrap_err();
        assert!(matches!(
            error,
            RemoteMemoryConfigError::ResolveWorkspaceRoot { path, .. }
                if path == expected_root
        ));

        fs::create_dir(config_dir.join("missing")).unwrap();
        fs::write(config_dir.join("spelled-root"), "not a directory").unwrap();
        let error = remote
            .matches_workspace(&directory.path().canonicalize().unwrap())
            .unwrap_err();
        assert!(matches!(
            error,
            RemoteMemoryConfigError::WorkspaceRootNotDirectory(path)
                if path == expected_root
        ));
    }

    #[cfg(unix)]
    #[test]
    fn remote_memory_scope_uses_symlink_targets() {
        use std::os::unix::fs::symlink;

        let directory = tempdir().unwrap();
        let allowed = directory.path().join("allowed");
        let child = allowed.join("child");
        let outside = directory.path().join("outside");
        fs::create_dir_all(&child).unwrap();
        fs::create_dir_all(&outside).unwrap();
        symlink(&allowed, directory.path().join("allowed-alias")).unwrap();
        symlink(&outside, allowed.join("escape")).unwrap();
        let config_path = directory.path().join("config.toml");
        fs::write(
            &config_path,
            remote_memory_config(
                "https://memory.example/v1",
                "personal",
                "TACT_MEMORY_TOKEN",
                "allowed-alias",
            ),
        )
        .unwrap();
        let config = load_config_at(config_path, directory.path()).unwrap();
        let remote = config.memory().remote().unwrap();

        assert!(
            remote
                .matches_workspace(&child.canonicalize().unwrap())
                .unwrap()
        );
        assert!(
            !remote
                .matches_workspace(&allowed.join("escape").canonicalize().unwrap())
                .unwrap()
        );
    }

    #[test]
    fn remote_memory_scope_includes_only_registered_linked_worktrees() {
        let directory = tempdir().unwrap();
        let repository = directory.path().join("repository");
        let git_directory = repository.join(".git");
        let worktree = directory.path().join("feature-worktree");
        let unregistered = directory.path().join("unregistered-worktree");
        let worktree_git_directory = git_directory.join("worktrees/feature");
        fs::create_dir_all(&worktree_git_directory).unwrap();
        fs::create_dir(&worktree).unwrap();
        fs::create_dir(&unregistered).unwrap();
        fs::write(worktree_git_directory.join("commondir"), "../..\n").unwrap();
        fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", worktree_git_directory.display()),
        )
        .unwrap();
        fs::write(
            worktree_git_directory.join("gitdir"),
            format!("{}\n", worktree.join(".git").display()),
        )
        .unwrap();
        fs::write(
            unregistered.join(".git"),
            format!("gitdir: {}\n", git_directory.display()),
        )
        .unwrap();
        let config_path = directory.path().join("config.toml");
        fs::write(
            &config_path,
            remote_memory_config(
                "https://memory.example/v1",
                "personal",
                "TACT_MEMORY_TOKEN",
                repository.to_str().unwrap(),
            ),
        )
        .unwrap();
        let config = load_config_at(config_path, directory.path()).unwrap();

        assert!(
            config
                .memory()
                .remote()
                .unwrap()
                .matches_workspace(&worktree.canonicalize().unwrap())
                .unwrap()
        );
        assert!(
            !config
                .memory()
                .remote()
                .unwrap()
                .matches_workspace(&unregistered.canonicalize().unwrap())
                .unwrap()
        );
    }

    #[test]
    fn unknown_memory_fields_are_rejected() {
        let directory = tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        fs::write(&config_path, "[memory]\nenabled = true\nworkspace = true\n").unwrap();

        let error = load_without_environment(config_path, directory.path()).unwrap_err();

        assert!(matches!(error, Error::Config(ConfigError::Parse { .. })));
    }
}
