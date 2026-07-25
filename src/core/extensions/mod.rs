//! Agent extensions exposed to the core runtime.

mod mcp;
mod skills;
pub(crate) mod subagents;

pub(super) use mcp::provider as mcp_provider;
pub(super) use skills::SkillCatalog;
