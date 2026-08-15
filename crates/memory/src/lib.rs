//! Bounded memory storage for local agents and shared teams.
//!
//! # Architecture
//!
//! [`MemoryStore`] is the asynchronous storage contract. [`LocalMemoryStore`] preserves Tact's
//! schema-v1 on-disk format, [`RemoteMemoryClient`] carries the same operations over authenticated
//! HTTP, and [`server::MemoryServer`] exposes any namespace-bound implementation through the same
//! protocol. [`SelectedMemoryStore`] lets an application choose one local or remote backend without
//! merging their runtime results.
//!
//! # Remote protocol
//!
//! [`server::protocol`] contains the versioned JSON types and route constants shared by clients
//! and servers. Every compatibility check and route derives from [`VERSION`]. Remote credentials
//! bind each writer to one author namespace; record keys retain that namespace for attribution and
//! optimistic concurrency.
//!
//! # Bounds and secrets
//!
//! Implementations enforce record, content, query, and aggregate corpus bounds.
//! Tact applies one shared secret-content detector at client-side storage boundaries: mutations
//! are rejected before reaching the selected backend, and local or remote records that predate
//! that check are suppressed before use. Server-side backends remain storage-policy agnostic.
//! [`RemoteToken`] owns bearer credentials in zeroizing storage and redacts its debug representation.
/// Incompatible remote-memory protocol generation.
///
/// Route paths and session negotiation derive from this single value. Increment it only when
/// clients and servers must intentionally stop interoperating.
pub const VERSION: u32 = 1;

mod model;
mod retrieval;
#[cfg(any(feature = "client", feature = "local"))]
mod secrets;
pub mod server;
mod store;
#[cfg(feature = "tool")]
mod tool;

pub use model::{
    MemoryAccess, MemoryCandidate, MemoryImportReport, MemoryKey, MemoryLimits, MemoryRecord,
    MemoryScan, MemorySource, normalize_identity,
};
pub use server::protocol::RemoteRole;
#[cfg(feature = "local")]
pub use store::LocalMemoryStore;
#[cfg(all(feature = "client", feature = "local"))]
pub use store::SelectedMemoryStore;
pub use store::{MemoryError, MemoryStore};
#[cfg(feature = "client")]
pub use store::{RemoteClientError, RemoteMemoryClient, RemoteToken};
#[cfg(feature = "tool")]
pub use tool::{MemoryTool, MutationAuthorizer};

#[cfg(test)]
mod tests;
