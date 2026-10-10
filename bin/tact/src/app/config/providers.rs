//! Model provider credentials, authentication policy, and Claude availability.
//!
//! API keys stored in the file are shared through `Arc` between the provider section and the
//! authentication policy that consumes them, so cloned configurations never copy secret bytes.

use super::render::{
    deserialize_optional_secret, serialize_optional_secret, serialize_optional_string,
};
use crate::app::{
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

/// Credentials for OpenAI Platform requests. `decisions_api_key` serves only the Decisions API,
/// for setups whose model requests use a ChatGPT subscription instead of an API key.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct OpenAiConfig {
    #[serde(
        deserialize_with = "deserialize_optional_secret",
        serialize_with = "serialize_optional_secret"
    )]
    pub(super) api_key: Option<Arc<SecretString>>,
    #[serde(
        deserialize_with = "deserialize_optional_secret",
        serialize_with = "serialize_optional_secret"
    )]
    pub(super) decisions_api_key: Option<Arc<SecretString>>,
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
}

impl ClaudeConfig {
    pub(crate) fn api_key(&self) -> Option<&Arc<SecretString>> {
        self.api_key.as_ref()
    }

    pub(crate) fn workspace_id(&self) -> Option<&str> {
        self.workspace_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
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

    pub(crate) fn ensure_model_enabled(&self, model: Model) -> Result<()> {
        if matches!(model, Model::Claude(_)) {
            self.ensure_enabled()?;
        }
        Ok(())
    }
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
            load_config("[claude]\nenabled = true\napi_base_url = 'http://localhost:8080'\nworkspace_id = 'wrkspc_fixture'\n")
                .unwrap();
        let rendered = config.to_toml().unwrap();
        let restored = load_config(&rendered).unwrap();
        assert!(restored.claude().enabled());
        assert_eq!(restored.claude().workspace_id(), Some("wrkspc_fixture"));
        assert_eq!(
            restored.claude().api_base_url(),
            Some("http://localhost:8080")
        );
        assert_table_fields(
            &toml_value(&rendered, &["claude"]),
            &["enabled", "api_key", "api_base_url", "workspace_id"],
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

    #[cfg(unix)]
    #[test]
    fn openai_config_key_is_shared_and_redacted_with_chatgpt_authentication() {
        let secret = "openai-config-secret-sentinel";
        let decisions_secret = "decisions-config-secret-sentinel";
        let config = load_config(&format!(
            "[auth]\nmode = 'chatgpt'\n[openai]\napi_key = '{secret}'\ndecisions_api_key = '{decisions_secret}'\n"
        ))
        .unwrap();
        let key = config.auth().api_key().unwrap();
        assert_eq!(key.expose_secret(), secret);
        let cloned = config.clone();
        assert!(Arc::ptr_eq(key, cloned.auth().api_key().unwrap()));
        assert!(Arc::ptr_eq(key, config.openai.api_key.as_ref().unwrap()));
        let decisions_key = config.decisions().api_key().unwrap();
        assert_eq!(decisions_key.expose_secret(), decisions_secret);
        assert!(Arc::ptr_eq(
            decisions_key,
            config.openai.decisions_api_key.as_ref().unwrap()
        ));
        let rendered = config.to_toml().unwrap();
        assert!(!rendered.contains(secret));
        assert!(!rendered.contains(decisions_secret));
        assert!(!format!("{config:?}").contains(secret));
        assert!(!format!("{config:?}").contains(decisions_secret));
        let document: toml::Value = toml::from_str(&rendered).unwrap();
        assert_eq!(document["openai"]["api_key"].as_str(), Some("[REDACTED]"));
        assert_eq!(
            document["openai"]["decisions_api_key"].as_str(),
            Some("[REDACTED]")
        );
        assert!(document["auth"].get("api_key").is_none());
        assert!(document["decisions"].get("api_key").is_none());
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
        ] {
            let agent = format!("[agent]\nmodel = '{model}'\n");
            assert!(matches!(
                load_config(&agent),
                Err(Error::Config(ConfigError::ClaudeDisabled))
            ));
            let config = load_config(&format!("{agent}[claude]\nenabled = true\n")).unwrap();
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
