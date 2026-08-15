#![doc = include_str!("../README.md")]

mod capacity;
mod harness;
mod message;
mod model;
mod runtime;
mod task_tree;
mod tools;

pub use model::{
    AgentDescriptor, AgentId, AgentMessage, AgentMessageUpdate, AgentStatus, AgentThread,
    AgentUpdate, MessageDeliveryState, MessageDisposition, MessageId, MessagePriority,
    MessagePurpose, MessageSender, ScopedAgentUpdate, SubagentRuntimeId, ThreadId,
};
pub use runtime::{AuthorityError, RootAgentAuthority, Subagents, WeakSubagents};
