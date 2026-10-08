//! Typed failures raised by the subagent runtime and its tools.
//!
//! Tool handlers return these errors to Nanocodex, which reports their `Display` text to the
//! calling model as a failed tool result. Messages therefore address the model and, where one
//! exists, name the corrective action.

use super::model::{AgentId, AgentStatus, MessageId, MessagePriority};
use nanocodex::{HarnessModel as Model, NanocodexError, Thinking};
use std::fmt;
use thiserror::Error;
use tokio::task::JoinError;

/// A failure of a subagent tool call or runtime operation.
#[derive(Debug, Error)]
pub(crate) enum SubagentError {
    /// The owning runtime was dropped while a tool still held a weak handle.
    #[error("subagent runtime is closed")]
    RuntimeClosed,
    /// The requested child violates the runtime's model or effort policy.
    #[error("invalid task request: {0}")]
    Spawn(#[from] SpawnError),
    /// The application factory or the child session itself rejected the request.
    #[error(transparent)]
    Agent(#[from] NanocodexError),
    /// The supplied output schema is not a valid JSON Schema.
    #[error("invalid output_schema: {0}")]
    InvalidSchema(#[source] Box<jsonschema::ValidationError<'static>>),
    /// Every concurrent turn slot is in use.
    #[error("sub-agent concurrency limit of {limit} has been reached; try delegation again later")]
    CapacityExhausted {
        /// The configured concurrency limit.
        limit: usize,
    },
    /// The agent is not part of the caller's task tree.
    #[error("unknown agent_id {0}")]
    UnknownAgent(AgentId),
    /// A spawn named a parent that is not registered.
    #[error("unknown parent agent {0}")]
    UnknownParent(AgentId),
    /// A child tried to manage an agent outside its own subtree.
    #[error("agent {caller} may only manage its descendants")]
    NotDescendant {
        /// The child that issued the request.
        caller: AgentId,
    },
    /// The parent began closing before it could spawn a child.
    #[error("agent {0} is closing and cannot spawn children")]
    ParentClosing(AgentId),
    /// The parent stopped between reserving and registering a child.
    #[error("agent {parent} stopped while spawning child {child}")]
    ParentStopped {
        /// The stopping parent.
        parent: AgentId,
        /// The child whose registration was abandoned.
        child: AgentId,
    },
    /// The agent's harness has shut down.
    #[error("agent {0} is closed")]
    AgentClosed(AgentId),
    /// A close completed while the agent still owned an active turn.
    #[error("agent {0} is still running")]
    StillRunning(AgentId),
    /// Registry bookkeeping lost a scope or session it had just resolved.
    #[error("subagent {0} disappeared")]
    StateDisappeared(RegistryEntry),
    /// `submit_result` was called from a root session.
    #[error("submit_result is only available to subagents")]
    NotSubagent,
    /// `submit_result` was called while the child had no running turn.
    #[error("submit_result is only available during an active subagent turn")]
    NoActiveTurn,
    /// An urgent message is replacing the turn token.
    #[error("the subagent turn is being steered; retry submit_result")]
    TurnSteering,
    /// The submitted token does not identify the running turn.
    #[error("submit_result used a stale or unknown turn_token")]
    StaleTurnToken,
    /// The running turn already has an accepted result.
    #[error("submit_result already accepted one result for this turn")]
    AlreadySubmitted,
    /// The submitted value violates the agent's output schema.
    #[error("submitted output does not match the required schema: {}", .violations.join("; "))]
    OutputMismatch {
        /// The first few schema violations, rendered for the model.
        violations: Vec<String>,
    },
    /// `wait_agent` was called without any agent IDs.
    #[error("agent_ids must not be empty")]
    EmptyWaitSet,
    /// `wait_agent` selected agents that can no longer change state.
    #[error(
        "already terminal agent_ids: [{}]; remaining active agent_ids: [{}]. Read available \
         results with list_agents({{include_completed:true}}) and wait only on active IDs.",
        IdList(.terminal),
        IdList(.active)
    )]
    AlreadyTerminal {
        /// Selected agents that are already terminal.
        terminal: Vec<AgentId>,
        /// Selected agents that can still change state.
        active: Vec<AgentId>,
    },
    /// The message violates a content, addressing, or threading rule.
    #[error(transparent)]
    Message(#[from] MessageError),
    /// The recipient has not started its first turn.
    #[error("agent {0} has not started and cannot receive messages yet")]
    RecipientPending(AgentId),
    /// The recipient is closing or closed.
    #[error("agent {agent} is {status:?} and cannot receive messages")]
    RecipientStopped {
        /// The recipient.
        agent: AgentId,
        /// The recipient's closing or closed status.
        status: AgentStatus,
    },
    /// The recipient's inbound channel for this priority is full.
    #[error("agent message mailbox is full for message {0}")]
    MailboxFull(MessageId),
    /// The harness stopped before it accepted or answered a message.
    #[error(transparent)]
    Delivery(#[from] DeliveryFailure),
    /// An urgent message could not steer the active turn.
    #[error("could not urgently message agent {agent}: {source}")]
    Steer {
        /// The recipient.
        agent: AgentId,
        /// The steering failure.
        source: NanocodexError,
    },
    /// The runtime was dropped before it recorded a message's admission.
    #[error("subagent runtime stopped before admitting the message")]
    AdmissionAbandoned,
    /// The harness command channel is closed.
    #[error("subagent harness is closed")]
    HarnessClosed,
    /// The harness dropped a command without answering.
    #[error("subagent harness stopped before responding")]
    HarnessStopped,
    /// A turn was requested while another one is running.
    #[error("agent {0} is not idle")]
    NotIdle(AgentId),
    /// The registry refused to open a turn for the agent's current status.
    #[error("agent {0} cannot start another turn")]
    CannotStartTurn(AgentId),
    /// The child session rejected a new turn.
    #[error("could not start agent {agent}: {source}")]
    StartTurn {
        /// The agent whose turn failed to start.
        agent: AgentId,
        /// The session failure.
        source: NanocodexError,
    },
    /// The child session refused to cancel its active turn.
    #[error("could not stop agent {agent}: {source}")]
    StopTurn {
        /// The agent whose turn could not be cancelled.
        agent: AgentId,
        /// The session failure.
        source: NanocodexError,
    },
    /// The child session failed to shut down cleanly.
    #[error("could not close agent {agent}: {source}")]
    CloseAgent {
        /// The agent that failed to shut down.
        agent: AgentId,
        /// The session failure.
        source: NanocodexError,
    },
    /// A bounded shutdown phase exceeded its deadline.
    #[error("timed out {0}")]
    ShutdownTimedOut(ShutdownPhase),
    /// A background task panicked or was cancelled during shutdown.
    #[error("subagent shutdown failed while {phase}: {source}")]
    ShutdownTask {
        /// The phase that joined the task.
        phase: ShutdownPhase,
        /// The task failure.
        source: JoinError,
    },
}

/// A spawn request rejected by the runtime's model and effort policy.
#[derive(Debug, Error)]
pub(crate) enum SpawnError {
    /// Children must reason; `none` is reserved for root sessions.
    #[error("subagent thinking must be low, medium, high, xhigh, or max")]
    ThinkingDisabled,
    /// The child would reason harder than its registered parent.
    #[error("subagent thinking {requested} exceeds parent effort {parent}")]
    ThinkingExceedsParent {
        /// The requested child effort.
        requested: Thinking,
        /// The parent's assigned effort.
        parent: Thinking,
    },
    /// The child would run a higher Codex tier than its parent.
    #[error("subagent model {requested} exceeds parent model {parent}")]
    ModelExceedsParent {
        /// The requested child model.
        requested: Model,
        /// The parent's model.
        parent: Model,
    },
    /// The calling root session runs a model outside the supported roster.
    #[error(transparent)]
    CallerModel(#[from] crate::UnsupportedModel),
    /// The requested model is outside the supported roster.
    #[error("unsupported subagent model")]
    ModelNotOffered,
    /// Claude children are disabled for this runtime.
    #[error("Claude subagents are disabled")]
    ClaudeDisabled,
    /// The model does not accept the requested effort.
    #[error("model {model} does not support thinking {thinking}")]
    UnsupportedThinking {
        /// The requested model.
        model: Model,
        /// The requested effort.
        thinking: Thinking,
    },
    /// The child would exceed the runtime's live effort cap.
    #[error("subagent thinking {requested} exceeds configured maximum {maximum}")]
    ThinkingExceedsMaximum {
        /// The requested child effort.
        requested: Thinking,
        /// The configured cap.
        maximum: Thinking,
    },
    /// The application has not configured a child-session factory.
    #[error("subagent factory is not configured")]
    FactoryMissing,
}

/// A directed message that violates a content, addressing, or threading rule.
#[derive(Debug, Error)]
pub(crate) enum MessageError {
    /// The body is empty or whitespace.
    #[error("message must not be empty")]
    Empty,
    /// The body exceeds the byte limit.
    #[error("message exceeds the {limit}-byte limit")]
    TooLarge {
        /// The maximum body size in bytes.
        limit: usize,
    },
    /// An agent addressed itself.
    #[error("agents cannot message themselves")]
    SelfAddressed,
    /// A reply omitted the message it answers.
    #[error("reply messages require an in_reply_to message ID")]
    ReplyWithoutTarget,
    /// A non-reply named a message to answer.
    #[error("in_reply_to is only valid for reply messages")]
    TargetWithoutReply,
    /// The answered message is unknown or has been trimmed from history.
    #[error("unknown in_reply_to message {0}")]
    UnknownReplyTarget(MessageId),
    /// The answered message came from the root session, which has no inbox.
    #[error("top-level root agents do not accept inbound messages")]
    RootNotAddressable,
    /// Only the original recipient may answer, and only to the original sender.
    #[error("message {0} can only be answered by its recipient")]
    ReplyByNonRecipient(MessageId),
}

/// Why a harness stopped a message before its recipient's turn consumed it.
#[derive(Clone, Copy, Debug, Error)]
pub(crate) enum DeliveryFailure {
    /// An interrupt rejected the message before it was admitted.
    #[error("message rejected by agent interruption")]
    RejectedByInterrupt,
    /// A close rejected the message before it was admitted.
    #[error("message rejected because the agent closed")]
    RejectedByClose,
    /// An interrupt discarded the queued message.
    #[error("message cancelled by agent interruption")]
    CancelledByInterrupt,
    /// A close discarded the queued message.
    #[error("message cancelled because the agent closed")]
    CancelledByClose,
    /// The harness stopped with the message still queued.
    #[error("subagent harness stopped")]
    HarnessStopped,
    /// The recipient's pending queue for this priority is full.
    #[error("{priority:?} mailbox for agent {agent} is full")]
    QueueFull {
        /// The message priority.
        priority: MessagePriority,
        /// The recipient.
        agent: AgentId,
    },
}

/// Registry state that must outlive the operation that resolved it.
#[derive(Clone, Copy, Debug)]
pub(crate) enum RegistryEntry {
    Scope,
    Session,
    Parent,
}

impl fmt::Display for RegistryEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Scope => "scope",
            Self::Session => "session",
            Self::Parent => "parent",
        })
    }
}

/// A bounded step of interrupting or closing a subtree.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ShutdownPhase {
    InterruptHarnesses,
    CloseHarnesses,
    StopTurns,
    JoinHarnesses,
    JoinEventStreams,
}

impl fmt::Display for ShutdownPhase {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InterruptHarnesses => "interrupting subagent harnesses",
            Self::CloseHarnesses => "closing subagent harnesses",
            Self::StopTurns => "waiting for subagent turns to stop",
            Self::JoinHarnesses => "waiting for subagent harnesses to close",
            Self::JoinEventStreams => "waiting for subagent event streams to close",
        })
    }
}

struct IdList<'a>(&'a [AgentId]);

impl fmt::Display for IdList<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, id) in self.0.iter().enumerate() {
            if index > 0 {
                formatter.write_str(", ")?;
            }
            id.fmt(formatter)?;
        }
        Ok(())
    }
}
