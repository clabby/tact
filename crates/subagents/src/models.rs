use nanocodex::{ClaudeModel, HarnessModel, Model};
use serde::{Deserialize, Deserializer, de};
use thiserror::Error;

/// Models enabled by Tact's provider policy, in picker order.
pub const SUPPORTED_MODELS: [HarnessModel; 7] = [
    HarnessModel::Codex(Model::Luna),
    HarnessModel::Codex(Model::Sol),
    HarnessModel::Codex(Model::Astra),
    HarnessModel::Claude(ClaudeModel::Haiku55),
    HarnessModel::Claude(ClaudeModel::Sonnet55),
    HarnessModel::Claude(ClaudeModel::Opus55),
    HarnessModel::Claude(ClaudeModel::Fable51),
];

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
    let value = match value {
        "haiku-5.5" => ClaudeModel::Haiku55.as_str(),
        "sonnet-5.5" => ClaudeModel::Sonnet55.as_str(),
        "opus-5.5" => ClaudeModel::Opus55.as_str(),
        "fable-5.1" => ClaudeModel::Fable51.as_str(),
        value => value,
    };
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
        for (alias, model) in [
            "luna",
            "sol",
            "astra",
            "haiku-5.5",
            "sonnet-5.5",
            "opus-5.5",
            "fable-5.1",
        ]
        .into_iter()
        .zip(SUPPORTED_MODELS)
        {
            assert_eq!(parse_model(alias), Ok(model));
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
