use crate::{
    app::config::ReasoningEffort,
    core::extensions::subagents::{AgentThread, MessageDeliveryState, MessageId, MessageSender},
};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct EntryId(usize);

impl EntryId {
    pub(super) const fn from_index(index: usize) -> Self {
        Self(index)
    }

    pub(crate) const fn index(self) -> usize {
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
    pub(crate) parent: Option<EntryId>,
    pub(crate) trailing_spacer: bool,
}

#[derive(Clone, Debug)]
pub(crate) enum EntryKind {
    User { text: String },
    Assistant { text: String, complete: bool },
    Reasoning { text: String },
    Tool(ToolEntry),
    DirectedMessage(DirectedMessageEntry),
    EffortChanged { to: ReasoningEffort },
    FastModeChanged { enabled: bool },
    Interrupted { count: usize },
    ContextCompacted { duration_ns: u64 },
    TurnCompleted { duration_ns: u64 },
    ContextCompactionFailed { message: String },
    Error { message: String },
}

#[derive(Clone, Debug)]
pub(crate) struct DirectedMessageEntry {
    pub(crate) perspective: MessageSender,
    pub(crate) thread: AgentThread,
    pub(crate) deliveries: Vec<MessageDelivery>,
}

impl DirectedMessageEntry {
    pub(crate) fn delivery(&self, message_id: MessageId) -> Option<&MessageDeliveryState> {
        self.deliveries
            .iter()
            .find(|delivery| delivery.message_id == message_id)
            .map(|delivery| &delivery.state)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct MessageDelivery {
    pub(crate) message_id: MessageId,
    pub(crate) state: MessageDeliveryState,
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
    pub(crate) child_count: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ToolState {
    Running,
    Succeeded,
    Failed,
}
