#![doc = include_str!("../README.md")]

/// Incompatible remote-memory protocol generation.
///
/// Route paths and session negotiation derive from this single value. Increment it only when
/// clients and servers must intentionally stop interoperating.
pub const VERSION: u32 = 2;

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
    MemorySource, normalize_identity,
};
pub use server::protocol::RemoteRole;
#[cfg(feature = "local")]
pub use store::LocalMemoryStore;
#[cfg(all(feature = "client", feature = "local"))]
pub use store::SelectedMemoryStore;
pub use store::{MemoryError, MemoryNamespaceFilter, MemoryStore};
#[cfg(feature = "client")]
pub use store::{RemoteClientError, RemoteMemoryClient, RemoteToken};
#[cfg(feature = "tool")]
pub use tool::{MemoryTool, MutationAuthorizer};

#[cfg(test)]
mod tests;
