//! Tact's supported model roster and input parsing.

use crate::app::config::{ReasoningEffort, ReasoningMode, Speed};
use clap::ValueEnum;
use nanocodex::{
    ClaudeModel, HarnessModel as Model, Model as CodexModel, ReasoningMode as NativeReasoningMode,
};
use serde::{Deserialize, Deserializer, Serialize, de};
use tact_subagents::{AgentContext, SUPPORTED_MODELS};

/// The number of OpenAI models in [`SUPPORTED_MODELS`]. The roster lists every OpenAI model
/// before any Claude model, so the OpenAI-only roster is a prefix of the full one; the
/// assertion below rejects a roster that breaks this ordering at compile time.
const OPENAI_MODEL_COUNT: usize = {
    let mut count = 0;
    while count < SUPPORTED_MODELS.len() && matches!(SUPPORTED_MODELS[count], Model::Codex(_)) {
        count += 1;
    }
    let mut rest = count;
    while rest < SUPPORTED_MODELS.len() {
        assert!(
            !matches!(SUPPORTED_MODELS[rest], Model::Codex(_)),
            "OpenAI models must precede Claude models in SUPPORTED_MODELS"
        );
        rest += 1;
    }
    count
};

/// The selectable models: the full roster when Claude is enabled, otherwise only OpenAI models.
pub(crate) fn available(claude_enabled: bool) -> &'static [Model] {
    if claude_enabled {
        &SUPPORTED_MODELS
    } else {
        &SUPPORTED_MODELS[..OPENAI_MODEL_COUNT]
    }
}

pub(crate) fn parse(value: &str) -> Result<Model, String> {
    tact_subagents::parse_model(value).map_err(|error| error.to_string())
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
    match AgentContext::resolve_reasoning_mode(model, NativeReasoningMode::Pro) {
        NativeReasoningMode::Pro => &[ReasoningMode::Standard, ReasoningMode::Pro],
        NativeReasoningMode::Standard => &[ReasoningMode::Standard],
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
        Model::Claude(ClaudeModel::Haiku55) => "Haiku 5.5",
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
    use tact_subagents::SUPPORTED_MODELS;

    #[test]
    fn catalog_lists_the_enabled_roster_with_its_setting_couplings() {
        let openai_only = ModelCatalog::new(false);
        assert!(
            openai_only
                .models
                .iter()
                .all(|model| model.provider == Provider::Openai)
        );
        assert_eq!(
            openai_only.models.len(),
            SUPPORTED_MODELS
                .iter()
                .filter(|model| matches!(model, Model::Codex(_)))
                .count()
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

        let sol = catalog
            .models
            .iter()
            .find(|model| model.id == Model::Codex(CodexModel::Sol).as_str())
            .unwrap();
        assert_eq!(sol.effective_speeds, Speed::ALL);

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
            ("haiku", Model::Claude(ClaudeModel::Haiku55)),
            ("haiku-5.5", Model::Claude(ClaudeModel::Haiku55)),
            ("claude-haiku-5-5", Model::Claude(ClaudeModel::Haiku55)),
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
