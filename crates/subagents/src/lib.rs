#![doc = include_str!("../README.md")]

mod capacity;
mod error;
mod harness;
mod message;
mod model;
mod models;
mod output;
mod roster;
mod runtime;
mod speed;
mod task_tree;
#[cfg(test)]
mod test_support;
mod tools;
mod turn;

pub use model::{
    AgentContext, AgentDescriptor, AgentId, AgentMessage, AgentMessageUpdate, AgentStatus,
    AgentThread, AgentUpdate, MessageDeliveryState, MessageDisposition, MessageId, MessagePriority,
    MessagePurpose, MessageSender, ScopedAgentUpdate, SubagentRuntimeId, ThreadId,
};
pub use models::{SUPPORTED_MODELS, UnsupportedModel, parse_model};
pub use roster::{SubagentNode, SubagentRoster};
pub use runtime::{AuthorityError, RootAgentAuthority, Subagents, WeakSubagents};
pub use speed::Speed;
