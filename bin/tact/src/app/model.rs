//! Tact's supported model roster and input parsing.

use nanocodex::{ClaudeModel, HarnessModel as Model, Model as CodexModel};
use serde::{Deserialize, Deserializer, de};
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
        Model::Claude(ClaudeModel::Opus55) => "Opus 5.5",
        Model::Claude(ClaudeModel::Fable51) => "Fable 5.1",
        _ => model.as_str(),
    }
}

#[cfg(test)]
mod tests {
    use super::parse;
    use nanocodex::{ClaudeModel, HarnessModel as Model, Model as CodexModel};

    #[test]
    fn accepts_current_model_ids_and_short_names() {
        for (value, expected) in [
            ("gpt-6-luna", Model::Codex(CodexModel::Luna)),
            ("luna", Model::Codex(CodexModel::Luna)),
            ("gpt-6.1-sol", Model::Codex(CodexModel::Sol)),
            ("sol", Model::Codex(CodexModel::Sol)),
            ("gpt-6-astra", Model::Codex(CodexModel::Astra)),
            ("astra", Model::Codex(CodexModel::Astra)),
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
