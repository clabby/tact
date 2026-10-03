use nanocodex::{ClaudeModel, HarnessModel, Model};
use serde::{Deserialize, Deserializer, de};

/// Models enabled by Tact's provider policy, in picker order.
pub const SUPPORTED_MODELS: [HarnessModel; 6] = [
    HarnessModel::Codex(Model::Luna),
    HarnessModel::Codex(Model::Sol),
    HarnessModel::Codex(Model::Astra),
    HarnessModel::Claude(ClaudeModel::Sonnet55),
    HarnessModel::Claude(ClaudeModel::Opus55),
    HarnessModel::Claude(ClaudeModel::Fable51),
];

/// Parses Tact's aliases and rejects models outside its supported roster.
pub fn parse_model(value: &str) -> Result<HarnessModel, String> {
    let value = match value {
        "sonnet-5.5" => ClaudeModel::Sonnet55.as_str(),
        "opus-5.5" => ClaudeModel::Opus55.as_str(),
        "fable-5.1" => ClaudeModel::Fable51.as_str(),
        value => value,
    };
    value
        .parse::<HarnessModel>()
        .ok()
        .filter(|model| SUPPORTED_MODELS.contains(model))
        .ok_or_else(|| format!("unsupported Tact model: {value}"))
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
            "haiku",
            "kimi-k3",
            "gpt-6-sol",
            "unknown",
        ] {
            assert!(parse_model(unsupported).is_err());
        }
    }
}
