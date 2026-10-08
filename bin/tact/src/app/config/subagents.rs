//! The effective subagents section.

use super::file::SubagentsConfigFile;
use serde::Serialize;

/// Effective subagent configuration.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct SubagentsConfig {
    enabled: bool,
}

impl SubagentsConfig {
    /// Subagents are enabled unless the file disables them.
    pub(super) fn new(file: SubagentsConfigFile) -> Self {
        Self {
            enabled: file.enabled.unwrap_or(true),
        }
    }

    pub(crate) const fn enabled(&self) -> bool {
        self.enabled
    }
}

#[cfg(test)]
mod tests {
    use crate::app::{
        config::test_support::load_config,
        error::{ConfigError, Error},
    };

    #[test]
    fn subagents_are_enabled_by_default() {
        for contents in ["", "[subagents]\n"] {
            let config = load_config(contents).unwrap();

            assert!(config.subagents().enabled());
            let rendered: toml::Value = toml::from_str(&config.to_toml().unwrap()).unwrap();
            assert_eq!(rendered["subagents"]["enabled"].as_bool(), Some(true));
        }
    }

    #[test]
    fn subagent_enablement_round_trips() {
        for enabled in [false, true] {
            let config = load_config(&format!("[subagents]\nenabled = {enabled}\n")).unwrap();
            let reloaded = load_config(&config.to_toml().unwrap()).unwrap();
            assert_eq!(config.subagents().enabled(), enabled);
            assert_eq!(reloaded.subagents().enabled(), enabled);
        }
    }

    #[test]
    fn unknown_subagent_fields_are_rejected() {
        for field in ["allow_luna", "allow_sol", "limit"] {
            let error = load_config(&format!("[subagents]\n{field} = false\n")).unwrap_err();
            assert!(matches!(error, Error::Config(ConfigError::Parse { .. })));
        }
    }
}
