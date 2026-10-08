//! Tact's supported model roster and input parsing.

use crate::app::config::{ReasoningEffort, ReasoningMode, Speed};
use clap::ValueEnum;
use nanocodex::{ClaudeModel, HarnessModel as Model, Model as CodexModel};
use serde::{Deserialize, Deserializer, Serialize, de};
use tact_subagents::SUPPORTED_MODELS;

pub(crate) fn available(claude_enabled: bool) -> &'static [Model] {
    if claude_enabled {
        &SUPPORTED_MODELS
    } else {
        &SUPPORTED_MODELS[..3]
    }
}

pub(crate) fn parse(value: &str) -> Result<Model, String> {
    tact_subagents::parse_model(value)
}

/// The selectable models and the settings each one accepts. Every front-end offers exactly
/// these choices; the event loop enforces the same couplings when a setting is applied.
#[derive(Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ModelCatalog {
    pub(crate) models: Vec<ModelOption>,
    pub(crate) efforts: &'static [ReasoningEffort],
    /// Speed preferences in increasing order.
    pub(crate) speeds: &'static [Speed],
}

#[derive(Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ModelOption {
    /// The canonical identifier accepted by [`parse`].
    pub(crate) id: &'static str,
    pub(crate) label: &'static str,
    pub(crate) provider: Provider,
    pub(crate) reasoning_modes: &'static [ReasoningMode],
    /// The speed each preference in [`ModelCatalog::speeds`] runs at on this model.
    pub(crate) effective_speeds: Vec<Speed>,
    /// Effort cannot change once the session's first turn has started.
    pub(crate) effort_fixed_after_start: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Provider {
    Openai,
    Anthropic,
}

impl ModelCatalog {
    pub(crate) fn new(claude_enabled: bool) -> Self {
        Self {
            models: available(claude_enabled)
                .iter()
                .map(|&model| ModelOption::new(model))
                .collect(),
            efforts: ReasoningEffort::value_variants(),
            speeds: &Speed::ALL,
        }
    }
}

impl ModelOption {
    fn new(model: Model) -> Self {
        let claude = matches!(model, Model::Claude(_));
        Self {
            id: model.as_str(),
            label: name(model),
            provider: if claude {
                Provider::Anthropic
            } else {
                Provider::Openai
            },
            reasoning_modes: reasoning_modes(model),
            effective_speeds: Speed::ALL.map(|speed| speed.for_model(model)).to_vec(),
            effort_fixed_after_start: claude,
        }
    }
}

/// The reasoning modes a model accepts. Pro reasoning is an OpenAI capability.
pub(crate) const fn reasoning_modes(model: Model) -> &'static [ReasoningMode] {
    match model {
        Model::Codex(_) => &[ReasoningMode::Standard, ReasoningMode::Pro],
        _ => &[ReasoningMode::Standard],
    }
}

pub(crate) fn deserialize_optional<'de, D>(deserializer: D) -> Result<Option<Model>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer)?
        .map(|value| parse(&value).map_err(de::Error::custom))
        .transpose()
}

pub(crate) const fn name(model: Model) -> &'static str {
    match model {
        Model::Codex(CodexModel::Luna) => "Luna",
        Model::Codex(CodexModel::Sol) => "Sol",
        Model::Codex(CodexModel::Astra) => "Astra",
        Model::Claude(ClaudeModel::Sonnet55) => "Sonnet 5.5",
        Model::Claude(ClaudeModel::Opus55) => "Opus 5.5",
        Model::Claude(ClaudeModel::Fable51) => "Fable 5.1",
        _ => model.as_str(),
    }
}

#[cfg(test)]
mod tests {
    use super::{ModelCatalog, Provider, parse};
    use crate::app::config::{ReasoningMode, Speed};
    use nanocodex::{ClaudeModel, HarnessModel as Model, Model as CodexModel};

    #[test]
    fn catalog_lists_the_enabled_roster_with_its_setting_couplings() {
        let openai_only = ModelCatalog::new(false);
        assert!(
            openai_only
                .models
                .iter()
                .all(|model| model.provider == Provider::Openai)
        );
        let catalog = ModelCatalog::new(true);
        assert_eq!(catalog.speeds, Speed::ALL);
        assert_eq!(catalog.efforts.len(), 5);

        let astra = catalog
            .models
            .iter()
            .find(|model| model.id == Model::Codex(CodexModel::Astra).as_str())
            .unwrap();
        assert_eq!(
            astra.reasoning_modes,
            [ReasoningMode::Standard, ReasoningMode::Pro]
        );
        assert_eq!(astra.effective_speeds, Speed::ALL);
        assert!(!astra.effort_fixed_after_start);

        let sonnet = catalog
            .models
            .iter()
            .find(|model| model.id == Model::Claude(ClaudeModel::Sonnet55).as_str())
            .unwrap();
        assert_eq!(sonnet.label, "Sonnet 5.5");
        assert_eq!(sonnet.provider, Provider::Anthropic);
        assert_eq!(sonnet.reasoning_modes, [ReasoningMode::Standard]);
        assert_eq!(sonnet.effective_speeds, [Speed::Standard; 3]);
        assert!(sonnet.effort_fixed_after_start);
    }

    #[test]
    fn accepts_current_model_ids_and_short_names() {
        for (value, expected) in [
            ("gpt-6-luna", Model::Codex(CodexModel::Luna)),
            ("luna", Model::Codex(CodexModel::Luna)),
            ("gpt-6.1-sol", Model::Codex(CodexModel::Sol)),
            ("sol", Model::Codex(CodexModel::Sol)),
            ("gpt-6-astra", Model::Codex(CodexModel::Astra)),
            ("astra", Model::Codex(CodexModel::Astra)),
            ("sonnet-5.5", Model::Claude(ClaudeModel::Sonnet55)),
            ("claude-sonnet-5-5", Model::Claude(ClaudeModel::Sonnet55)),
            ("opus-5.5", Model::Claude(ClaudeModel::Opus55)),
            ("claude-opus-5-5", Model::Claude(ClaudeModel::Opus55)),
            ("fable-5.1", Model::Claude(ClaudeModel::Fable51)),
            ("claude-fable-5-1", Model::Claude(ClaudeModel::Fable51)),
        ] {
            assert_eq!(parse(value), Ok(expected));
        }
    }

    #[test]
    fn rejects_retired_model_ids() {
        for value in [
            "gpt-6-sol",
            "gpt-5.6-luna",
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "terra",
        ] {
            assert!(parse(value).is_err(), "retired model {value} was accepted");
        }
    }
}
