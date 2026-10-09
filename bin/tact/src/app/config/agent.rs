//! The effective agent section: model selection, reasoning preferences, and request routing.

use super::{
    ConfigOverrides,
    file::AgentConfigFile,
    render::{non_empty, serialize_optional_string},
    settings::{DEFAULT_MAX_SUBAGENTS, ReasoningEffort, ReasoningMode, Speed, Transport},
};
use nanocodex::{HarnessModel as Model, Model as CodexModel};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Effective Nanocodex configuration.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct AgentConfig {
    pub(super) workspace: PathBuf,
    model: Model,
    pub(super) thinking: ReasoningEffort,
    pub(super) reasoning_mode: ReasoningMode,
    pub(super) speed: Speed,
    pub(super) max_subagents: usize,
    #[serde(serialize_with = "serialize_optional_string")]
    instructions: Option<String>,
    #[serde(serialize_with = "serialize_optional_string")]
    append_instructions: Option<String>,
    web_search: bool,
    image_generation: bool,
    #[serde(serialize_with = "serialize_optional_string")]
    websocket_url: Option<String>,
    #[serde(serialize_with = "serialize_optional_string")]
    api_base_url: Option<String>,
    transport: Transport,
    #[serde(serialize_with = "serialize_optional_string")]
    completion_hook: Option<String>,
}

impl AgentConfig {
    /// Resolves each setting from the CLI override, then the file, then its default. The
    /// workspace is resolved by the caller because it depends on path bases outside this section.
    pub(super) fn resolve(
        overrides: &ConfigOverrides,
        file: AgentConfigFile,
        workspace: PathBuf,
    ) -> Self {
        let model = overrides
            .model
            .or(file.model)
            .unwrap_or(Model::Codex(CodexModel::Sol));
        let legacy_speed = if file.fast_mode == Some(true) {
            Speed::Fast
        } else {
            Speed::Standard
        };
        Self {
            workspace,
            model,
            thinking: overrides
                .thinking
                .or(file.thinking)
                .unwrap_or_else(|| ReasoningEffort::default_for(model)),
            reasoning_mode: overrides
                .reasoning_mode
                .or(file.reasoning_mode)
                .unwrap_or_default(),
            speed: file.speed.unwrap_or(legacy_speed),
            max_subagents: overrides
                .max_subagents
                .or(file.max_subagents)
                .unwrap_or(DEFAULT_MAX_SUBAGENTS),
            instructions: non_empty(overrides.instructions.clone().or(file.instructions)),
            append_instructions: non_empty(
                overrides
                    .append_instructions
                    .clone()
                    .or(file.append_instructions),
            ),
            web_search: overrides.web_search.or(file.web_search).unwrap_or(true),
            image_generation: overrides
                .image_generation
                .or(file.image_generation)
                .unwrap_or(true),
            websocket_url: non_empty(overrides.websocket_url.clone().or(file.websocket_url)),
            api_base_url: non_empty(overrides.api_base_url.clone().or(file.api_base_url)),
            transport: overrides.transport.or(file.transport).unwrap_or_default(),
            completion_hook: non_empty(file.completion_hook),
        }
    }

    pub(crate) fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub(crate) const fn model(&self) -> Model {
        self.model
    }

    pub(crate) const fn thinking(&self) -> ReasoningEffort {
        self.thinking
    }

    pub(crate) const fn reasoning_mode(&self) -> ReasoningMode {
        self.reasoning_mode
    }

    pub(crate) const fn speed(&self) -> Speed {
        self.speed
    }

    pub(crate) const fn max_subagents(&self) -> usize {
        self.max_subagents
    }

    pub(crate) fn instructions(&self) -> Option<&str> {
        self.instructions.as_deref()
    }

    pub(crate) fn append_instructions(&self) -> Option<&str> {
        self.append_instructions.as_deref()
    }

    pub(crate) const fn web_search(&self) -> bool {
        self.web_search
    }

    pub(crate) const fn image_generation(&self) -> bool {
        self.image_generation
    }

    pub(crate) fn websocket_url(&self) -> Option<&str> {
        self.websocket_url.as_deref()
    }

    pub(crate) fn api_base_url(&self) -> Option<&str> {
        self.api_base_url.as_deref()
    }

    pub(crate) const fn transport(&self) -> Transport {
        self.transport
    }

    pub(crate) fn completion_hook(&self) -> Option<&str> {
        self.completion_hook.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use crate::app::{
        config::{
            ReasoningEffort, ReasoningMode, Speed, Transport,
            test_support::{load_config, load_config_at},
        },
        error::{ConfigError, Error},
    };
    use nanocodex::{HarnessModel as Model, Model as CodexModel};
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn speed_preference_migrates_legacy_values_without_clamping() {
        for (fields, expected) in [
            ("", Speed::Standard),
            ("fast_mode = false", Speed::Standard),
            ("fast_mode = true", Speed::Fast),
            ("speed = 'standard'\nfast_mode = true", Speed::Standard),
            ("speed = 'fast'\nfast_mode = false", Speed::Fast),
            ("speed = 'ultrafast'\nfast_mode = false", Speed::Ultrafast),
        ] {
            let config = load_config(&format!("[agent]\nmodel = 'luna'\n{fields}\n")).unwrap();
            assert_eq!(config.agent().speed(), expected);
            let rendered: toml::Value = toml::from_str(&config.to_toml().unwrap()).unwrap();
            assert_eq!(rendered["agent"]["speed"].as_str(), Some(expected.as_str()));
            assert!(rendered["agent"].get("fast_mode").is_none());
        }
        assert!(load_config("[agent]\nspeed = 'instant'\n").is_err());
    }

    #[test]
    fn terra_is_not_accepted_from_configuration() {
        let error = load_config("[agent]\nmodel = \"terra\"\n").unwrap_err();

        assert!(matches!(error, Error::Config(ConfigError::Parse { .. })));
        assert!(!error.to_string().contains("gpt-5.6-terra"));
    }

    #[test]
    fn model_defaults_choose_the_catalog_effort() {
        for (model, expected) in [
            ("sol", ReasoningEffort::Low),
            ("luna", ReasoningEffort::Medium),
            ("astra", ReasoningEffort::Low),
            ("haiku-5.5", ReasoningEffort::Medium),
            ("sonnet-5.5", ReasoningEffort::High),
            ("opus-5.5", ReasoningEffort::Medium),
            ("fable-5.1", ReasoningEffort::High),
        ] {
            let config = load_config(&format!(
                "[claude]\nenabled = true\n[agent]\nmodel = \"{model}\"\n"
            ))
            .unwrap();
            assert_eq!(config.agent().thinking(), expected, "model {model}");
        }

        let configured = load_config("[agent]\nmodel = \"sol\"\nthinking = \"medium\"\n").unwrap();
        assert_eq!(configured.agent().thinking(), ReasoningEffort::Medium);
    }

    #[test]
    fn completion_hook_can_be_configured() {
        let config = load_config("[agent]\ncompletion_hook = \"notify-send done\"\n").unwrap();

        assert_eq!(config.agent().completion_hook(), Some("notify-send done"));
        let rendered: toml::Value = toml::from_str(&config.to_toml().unwrap()).unwrap();
        assert_eq!(
            rendered["agent"]["completion_hook"].as_str(),
            Some("notify-send done")
        );
    }

    #[test]
    fn agent_configuration_is_loaded() {
        let directory = tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        fs::write(
            &config_path,
            "[agent]\nworkspace = \"workspace\"\nmodel = \"astra\"\nthinking = \"xhigh\"\nreasoning_mode = \"pro\"\nfast_mode = true\nmax_subagents = 7\n\
             instructions = \"Be concise.\"\nappend_instructions = \"Use project conventions.\"\n\
             web_search = false\nimage_generation = false\n\
             websocket_url = \"wss://example.com/responses\"\n\
             api_base_url = \"https://example.com/v1\"\n\
             transport = \"https\"\n",
        )
        .unwrap();

        let config = load_config_at(config_path, directory.path()).unwrap();

        assert_eq!(
            config.agent().workspace(),
            directory.path().join("workspace")
        );
        assert_eq!(config.agent().model(), Model::Codex(CodexModel::Astra));
        assert_eq!(config.agent().thinking(), ReasoningEffort::Xhigh);
        assert_eq!(config.agent().reasoning_mode(), ReasoningMode::Pro);
        assert_eq!(config.agent().speed(), Speed::Fast);
        assert_eq!(config.agent().max_subagents(), 7);
        assert_eq!(config.agent().instructions(), Some("Be concise."));
        assert_eq!(
            config.agent().append_instructions(),
            Some("Use project conventions.")
        );
        assert!(!config.agent().web_search());
        assert!(!config.agent().image_generation());
        assert_eq!(
            config.agent().websocket_url(),
            Some("wss://example.com/responses")
        );
        assert_eq!(
            config.agent().api_base_url(),
            Some("https://example.com/v1")
        );
        assert_eq!(config.agent().transport(), Transport::Https);
    }
}
