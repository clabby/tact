//! The configuration file schema.
//!
//! These types mirror the TOML file exactly: every field is optional, unknown fields are rejected,
//! and nothing is resolved against the CLI, the environment, or defaults. The effective section
//! types own that resolution. Credentials parsed from the file are held in zeroizing owners and
//! are discarded when the file's permissions do not keep them private.

use super::{
    providers::{AuthMode, ClaudeConfig, OpenAiConfig},
    settings::{ReasoningEffort, ReasoningMode, Speed, Transport},
};
use crate::app::{
    error::{ConfigError, Result},
    model,
    theme::Theme,
};
use nanocodex::HarnessModel as Model;
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fmt, fs,
    io::ErrorKind,
    net::IpAddr,
    num::{NonZeroU16, NonZeroUsize},
    path::{Path, PathBuf},
    result::Result as StdResult,
};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct ConfigFile {
    pub(super) auth: AuthConfigFile,
    pub(super) openai: OpenAiConfig,
    pub(super) claude: ClaudeConfig,
    pub(super) agent: AgentConfigFile,
    pub(super) mcp_servers: BTreeMap<String, McpServerConfigFile>,
    pub(super) skills: SkillsConfigFile,
    pub(super) memory: MemoryConfigFile,
    pub(super) subagents: SubagentsConfigFile,
    pub(super) web: WebConfigFile,
    pub(super) tui: TuiConfigFile,
    pub(super) theme: Theme,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct AuthConfigFile {
    pub(super) mode: Option<AuthMode>,
    pub(super) file: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct AgentConfigFile {
    pub(super) workspace: Option<PathBuf>,
    #[serde(default, deserialize_with = "model::deserialize_optional")]
    pub(super) model: Option<Model>,
    pub(super) thinking: Option<ReasoningEffort>,
    pub(super) reasoning_mode: Option<ReasoningMode>,
    pub(super) speed: Option<Speed>,
    /// The legacy boolean speed preference, read only when `speed` is absent.
    pub(super) fast_mode: Option<bool>,
    pub(super) max_subagents: Option<usize>,
    pub(super) instructions: Option<String>,
    pub(super) append_instructions: Option<String>,
    pub(super) web_search: Option<bool>,
    pub(super) image_generation: Option<bool>,
    pub(super) websocket_url: Option<String>,
    pub(super) api_base_url: Option<String>,
    pub(super) transport: Option<Transport>,
    pub(super) completion_hook: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
pub(super) enum McpServerConfigFile {
    Stdio(McpStdioConfigFile),
    Http(McpHttpConfigFile),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct McpStdioConfigFile {
    pub(super) command: String,
    #[serde(default)]
    pub(super) args: Vec<String>,
    #[serde(default)]
    pub(super) env: BTreeMap<String, String>,
    pub(super) cwd: Option<PathBuf>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct McpHttpConfigFile {
    pub(super) url: String,
    pub(super) bearer_token_env_var: Option<String>,
    #[serde(default)]
    pub(super) header_env: BTreeMap<String, String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct SkillsConfigFile {
    pub(super) enabled: bool,
    pub(super) roots: Vec<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct MemoryConfigFile {
    pub(super) enabled: bool,
    pub(super) local: LocalMemoryConfigFile,
    pub(super) remote: RemoteMemoryConfigFile,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct LocalMemoryConfigFile {
    pub(super) max_records: Option<NonZeroUsize>,
    pub(super) max_record_bytes: Option<NonZeroUsize>,
    pub(super) max_total_bytes: Option<NonZeroUsize>,
}

/// Remote memory settings. Every field is empty when the section is absent; a partially filled
/// section is rejected during resolution.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct RemoteMemoryConfigFile {
    pub(super) endpoint: String,
    pub(super) namespace: String,
    pub(super) bearer_token: RemoteMemoryTokenFile,
    pub(super) workspace_roots: Vec<PathBuf>,
}

/// The remote memory bearer token as parsed from the file.
#[derive(Default, Deserialize, Zeroize, ZeroizeOnDrop)]
#[serde(transparent)]
pub(super) struct RemoteMemoryTokenFile(String);

impl RemoteMemoryTokenFile {
    pub(super) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Moves the token into a zeroizing owner, leaving this one empty.
    pub(super) fn take(&mut self) -> Zeroizing<String> {
        Zeroizing::new(std::mem::take(&mut self.0))
    }
}

impl fmt::Debug for RemoteMemoryTokenFile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct SubagentsConfigFile {
    pub(super) enabled: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct WebConfigFile {
    pub(super) enabled: Option<bool>,
    pub(super) bind: Option<IpAddr>,
    pub(super) port: Option<u16>,
    pub(super) public_url: Option<String>,
    pub(super) tailscale: Option<bool>,
    pub(super) max_live_sessions: Option<NonZeroUsize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct TuiConfigFile {
    pub(super) mouse_scroll_lines: Option<NonZeroU16>,
}

impl ConfigFile {
    /// Reads the file, or the defaults when it is absent and not `required`.
    pub(super) fn read(path: &Path, required: bool) -> Result<Self> {
        let contents = match fs::read_to_string(path) {
            Ok(contents) => Zeroizing::new(contents),
            Err(source) if source.kind() == ErrorKind::NotFound && !required => {
                return Ok(Self::default());
            }
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.to_path_buf(),
                    source,
                }
                .into());
            }
        };
        Ok(Self::parse(&contents, path)?)
    }

    /// Parses file text attributed to `path` in diagnostics.
    pub(super) fn parse(text: &str, path: &Path) -> StdResult<Self, ConfigError> {
        toml::from_str(text).map_err(|mut source| {
            // TOML errors retain the entire input in a non-zeroizing allocation and render the
            // failing line, which may hold a credential. Remove it before the error can outlive
            // the caller's zeroizing buffer.
            source.set_input(None);
            ConfigError::Parse {
                path: path.to_path_buf(),
                source,
            }
        })
    }

    pub(super) fn has_credentials(&self) -> bool {
        !self.memory.remote.bearer_token.is_empty()
            || self.openai.api_key.is_some()
            || self.claude.api_key.is_some()
    }

    /// Rejects credentials stored in a file that other users may read. The credentials are
    /// discarded before the error is returned.
    #[cfg(unix)]
    pub(super) fn validate_secret_permissions(&mut self, path: &Path) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        if !self.has_credentials() {
            return Ok(());
        }
        let error = match fs::metadata(path) {
            Ok(metadata) => {
                let mode = metadata.permissions().mode() & 0o777;
                if mode & 0o077 == 0 {
                    return Ok(());
                }
                ConfigError::InsecureSecretPermissions {
                    path: path.to_path_buf(),
                    mode,
                }
            }
            Err(source) => ConfigError::Read {
                path: path.to_path_buf(),
                source,
            },
        };
        self.discard_credentials();
        Err(error.into())
    }

    /// Rejects every credential stored in the file, because this platform's file privacy cannot
    /// be verified. The credentials are discarded before the error is returned.
    #[cfg(not(unix))]
    pub(super) fn validate_secret_permissions(&mut self, path: &Path) -> Result<()> {
        if !self.has_credentials() {
            return Ok(());
        }
        self.discard_credentials();
        Err(ConfigError::UnsupportedSecretPermissions {
            path: path.to_path_buf(),
        }
        .into())
    }

    fn discard_credentials(&mut self) {
        self.memory.remote.bearer_token.zeroize();
        drop(self.openai.api_key.take());
        drop(self.claude.api_key.take());
    }
}

#[cfg(test)]
mod tests {
    use super::{RemoteMemoryConfigFile, RemoteMemoryTokenFile};
    use crate::app::{
        config::{
            Config, ConfigOverrides, Environment,
            test_support::{
                assert_error_redacts, load_config, load_without_environment, remote_memory_config,
            },
        },
        error::{ConfigError, Error},
    };
    use std::fs;
    use tempfile::tempdir;
    use zeroize::ZeroizeOnDrop;

    #[test]
    fn parsed_remote_memory_token_is_a_redacted_zeroizing_owner() {
        fn assert_zeroize_on_drop<T: ZeroizeOnDrop>() {}

        assert_zeroize_on_drop::<RemoteMemoryTokenFile>();
        let file = toml::from_str::<RemoteMemoryConfigFile>(
            "endpoint = 'https://memory.example/'\nnamespace = 'alice'\nbearer_token = 'private-token'\nworkspace_roots = ['/workspace']",
        )
        .unwrap();
        assert!(!format!("{file:?}").contains("private-token"));
    }

    #[test]
    fn malformed_provider_keys_do_not_appear_in_parse_errors() {
        for provider in ["openai", "claude"] {
            for (value, secret) in [
                ("987654321", "987654321"),
                ("['secret-sentinel']", "secret-sentinel"),
                ("{ value = 'secret-sentinel' }", "secret-sentinel"),
                ("'secret-sentinel' trailing", "secret-sentinel"),
            ] {
                let error = load_config(&format!("[{provider}]\napi_key = {value}\n")).unwrap_err();
                assert!(matches!(error, Error::Config(ConfigError::Parse { .. })));
                assert_error_redacts(&error, secret);
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn direct_remote_token_requires_private_config_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        fs::write(
            &config_path,
            remote_memory_config(
                "https://memory.example/v1",
                "personal",
                "permission-test-token",
                "allowed",
            ),
        )
        .unwrap();
        let load = || {
            Config::load_with(
                ConfigOverrides {
                    path: Some(config_path.clone()),
                    ..ConfigOverrides::default()
                },
                Environment {
                    codex_home: Some(directory.path().join("codex")),
                    ..Environment::default()
                },
                directory.path(),
            )
        };

        fs::set_permissions(&config_path, fs::Permissions::from_mode(0o600)).unwrap();
        load().unwrap();

        fs::set_permissions(&config_path, fs::Permissions::from_mode(0o644)).unwrap();
        let error = load().unwrap_err();
        let rendered = format!("{error:?} {error}");
        assert!(matches!(
            error,
            Error::Config(ConfigError::InsecureSecretPermissions { ref path, mode })
                if path == &config_path && mode == 0o644
        ));
        assert!(!rendered.contains("permission-test-token"));
    }

    #[cfg(unix)]
    #[test]
    fn provider_config_keys_require_private_permissions_even_when_unused() {
        use std::os::unix::fs::PermissionsExt;

        for provider in ["openai", "claude"] {
            let directory = tempdir().unwrap();
            let config_path = directory.path().join("config.toml");
            let secret = "sk-ant-api-permission-fixture";
            fs::write(
                &config_path,
                format!("[auth]\nmode = 'chatgpt'\n[{provider}]\napi_key = '{secret}'\n"),
            )
            .unwrap();
            let load = || {
                Config::load_with(
                    ConfigOverrides {
                        path: Some(config_path.clone()),
                        ..ConfigOverrides::default()
                    },
                    Environment {
                        codex_home: Some(directory.path().join("codex")),
                        ..Environment::default()
                    },
                    directory.path(),
                )
            };

            fs::set_permissions(&config_path, fs::Permissions::from_mode(0o600)).unwrap();
            load().unwrap();
            fs::set_permissions(&config_path, fs::Permissions::from_mode(0o644)).unwrap();
            let error = load().unwrap_err();
            assert!(matches!(
                error,
                Error::Config(ConfigError::InsecureSecretPermissions { ref path, mode })
                    if path == &config_path && mode == 0o644
            ));
            assert_error_redacts(&error, secret);
        }
    }

    #[cfg(not(unix))]
    #[test]
    fn provider_config_keys_require_verifiable_file_privacy() {
        for provider in ["openai", "claude"] {
            let secret = "sk-ant-api-permission-fixture";
            let error = load_config(&format!("[{provider}]\napi_key = '{secret}'\n")).unwrap_err();
            assert!(matches!(
                error,
                Error::Config(ConfigError::UnsupportedSecretPermissions { .. })
            ));
            assert_error_redacts(&error, secret);
        }
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let directory = tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        fs::write(&config_path, "unknown = true\n").unwrap();

        let error = load_without_environment(config_path, directory.path()).unwrap_err();

        assert!(matches!(error, Error::Config(ConfigError::Parse { .. })));
    }
}
