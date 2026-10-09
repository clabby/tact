use nanocodex::{HarnessModel as Model, oai::pricing::ServiceTier};
use serde::{Deserialize, Serialize};

/// Preferred processing speed, inherited by newly created child sessions.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Speed {
    /// Standard processing.
    #[default]
    Standard,
    /// Accelerated processing where the model supports it.
    Fast,
    /// The fastest processing offered by the selected model.
    Ultrafast,
}

impl Speed {
    /// Speed preferences in increasing order.
    pub const ALL: [Self; 3] = [Self::Standard, Self::Fast, Self::Ultrafast];

    /// The configuration and display name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Fast => "fast",
            Self::Ultrafast => "ultrafast",
        }
    }

    /// Resolves a preference to the fastest supported speed no higher than requested.
    pub const fn for_model(self, model: Model) -> Self {
        match (self, model) {
            (Self::Standard, _) => Self::Standard,
            (Self::Ultrafast, Model::Codex(model))
                if matches!(
                    ServiceTier::Ultrafast.effective_for_model(model),
                    ServiceTier::Ultrafast
                ) =>
            {
                Self::Ultrafast
            }
            _ if model.supports_fast_mode() => Self::Fast,
            _ => Self::Standard,
        }
    }
}

impl From<Speed> for ServiceTier {
    fn from(speed: Speed) -> Self {
        match speed {
            Speed::Standard => Self::Standard,
            Speed::Fast => Self::Fast,
            Speed::Ultrafast => Self::Ultrafast,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Speed;
    use nanocodex::{ClaudeModel, HarnessModel as Model, Model as CodexModel};

    #[test]
    fn speed_preferences_fall_back_to_the_models_fastest_supported_tier() {
        for (model, fast, ultrafast) in [
            (
                Model::Codex(CodexModel::Astra),
                Speed::Fast,
                Speed::Ultrafast,
            ),
            (Model::Codex(CodexModel::Sol), Speed::Fast, Speed::Ultrafast),
            (Model::Codex(CodexModel::Luna), Speed::Fast, Speed::Fast),
            (Model::Claude(ClaudeModel::Opus55), Speed::Fast, Speed::Fast),
            (
                Model::Claude(ClaudeModel::Haiku55),
                Speed::Standard,
                Speed::Standard,
            ),
            (
                Model::Claude(ClaudeModel::Sonnet55),
                Speed::Standard,
                Speed::Standard,
            ),
            (
                Model::Claude(ClaudeModel::Fable51),
                Speed::Standard,
                Speed::Standard,
            ),
        ] {
            assert_eq!(Speed::Standard.for_model(model), Speed::Standard);
            assert_eq!(Speed::Fast.for_model(model), fast);
            assert_eq!(Speed::Ultrafast.for_model(model), ultrafast);
        }
    }
}
