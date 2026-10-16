use nanocodex::{ClaudeModel, HarnessModel, Model};
use serde::{Deserialize, Deserializer, de};
use thiserror::Error;

/// Models enabled by Tact's provider policy, in picker order.
pub const SUPPORTED_MODELS: [HarnessModel; 8] = [
    HarnessModel::Codex(Model::Luna),
    HarnessModel::Codex(Model::Sol),
    HarnessModel::Codex(Model::Astra),
    HarnessModel::Claude(ClaudeModel::Haiku55),
    HarnessModel::Claude(ClaudeModel::Sonnet55),
    HarnessModel::Claude(ClaudeModel::Opus55),
    HarnessModel::Claude(ClaudeModel::Fable51),
    HarnessModel::Claude(ClaudeModel::Mythos51),
];

/// The short name agents and users type for each model, in [`SUPPORTED_MODELS`] order.
const ALIASES: [&str; SUPPORTED_MODELS.len()] = [
    "luna",
    "sol",
    "astra",
    "haiku-5.5",
    "sonnet-5.5",
    "opus-5.5",
    "fable-5.1",
    "mythos-5.1",
];

/// Whether Anthropic serves `model` only to accounts it has granted access. Hosts offer such a
/// model only after the account's model list confirms the grant.
pub const fn is_access_gated(model: HarnessModel) -> bool {
    matches!(model, HarnessModel::Claude(ClaudeModel::Mythos51))
}

/// The short name of a supported model.
pub(crate) fn alias(model: HarnessModel) -> Option<&'static str> {
    SUPPORTED_MODELS
        .iter()
        .position(|supported| *supported == model)
        .map(|index| ALIASES[index])
}

/// A model name outside [`SUPPORTED_MODELS`].
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("unsupported Tact model: {0}")]
pub struct UnsupportedModel(String);

/// Parses Tact's aliases and rejects models outside its supported roster.
///
/// # Errors
///
/// Returns [`UnsupportedModel`] for unknown names and for upstream models Tact does not offer.
pub fn parse_model(value: &str) -> Result<HarnessModel, UnsupportedModel> {
    if let Some(index) = ALIASES.iter().position(|alias| *alias == value) {
        return Ok(SUPPORTED_MODELS[index]);
    }
    value
        .parse::<HarnessModel>()
        .ok()
        .filter(|model| SUPPORTED_MODELS.contains(model))
        .ok_or_else(|| UnsupportedModel(value.to_owned()))
}

pub(crate) fn deserialize_model<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<HarnessModel, D::Error> {
    parse_model(&String::deserialize(deserializer)?).map_err(de::Error::custom)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tact_aliases_and_rejects_other_upstream_models() {
        for (name, model) in [
            "luna",
            "sol",
            "astra",
            "haiku-5.5",
            "sonnet-5.5",
            "opus-5.5",
            "fable-5.1",
            "mythos-5.1",
        ]
        .into_iter()
        .zip(SUPPORTED_MODELS)
        {
            assert_eq!(parse_model(name), Ok(model));
            assert_eq!(alias(model), Some(name));
            assert_eq!(parse_model(model.as_str()), Ok(model));
        }
        for unsupported in [
            "claude-sonnet-5",
            "claude-haiku-4-5",
            "kimi-k3",
            "gpt-6-sol",
            "unknown",
        ] {
            assert!(parse_model(unsupported).is_err());
        }
    }
}
