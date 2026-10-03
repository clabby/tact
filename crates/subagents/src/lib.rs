#![doc = include_str!("../README.md")]

mod capacity;
mod harness;
mod message;
mod model;
mod roster;
mod runtime;
mod task_tree;
mod tools;

pub use model::{
    AgentContext, AgentDescriptor, AgentId, AgentMessage, AgentMessageUpdate, AgentStatus,
    AgentThread, AgentUpdate, MessageDeliveryState, MessageDisposition, MessageId, MessagePriority,
    MessagePurpose, MessageSender, ScopedAgentUpdate, SubagentRuntimeId, ThreadId,
};
pub use roster::{SUPPORTED_MODELS, parse_model};
pub use runtime::{AuthorityError, RootAgentAuthority, Subagents, WeakSubagents};
