//! Reusable child-agent tools and the typed runtime/UI update boundary.

mod capacity;
mod harness;
#[cfg(any(feature = "agent-messaging", test))]
mod message;
mod model;
mod runtime;
mod task_tree;
mod tools;

pub(crate) use model::{
    AgentDescriptor, AgentId, AgentMessage, AgentMessageUpdate, AgentOrigin, AgentStatus,
    AgentThread, AgentUpdate, MessageDeliveryState, MessageDisposition, MessageId, MessagePriority,
    MessagePurpose, MessageSender, ScopedAgentUpdate, SubagentRuntimeId, ThreadId,
};
pub(crate) use runtime::{SubagentControl, channel};
pub(crate) use tools::root_tools;
