use crate::config::ReasoningEffort;
use serde_json::Value;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct EntryId(usize);

impl EntryId {
    pub(super) const fn from_index(index: usize) -> Self {
        Self(index)
    }

    pub(super) const fn index(self) -> usize {
        self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TransientStatus {
    Thinking,
    Responding,
    Warming,
    WaitingForBackgroundWork,
    Tool(String),
    Compacting,
    Retrying(String),
    Connecting,
    Reconnecting,
    Error(String),
}

#[derive(Clone, Debug)]
pub(crate) struct TranscriptEntry {
    pub(crate) id: EntryId,
    pub(crate) revision: u64,
    pub(crate) kind: EntryKind,
    pub(crate) hidden: bool,
}

#[derive(Clone, Debug)]
pub(crate) enum EntryKind {
    User { text: String },
    Assistant { text: String, complete: bool },
    Reasoning { text: String },
    Tool(ToolEntry),
    EffortChanged { to: ReasoningEffort },
    FastModeChanged { enabled: bool },
    Interrupted { count: usize },
    ContextCompacted { duration_ns: u64 },
    ContextCompactionFailed { message: String },
    Error { message: String },
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum MessagePhase {
    Commentary,
    Final,
}

impl From<Option<&str>> for MessagePhase {
    fn from(phase: Option<&str>) -> Self {
        if phase == Some("commentary") {
            return Self::Commentary;
        }
        Self::Final
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ToolEntry {
    pub(crate) name: String,
    pub(crate) arguments: Value,
    pub(crate) started_at_unix_ms: u64,
    pub(crate) state: ToolState,
    pub(crate) duration_ns: Option<u64>,
    pub(crate) result: Option<Value>,
    pub(crate) metadata: Option<Value>,
    pub(crate) substeps: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ToolState {
    Running,
    Succeeded,
    Failed,
}
