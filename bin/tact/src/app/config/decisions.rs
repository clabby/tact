//! The effective decisions section.

use crate::app::secret::SecretString;
use serde::Serialize;
use std::sync::Arc;

/// Effective configuration for the `decide` tool, which calls the OpenAI Decisions API.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct DecisionsConfig {
    enabled: bool,
    /// `openai.decisions_api_key`, shared with the `[openai]` section, which renders it.
    #[serde(skip)]
    api_key: Option<Arc<SecretString>>,
}

impl DecisionsConfig {
    pub(crate) const fn new(enabled: bool, api_key: Option<Arc<SecretString>>) -> Self {
        Self { enabled, api_key }
    }

    pub(crate) const fn enabled(&self) -> bool {
        self.enabled
    }

    pub(crate) fn api_key(&self) -> Option<&Arc<SecretString>> {
        self.api_key.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use crate::app::{
        config::test_support::load_config,
        error::{ConfigError, Error},
    };

    #[test]
    fn decisions_are_disabled_by_default() {
        for contents in ["", "[decisions]\n"] {
            let config = load_config(contents).unwrap();

            assert!(!config.decisions().enabled());
            let rendered: toml::Value = toml::from_str(&config.to_toml().unwrap()).unwrap();
            assert_eq!(rendered["decisions"]["enabled"].as_bool(), Some(false));
        }
    }

    #[test]
    fn decisions_enablement_round_trips() {
        for enabled in [false, true] {
            let config = load_config(&format!("[decisions]\nenabled = {enabled}\n")).unwrap();
            let reloaded = load_config(&config.to_toml().unwrap()).unwrap();
            assert_eq!(config.decisions().enabled(), enabled);
            assert_eq!(reloaded.decisions().enabled(), enabled);
        }
    }

    #[test]
    fn unknown_decisions_fields_are_rejected() {
        let error = load_config("[decisions]\napi_key = 'sk-test'\n").unwrap_err();
        assert!(matches!(error, Error::Config(ConfigError::Parse { .. })));
    }
}
