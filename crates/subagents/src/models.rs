use nanocode::{ClaudeModel, HarnessFamily, Model, ModelSelection, Thinking};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use std::{fmt, str::FromStr};
use thiserror::Error;

/// A model from one of the two built-in Nanocodex harnesses.
///
/// Nanocodex's [`ModelSelection`] names models from an open set of backends. Tact only runs the
/// built-in Codex and Claude harnesses, so it keeps this closed, copyable form and converts at
/// the SDK boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HarnessModel {
    /// A Codex (OpenAI Responses) model.
    Codex(Model),
    /// A native Claude model.
    Claude(ClaudeModel),
}

impl HarnessModel {
    /// The harness family that runs this model.
    pub fn family(self) -> HarnessFamily {
        match self {
            Self::Codex(_) => HarnessFamily::Codex,
            Self::Claude(_) => HarnessFamily::Claude,
        }
    }

    /// The provider model identifier.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Codex(model) => model.as_str(),
            Self::Claude(model) => model.as_str(),
        }
    }

    /// The SDK model selection for this model.
    pub fn selection(self) -> ModelSelection {
        match self {
            Self::Codex(model) => ModelSelection::Codex(model),
            Self::Claude(model) => ModelSelection::Claude(model),
        }
    }

    /// The model's default reasoning effort.
    pub fn default_thinking(self) -> Thinking {
        self.selection().default_thinking()
    }

    /// Whether the model accepts `thinking`.
    pub fn supports_thinking(self, thinking: Thinking) -> bool {
        self.selection().supports_thinking(thinking)
    }

    /// Whether the model offers an accelerated service tier.
    pub fn supports_fast_mode(self) -> bool {
        self.selection().supports_fast_mode()
    }
}

impl From<Model> for HarnessModel {
    fn from(model: Model) -> Self {
        Self::Codex(model)
    }
}

impl From<ClaudeModel> for HarnessModel {
    fn from(model: ClaudeModel) -> Self {
        Self::Claude(model)
    }
}

impl From<HarnessModel> for ModelSelection {
    fn from(model: HarnessModel) -> Self {
        model.selection()
    }
}

/// An SDK model selection outside the built-in Codex and Claude catalogs.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("unsupported harness model: {0}")]
pub struct UnknownHarnessModel(String);

impl TryFrom<&ModelSelection> for HarnessModel {
    type Error = UnknownHarnessModel;

    fn try_from(model: &ModelSelection) -> Result<Self, Self::Error> {
        model
            .as_codex()
            .map(Self::Codex)
            .or_else(|| model.as_claude().map(Self::Claude))
            .ok_or_else(|| UnknownHarnessModel(format!("{}/{}", model.backend, model.as_str())))
    }
}

impl Default for HarnessModel {
    fn default() -> Self {
        Self::Codex(Model::default())
    }
}

impl PartialEq<Model> for HarnessModel {
    fn eq(&self, model: &Model) -> bool {
        *self == Self::Codex(*model)
    }
}

impl fmt::Display for HarnessModel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for HarnessModel {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value
            .parse::<Model>()
            .map(Self::Codex)
            .or_else(|_| value.parse::<ClaudeModel>().map(Self::Claude))
    }
}

impl Serialize for HarnessModel {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for HarnessModel {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

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
