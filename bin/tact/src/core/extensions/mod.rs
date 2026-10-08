//! Agent extensions exposed to the core runtime.

mod current_session;
mod mcp;
pub(crate) mod sessions;
mod skills;

pub(super) use current_session::CurrentSessionTool;
pub(super) use mcp::provider as mcp_provider;
pub(super) use skills::SkillCatalog;
pub(crate) use skills::{Skill, SkillMatches};
