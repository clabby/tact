use nanocodex::AgentEvent;
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_RUNTIME_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub(crate) struct AgentId(u64);

impl AgentId {
    #[cfg(test)]
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }

    pub(super) fn next(counter: &mut u64) -> Self {
        *counter = counter.saturating_add(1);
        Self(*counter)
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AgentOrigin {
    Spawn,
    Fork,
}

impl AgentOrigin {
    pub(super) const fn tool_name(self) -> &'static str {
        match self {
            Self::Spawn => "spawn_agent",
            Self::Fork => "fork_agent",
        }
    }

    pub(super) const fn result_name(self) -> &'static str {
        match self {
            Self::Spawn => "independent",
            Self::Fork => "fork",
        }
    }

    pub(super) const fn description(self) -> &'static str {
        match self {
            Self::Spawn => {
                "Starts a reusable clean-room subagent without inherited conversation history and immediately returns its ID."
            }
            Self::Fork => {
                "Starts a reusable subagent from the latest safe model boundary and immediately returns its ID."
            }
        }
    }

    pub(super) fn prompt(self, id: AgentId, task: &str) -> String {
        let context = match self {
            Self::Spawn => "You have no inherited conversation context.",
            Self::Fork => "Use the inherited conversation only as context for this delegation.",
        };
        format!(
            "Act as a specialist subagent. {context} Work only on the delegated task and return a \
             compact, evidence-backed report to the parent agent. Your agent ID is {id}. The \
             runtime automatically places agents you delegate beneath you in the task tree.\n\n\
             Delegated task:\n{task}"
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum AgentStatus {
    Pending,
    Running,
    Completed { report: String },
    Interrupted,
    Failed { error: String },
    Closing,
    Closed,
}

impl AgentStatus {
    pub(crate) const fn is_active(&self) -> bool {
        matches!(self, Self::Pending | Self::Running | Self::Closing)
    }

    pub(super) const fn is_wait_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed { .. } | Self::Interrupted | Self::Failed { .. } | Self::Closed
        )
    }

    pub(super) const fn can_start_turn(&self) -> bool {
        matches!(
            self,
            Self::Pending | Self::Completed { .. } | Self::Interrupted | Self::Failed { .. }
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AgentDescriptor {
    pub(crate) id: AgentId,
    pub(crate) session_id: String,
    pub(crate) role: String,
    pub(crate) task: String,
    pub(crate) origin: AgentOrigin,
    pub(crate) parent: Option<AgentId>,
}

#[derive(Debug)]
pub(crate) enum AgentUpdate {
    Added(AgentDescriptor),
    Event { id: AgentId, event: AgentEvent },
    Status { id: AgentId, status: AgentStatus },
}

pub(crate) struct ScopedAgentUpdate {
    pub(crate) root_session_id: String,
    pub(crate) update: AgentUpdate,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) struct SubagentRuntimeId(u64);

impl SubagentRuntimeId {
    pub(super) fn next() -> Self {
        Self(NEXT_RUNTIME_ID.fetch_add(1, Ordering::Relaxed) + 1)
    }
}
