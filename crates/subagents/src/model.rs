use nanocodex::{Model, agent::events::AgentEvent};
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_RUNTIME_ID: AtomicU64 = AtomicU64::new(0);

/// Identifies a child within one root session's task tree.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct AgentId(u64);

impl AgentId {
    /// Creates an identifier from its wire value.
    pub const fn new(value: u64) -> Self {
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

/// Identifies a directed message within one root session.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct MessageId(u64);

impl MessageId {
    /// Creates an identifier from its wire value.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

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

/// Correlates the messages in one two-party conversation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ThreadId(u64);

impl ThreadId {
    /// Creates an identifier from its wire value.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub(super) const fn for_message(message: MessageId) -> Self {
        Self(message.0)
    }
}

impl fmt::Display for ThreadId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Identifies the origin of a directed message.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MessageSender {
    /// The root session that owns the task tree.
    Root,
    /// A child session in the task tree.
    Agent {
        /// The sending child.
        agent_id: AgentId,
    },
}

impl MessageSender {
    pub(super) const fn agent_id(self) -> Option<AgentId> {
        match self {
            Self::Root => None,
            Self::Agent { agent_id } => Some(agent_id),
        }
    }
}

/// Controls when a directed message interrupts its recipient.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MessagePriority {
    /// Deliver after the recipient's active turn, or start an idle recipient.
    #[default]
    Deferred,
    /// Steer an active turn at its next safe model boundary.
    Urgent,
}

impl MessagePriority {
    /// Returns the stable wire name used in prompts and tool results.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Deferred => "deferred",
            Self::Urgent => "urgent",
        }
    }
}

/// Describes the coordination intent of a directed message.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MessagePurpose {
    /// Replace the recipient's task when the sender has management authority.
    Delegate,
    /// Share ordinary coordination context without replacing the task.
    #[default]
    Coordinate,
    /// Report evidence or a result that may affect another agent's work.
    Finding,
    /// Ask the recipient for information.
    Question,
    /// Answer the message identified by `in_reply_to`.
    Reply,
}

impl MessagePurpose {
    /// Returns the stable wire name used in prompts and tool results.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Delegate => "delegate",
            Self::Coordinate => "coordinate",
            Self::Finding => "finding",
            Self::Question => "question",
            Self::Reply => "reply",
        }
    }
}

/// Reports how a recipient accepted a message.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageDisposition {
    /// The message started a new turn on an idle recipient.
    Started,
    /// The message will run after the recipient's active turn.
    Queued,
    /// The message steered the recipient's active turn.
    Steered,
}

/// A bounded directed message between agents in one task tree.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentMessage {
    /// The message identity.
    pub id: MessageId,
    /// The conversation containing this message.
    pub thread_id: ThreadId,
    /// The message origin.
    pub from: MessageSender,
    /// The recipient child.
    pub to: AgentId,
    /// The requested delivery behavior.
    pub priority: MessagePriority,
    /// The coordination intent.
    pub purpose: MessagePurpose,
    /// The prior message answered by this reply.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<MessageId>,
    /// The bounded UTF-8 message body.
    pub body: String,
}

impl AgentMessage {
    pub(super) fn prompt(&self) -> String {
        let (sender, response_guidance) = match self.from {
            MessageSender::Root => (
                "the root agent".to_owned(),
                "Return any response through your required structured result; the root does not \
                 accept inbound agent messages in this experiment."
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

/// The retained messages in one two-party conversation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentThread {
    /// The thread identity.
    pub id: ThreadId,
    /// The two endpoints permitted to participate in the thread.
    pub participants: [MessageSender; 2],
    /// Retained messages in delivery order.
    pub messages: Vec<AgentMessage>,
}

/// Tracks admission and terminal delivery separately.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum MessageDeliveryState {
    /// The recipient mailbox accepted the message.
    Admitted {
        /// How the recipient accepted the message.
        disposition: MessageDisposition,
    },
    /// The recipient incorporated the message into a turn.
    Delivered {
        /// How the recipient accepted the message.
        disposition: MessageDisposition,
    },
    /// Delivery reached a terminal failure.
    Failed {
        /// A bounded description of the failure.
        error: String,
    },
}

/// A complete thread snapshot emitted when one message changes state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentMessageUpdate {
    /// The message whose delivery state changed.
    pub message_id: MessageId,
    /// The current retained thread.
    pub thread: AgentThread,
    /// The message's new delivery state.
    pub delivery: MessageDeliveryState,
}

pub(super) fn agent_prompt(id: AgentId, task: &str) -> String {
    let coordination = " Other agents may be working concurrently in the same workspace. Use \
                        list_agents to discover relevant peers. Communicate when doing so prevents \
                        duplicated work, coordinates shared dependencies or overlapping files, or \
                        surfaces findings that materially affect another agent's task. Treat \
                        concurrent changes as owned by their authors and avoid overwriting them. \
                        You may exchange bounded directed messages with any other agent in this \
                        task tree through send_agent_message. Deferred messages start an idle \
                        agent or wait for its active turn to finish. If a send is queued, do not \
                        wait for it inside your current turn: finish the turn so queued messages \
                        can be delivered. Urgent messages steer active turns. Ordinary messages \
                        provide coordination context; only a delegate message from an authorized \
                        manager replaces your assigned task.";
    format!(
        "Act as a specialist subagent. You have no inherited conversation context. Work only on \
         the delegated task and produce the required evidence-backed structured result. Your \
         agent ID is {id}. The runtime automatically places agents you delegate beneath you in \
         the task tree.{coordination}\n\nDelegated task:\n{task}"
    )
}

/// The lifecycle state of a child session.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AgentStatus {
    /// The child exists but has not started a turn.
    Pending,
    /// The child has an active turn.
    Running,
    /// The child submitted a schema-valid result.
    Completed {
        /// The validated structured result.
        output: serde_json::Value,
    },
    /// The most recent turn was interrupted and the session remains reusable.
    Interrupted,
    /// The most recent turn failed and the session remains reusable.
    Failed {
        /// A bounded description of the failure.
        error: String,
    },
    /// The runtime is stopping the child and rejecting new work.
    Closing,
    /// The child is terminal and cannot be reused.
    Closed,
}

impl AgentStatus {
    /// Returns whether the child still owns or is stopping active work.
    pub const fn is_active(&self) -> bool {
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

/// Describes a child session and its position in the task tree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentDescriptor {
    /// The child identity within its root session.
    pub id: AgentId,
    /// The underlying Nanocodex session identity.
    pub session_id: String,
    /// The model selected for the child.
    pub model: Model,
    /// The short specialization assigned by the caller.
    pub role: String,
    /// The child's current delegated task.
    pub task: String,
    /// The child that spawned this agent, or `None` for a direct child of the root.
    pub parent: Option<AgentId>,
}

/// A typed observation emitted by a [`Subagents`](crate::Subagents) runtime.
#[derive(Debug)]
pub enum AgentUpdate {
    /// A child was created or its delegated task changed.
    Added(AgentDescriptor),
    /// The child emitted a Nanocodex event.
    Event {
        /// The child that emitted the event.
        id: AgentId,
        /// The underlying session event.
        event: AgentEvent,
    },
    /// A child's lifecycle state changed.
    Status {
        /// The affected child.
        id: AgentId,
        /// The new lifecycle state.
        status: AgentStatus,
    },
    /// A directed message changed delivery state.
    Message(AgentMessageUpdate),
}

/// Associates one runtime update with its owning root session.
pub struct ScopedAgentUpdate {
    /// The root Nanocodex session that owns the task tree.
    pub root_session_id: String,
    /// The typed runtime observation.
    pub update: AgentUpdate,
}

/// Identifies one in-process runtime instance.
///
/// Consumers can discard late updates whose runtime identity no longer matches the active root.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SubagentRuntimeId(u64);

impl SubagentRuntimeId {
    pub(super) fn next() -> Self {
        Self(NEXT_RUNTIME_ID.fetch_add(1, Ordering::Relaxed) + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::{AgentId, AgentStatus, MessagePriority, agent_prompt};

    #[test]
    fn deferred_is_the_default_serialized_message_priority() {
        assert_eq!(MessagePriority::default(), MessagePriority::Deferred);
        assert_eq!(
            serde_json::to_value(MessagePriority::default()).unwrap(),
            serde_json::json!("deferred")
        );
    }

    #[test]
    fn agent_prompt_explains_peer_coordination_and_queued_delivery() {
        let prompt = agent_prompt(AgentId::new(1), "coordinate with a peer");

        assert!(prompt.contains("Other agents may be working concurrently"));
        assert!(prompt.contains("list_agents"));
        assert!(prompt.contains("prevents duplicated work"));
        assert!(prompt.contains("avoid overwriting them"));
        assert!(prompt.contains("If a send is queued"));
        assert!(prompt.contains("finish the turn"));
    }

    #[test]
    fn completed_status_serializes_structured_output_without_stringifying_it() {
        let status = AgentStatus::Completed {
            output: serde_json::json!({ "findings": [{ "line": 42 }] }),
        };

        assert_eq!(
            serde_json::to_value(status).unwrap(),
            serde_json::json!({
                "state": "completed",
                "output": { "findings": [{ "line": 42 }] }
            })
        );
    }
}
