//! Model preferences that front-ends can change at runtime and persist to the configuration file.

use clap::ValueEnum;
use nanocodex::{HarnessModel as Model, Thinking, oai::transport::ResponsesTransport};
use serde::{Deserialize, Serialize};
pub(crate) use tact_subagents::Speed;

/// The number of concurrently live subagents allowed when neither the CLI nor the file sets one.
pub(crate) const DEFAULT_MAX_SUBAGENTS: usize = 32;

/// Reasoning effort used by the model.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ReasoningEffort {
    #[default]
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ReasoningEffort {
    pub(crate) const ALL: [Self; 5] = [Self::Low, Self::Medium, Self::High, Self::Xhigh, Self::Max];

    /// The effort a model runs at when no effort is configured. Every model's catalog default
    /// is one of Tact's selectable efforts.
    pub(crate) fn default_for(model: Model) -> Self {
        Self::ALL
            .into_iter()
            .find(|effort| Thinking::from(*effort) == model.default_thinking())
            .expect("model default effort must be supported by Tact")
    }

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }

    pub(crate) const fn index(self) -> usize {
        match self {
            Self::Low => 0,
            Self::Medium => 1,
            Self::High => 2,
            Self::Xhigh => 3,
            Self::Max => 4,
        }
    }
}

impl From<ReasoningEffort> for Thinking {
    fn from(effort: ReasoningEffort) -> Self {
        match effort {
            ReasoningEffort::Low => Self::Low,
            ReasoningEffort::Medium => Self::Medium,
            ReasoningEffort::High => Self::High,
            ReasoningEffort::Xhigh => Self::Xhigh,
            ReasoningEffort::Max => Self::Max,
        }
    }
}

/// Reasoning execution mode used by the model.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ReasoningMode {
    #[default]
    Standard,
    Pro,
}

impl ReasoningMode {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Pro => "pro",
        }
    }
}

impl From<ReasoningMode> for nanocodex::ReasoningMode {
    fn from(mode: ReasoningMode) -> Self {
        match mode {
            ReasoningMode::Standard => Self::Standard,
            ReasoningMode::Pro => Self::Pro,
        }
    }
}

/// Responses API transport policy.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Transport {
    #[default]
    Websocket,
    Https,
}

impl From<Transport> for ResponsesTransport {
    fn from(transport: Transport) -> Self {
        match transport {
            Transport::Websocket => Self::WebSocket,
            Transport::Https => Self::Https,
        }
    }
}
