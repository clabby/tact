//! Agent extensions exposed to the core runtime.

mod mcp;
pub(crate) mod sessions;
mod skills;

pub(super) use mcp::provider as mcp_provider;
pub(crate) use skills::Skill;
pub(super) use skills::SkillCatalog;
