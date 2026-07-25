//! Reusable child-agent tools and the typed runtime/UI update boundary.

mod capacity;
mod harness;
mod model;
mod runtime;
mod task_tree;
mod tools;

pub(crate) use model::{
    AgentDescriptor, AgentId, AgentOrigin, AgentStatus, AgentUpdate, ScopedAgentUpdate,
    SubagentRuntimeId,
};
pub(crate) use runtime::{SubagentControl, channel};
pub(crate) use tools::root_tools;
