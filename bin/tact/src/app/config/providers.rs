//! Model provider credentials, authentication policy, and Claude availability.
//!
//! API keys stored in the file are shared through `Arc` between the provider section and the
//! authentication policy that consumes them, so cloned configurations never copy secret bytes.

use super::render::{
    deserialize_optional_secret, serialize_optional_secret, serialize_optional_string,
};
use crate::app::{
    auth::GlasswingCredentials,
    error::{ConfigError, Result},
    secret::SecretString,
};
use clap::ValueEnum;
use nanocodex::HarnessModel as Model;
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tact_subagents::is_access_gated;

/// Authentication method used by `tact`.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum AuthMode {
    /// Prefer a stored ChatGPT session, then fall back to an API key.
    #[default]
    Auto,
    /// Require a stored ChatGPT session.
    #[serde(rename = "chatgpt")]
    #[value(name = "chatgpt")]
    ChatGpt,
    /// Require an OpenAI API key from configuration or the environment.
    ApiKey,
}

/// Effective authentication configuration.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct AuthConfig {
    mode: AuthMode,
    file: PathBuf,
    /// Shared with [`OpenAiConfig`], which renders it.
    #[serde(skip)]
    api_key: Option<Arc<SecretString>>,
}

impl AuthConfig {
    pub(crate) const fn new(
        mode: AuthMode,
        file: PathBuf,
        api_key: Option<Arc<SecretString>>,
    ) -> Self {
        Self {
            mode,
            file,
            api_key,
        }
    }

    pub(crate) fn api_key(&self) -> Option<&Arc<SecretString>> {
        self.api_key.as_ref()
    }

    pub(crate) const fn mode(&self) -> AuthMode {
        self.mode
    }

    pub(crate) fn file(&self) -> &Path {
        &self.file
    }
}

/// Credentials for OpenAI Platform requests.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct OpenAiConfig {
    #[serde(
        deserialize_with = "deserialize_optional_secret",
        serialize_with = "serialize_optional_secret"
    )]
    pub(super) api_key: Option<Arc<SecretString>>,
}

/// Availability, credentials, and API routing for Claude models.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ClaudeConfig {
    enabled: bool,
    #[serde(
        deserialize_with = "deserialize_optional_secret",
        serialize_with = "serialize_optional_secret"
    )]
    pub(super) api_key: Option<Arc<SecretString>>,
    #[serde(serialize_with = "serialize_optional_string")]
    api_base_url: Option<String>,
    #[serde(serialize_with = "serialize_optional_string")]
    workspace_id: Option<String>,
    /// The short-lived Project Glasswing key that access-gated models such as Mythos require.
    /// Those models authenticate only with it, and every other Claude request uses `api_key`.
    #[serde(
        deserialize_with = "deserialize_optional_secret",
        serialize_with = "serialize_optional_secret"
    )]
    pub(super) glasswing_api_key: Option<Arc<SecretString>>,
    #[serde(serialize_with = "serialize_optional_string")]
    glasswing_workspace_id: Option<String>,
    /// The Glasswing credentials running Mythos clients read, shared across reloads.
    #[serde(skip)]
    glasswing: Arc<GlasswingCredentials>,
}

impl ClaudeConfig {
    pub(crate) fn api_key(&self) -> Option<&Arc<SecretString>> {
        self.api_key.as_ref()
    }

    pub(crate) fn workspace_id(&self) -> Option<&str> {
        trimmed(self.workspace_id.as_deref())
    }

    pub(crate) fn glasswing_workspace_id(&self) -> Option<&str> {
        trimmed(self.glasswing_workspace_id.as_deref())
    }

    /// The live Glasswing credentials that Mythos clients authenticate with.
    pub(crate) fn glasswing_credentials(&self) -> Arc<GlasswingCredentials> {
        Arc::clone(&self.glasswing)
    }

    /// Stores this configuration's Glasswing key and workspace in the live credentials, where
    /// running Mythos sessions read them on their next request.
    pub(crate) fn publish_glasswing(&self) {
        self.glasswing.replace(
            self.glasswing_api_key.as_ref().map(Arc::clone),
            self.glasswing_workspace_id(),
        );
    }

    /// Adopts `previous`'s live credentials so that publishing this configuration reaches the
    /// sessions that `previous` started.
    pub(super) fn share_glasswing(&mut self, previous: &Self) {
        self.glasswing = Arc::clone(&previous.glasswing);
    }

    pub(crate) fn ensure_enabled(&self) -> Result<()> {
        if !self.enabled {
            return Err(ConfigError::ClaudeDisabled.into());
        }
        Ok(())
    }

    pub(crate) const fn enabled(&self) -> bool {
        self.enabled
    }

    pub(crate) fn api_base_url(&self) -> Option<&str> {
        self.api_base_url
            .as_deref()
            .filter(|url| !url.trim().is_empty())
    }

    /// Whether this configuration may run `model`. Claude models need Claude enabled, and
    /// access-gated models also need a Glasswing key.
    pub(crate) fn ensure_model_enabled(
        &self,
        model: Model,
    ) -> std::result::Result<(), ConfigError> {
        if matches!(model, Model::Claude(_)) && !self.enabled {
            return Err(ConfigError::ClaudeDisabled);
        }
        if is_access_gated(model) && self.glasswing_api_key.is_none() {
            return Err(ConfigError::GlasswingApiKeyMissing(model));
        }
        Ok(())
    }
}

fn trimmed(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use crate::app::{
        config::{
            Config, ConfigOverrides, Environment, ReasoningEffort,
            test_support::{assert_table_fields, load_config, toml_value},
        },
        error::{ConfigError, Error},
    };
    use nanocodex::{ClaudeModel, HarnessModel as Model};
    use std::sync::Arc;
    use tempfile::tempdir;

    #[test]
    fn legacy_claude_authentication_configuration_is_rejected() {
        for legacy in [
            "auth = 'subscription'",
            "auth = 'api-key'",
            "subscription_store = 'private/auth'",
            "auth = 'subscription'\nsubscription_store = 'private/auth'",
        ] {
            let error = load_config(&format!("[claude]\nenabled = true\n{legacy}\n")).unwrap_err();
            assert!(matches!(error, Error::Config(ConfigError::Parse { .. })));
        }
    }

    #[test]
    fn claude_api_key_configuration_round_trips() {
        let config =
            load_config("[claude]\nenabled = true\napi_base_url = 'http://localhost:8080'\nworkspace_id = 'wrkspc_fixture'\nglasswing_workspace_id = 'wrkspc_glasswing'\n")
                .unwrap();
        let rendered = config.to_toml().unwrap();
        let restored = load_config(&rendered).unwrap();
        assert!(restored.claude().enabled());
        assert_eq!(restored.claude().workspace_id(), Some("wrkspc_fixture"));
        assert_eq!(
            restored.claude().glasswing_workspace_id(),
            Some("wrkspc_glasswing")
        );
        assert!(restored.claude().glasswing_api_key.is_none());
        assert_eq!(
            restored.claude().api_base_url(),
            Some("http://localhost:8080")
        );
        assert_table_fields(
            &toml_value(&rendered, &["claude"]),
            &[
                "enabled",
                "api_key",
                "api_base_url",
                "workspace_id",
                "glasswing_api_key",
                "glasswing_workspace_id",
            ],
        );
    }

    #[cfg(unix)]
    #[test]
    fn claude_config_key_is_shared_and_redacted_even_when_disabled() {
        let secret = "sk-ant-api-fixture-secret";
        let config = load_config(&format!("[claude]\napi_key = '{secret}'\n")).unwrap();
        let key = config.claude().api_key().unwrap();
        assert!(!config.claude().enabled());
        assert_eq!(key.expose_secret(), secret);
        let cloned = config.clone();
        assert!(Arc::ptr_eq(key, cloned.claude().api_key().unwrap()));

        let rendered = config.to_toml().unwrap();
        assert!(!rendered.contains(secret));
        assert!(!format!("{config:?}").contains(secret));
        let document: toml::Value = toml::from_str(&rendered).unwrap();
        assert_eq!(document["claude"]["api_key"].as_str(), Some("[REDACTED]"));
    }

    #[test]
    fn glasswing_key_is_shared_and_redacted() {
        let secret = "sk-ant-api03-glasswing-secret";
        let config = load_config(&format!(
            "[claude]\nenabled = true\nglasswing_api_key = '{secret}'\n"
        ))
        .unwrap();
        let key = config.claude().glasswing_api_key.as_ref().unwrap();
        assert_eq!(key.expose_secret(), secret);
        assert!(Arc::ptr_eq(
            key,
            config.clone().claude().glasswing_api_key.as_ref().unwrap()
        ));
        let rendered = config.to_toml().unwrap();
        assert!(!rendered.contains(secret));
        assert!(!format!("{config:?}").contains(secret));
        let document: toml::Value = toml::from_str(&rendered).unwrap();
        assert_eq!(
            document["claude"]["glasswing_api_key"].as_str(),
            Some("[REDACTED]")
        );
    }

    #[cfg(unix)]
    #[test]
    fn openai_config_key_is_shared_and_redacted_with_chatgpt_authentication() {
        let secret = "openai-config-secret-sentinel";
        let config = load_config(&format!(
            "[auth]\nmode = 'chatgpt'\n[openai]\napi_key = '{secret}'\n"
        ))
        .unwrap();
        let key = config.auth().api_key().unwrap();
        assert_eq!(key.expose_secret(), secret);
        let cloned = config.clone();
        assert!(Arc::ptr_eq(key, cloned.auth().api_key().unwrap()));
        assert!(Arc::ptr_eq(key, config.openai.api_key.as_ref().unwrap()));
        let rendered = config.to_toml().unwrap();
        assert!(!rendered.contains(secret));
        assert!(!format!("{config:?}").contains(secret));
        let document: toml::Value = toml::from_str(&rendered).unwrap();
        assert_eq!(document["openai"]["api_key"].as_str(), Some("[REDACTED]"));
        assert!(document["auth"].get("api_key").is_none());
    }

    #[test]
    fn blank_provider_config_keys_are_absent() {
        for provider in ["openai", "claude"] {
            for value in ["''", "'   '", "\" \\t\\n \""] {
                let config = load_config(&format!("[{provider}]\napi_key = {value}\n")).unwrap();
                let key = match provider {
                    "openai" => config.auth().api_key(),
                    _ => config.claude().api_key(),
                };
                assert!(key.is_none());
                let rendered: toml::Value = toml::from_str(&config.to_toml().unwrap()).unwrap();
                assert_eq!(rendered[provider]["api_key"].as_str(), Some(""));
            }
        }
    }

    #[test]
    fn claude_models_require_explicit_opt_in() {
        for (model, default_effort) in [
            ("sonnet-5.5", ReasoningEffort::High),
            ("haiku-5.5", ReasoningEffort::Medium),
            ("opus-5.5", ReasoningEffort::Medium),
            ("fable-5.1", ReasoningEffort::High),
            ("mythos-5.1", ReasoningEffort::High),
        ] {
            let agent = format!("[agent]\nmodel = '{model}'\n");
            assert!(matches!(
                load_config(&agent),
                Err(Error::Config(ConfigError::ClaudeDisabled))
            ));
            let enabled = format!("{agent}[claude]\nenabled = true\n");
            let gated = model == "mythos-5.1";
            assert_eq!(
                matches!(
                    load_config(&enabled),
                    Err(Error::Config(ConfigError::GlasswingApiKeyMissing(_)))
                ),
                gated
            );
            let config = load_config(&format!(
                "{enabled}glasswing_api_key = 'sk-ant-api03-glasswing'\n"
            ))
            .unwrap();
            assert!(config.claude().enabled());
            assert!(matches!(config.agent().model(), Model::Claude(_)));
            assert_eq!(config.agent().thinking(), default_effort);
            assert!(config.claude().api_base_url().is_none());
            let restored = load_config(&config.to_toml().unwrap()).unwrap();
            assert_eq!(restored.agent().model(), config.agent().model());
            assert!(restored.claude().api_base_url().is_none());
            assert!(restored.claude().workspace_id().is_none());
        }
    }

    #[test]
    fn startup_model_override_cannot_bypass_claude_opt_in() {
        let directory = tempdir().unwrap();
        let error = Config::load_with(
            ConfigOverrides {
                model: Some(Model::Claude(ClaudeModel::Opus55)),
                ..ConfigOverrides::default()
            },
            Environment {
                home: Some(directory.path().to_path_buf()),
                ..Environment::default()
            },
            directory.path(),
        )
        .unwrap_err();
        assert!(matches!(error, Error::Config(ConfigError::ClaudeDisabled)));
    }
}
