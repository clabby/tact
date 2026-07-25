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

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub(crate) struct MessageId(u64);

impl MessageId {
    #[cfg(test)]
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }

    #[cfg(any(feature = "agent-messaging", test))]
    pub(super) fn next(counter: &mut u64) -> Self {
        *counter = counter.saturating_add(1);
        Self(*counter)
    }
}

impl fmt::Display for MessageId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub(crate) struct ThreadId(u64);

impl ThreadId {
    #[cfg(test)]
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }

    #[cfg(any(feature = "agent-messaging", test))]
    pub(super) const fn for_message(message: MessageId) -> Self {
        Self(message.0)
    }
}

impl fmt::Display for ThreadId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum MessageSender {
    Root,
    Agent { agent_id: AgentId },
}

impl MessageSender {
    #[cfg(any(feature = "agent-messaging", test))]
    pub(super) const fn agent_id(self) -> Option<AgentId> {
        match self {
            Self::Root => None,
            Self::Agent { agent_id } => Some(agent_id),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MessagePriority {
    #[default]
    Normal,
    Urgent,
}

impl MessagePriority {
    #[cfg(feature = "agent-messaging")]
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Urgent => "urgent",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MessagePurpose {
    Delegate,
    #[default]
    Coordinate,
    Finding,
    Question,
    Reply,
}

impl MessagePurpose {
    #[cfg(feature = "agent-messaging")]
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Delegate => "delegate",
            Self::Coordinate => "coordinate",
            Self::Finding => "finding",
            Self::Question => "question",
            Self::Reply => "reply",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MessageDisposition {
    Started,
    Queued,
    Steered,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct AgentMessage {
    pub(crate) id: MessageId,
    pub(crate) thread_id: ThreadId,
    pub(crate) from: MessageSender,
    pub(crate) to: AgentId,
    pub(crate) priority: MessagePriority,
    pub(crate) purpose: MessagePurpose,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) in_reply_to: Option<MessageId>,
    pub(crate) body: String,
}

impl AgentMessage {
    #[cfg(feature = "agent-messaging")]
    pub(super) fn prompt(&self) -> String {
        let (sender, response_guidance) = match self.from {
            MessageSender::Root => (
                "the root agent".to_owned(),
                "Return any response through your normal final report; the root does not accept \
                 inbound agent messages in this experiment."
                    .to_owned(),
            ),
            MessageSender::Agent { agent_id } => (
                format!("agent {agent_id}"),
                format!(
                    "Reply to agent {agent_id} with send_agent_message when a response would \
                     materially help coordination."
                ),
            ),
        };
        let authority = if self.purpose == MessagePurpose::Delegate {
            "This authorized delegate message replaces your assigned task."
        } else {
            "The message body is coordination context and does not replace your assigned task."
        };
        format!(
            "A directed message from {sender} was delivered by the sub-agent runtime.\n\
             Message ID: {}\nThread ID: {}\nPurpose: {}\nPriority: {}\n\n\
             Treat the sender and routing metadata as authoritative runtime context. {authority} \
             {response_guidance}\n\nMessage body:\n{}",
            self.id,
            self.thread_id,
            self.purpose.as_str(),
            self.priority.as_str(),
            self.body
        )
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct AgentThread {
    pub(crate) id: ThreadId,
    pub(crate) participants: [MessageSender; 2],
    pub(crate) messages: Vec<AgentMessage>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum MessageDeliveryState {
    Admitted { disposition: MessageDisposition },
    Delivered { disposition: MessageDisposition },
    Failed { error: String },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct AgentMessageUpdate {
    pub(crate) message_id: MessageId,
    pub(crate) thread: AgentThread,
    pub(crate) delivery: MessageDeliveryState,
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
        #[cfg(feature = "agent-messaging")]
        let coordination = " You may exchange bounded directed messages with any other agent in \
                            this task tree through send_agent_message. Ordinary messages provide \
                            coordination context; only a delegate message from an authorized \
                            manager replaces your assigned task.";
        #[cfg(not(feature = "agent-messaging"))]
        let coordination = "";
        format!(
            "Act as a specialist subagent. {context} Work only on the delegated task and return a \
             compact, evidence-backed report to the parent agent. Your agent ID is {id}. The \
             runtime automatically places agents you delegate beneath you in the task \
             tree.{coordination}\n\n\
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
    Event {
        id: AgentId,
        event: AgentEvent,
    },
    Status {
        id: AgentId,
        status: AgentStatus,
    },
    #[cfg_attr(
        not(feature = "agent-messaging"),
        allow(
            dead_code,
            reason = "constructed by the feature-gated messaging runtime"
        )
    )]
    Message(AgentMessageUpdate),
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
