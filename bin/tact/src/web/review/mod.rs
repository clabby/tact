//! Review of a checkout's changes: diff ranges, overviews, AI review, inline questions, and the
//! review decision a browser inserts into a session's draft.
//!
//! - [`engine`] keeps one review per checkout, its snapshot generations and caches, and the agent
//!   operations that run against a snapshot.
//! - [`backend`] reads the checkout through the `vcs` module and runs prompts on a session's agent.
//! - [`prompts`] builds those prompts and checks the agent's replies.
//! - [`routes`] is the HTTP surface; [`error`] is its failure body.
//! - [`model`], [`validation`], and [`compose`] hold the wire shapes, their limits, and the
//!   Markdown a decision becomes.

mod agent;
mod backend;
mod compose;
mod engine;
mod error;
mod model;
mod prompts;
mod routes;
#[cfg(test)]
mod tests;
mod validation;

pub(super) use agent::BridgeAgent;
#[cfg(test)]
pub(super) use agent::{AgentPrompt, ReviewAgent};
pub(super) use engine::ReviewRegistry;
pub(super) use routes::router;
