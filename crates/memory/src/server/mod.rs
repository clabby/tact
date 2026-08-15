//! Reusable authenticated server for shared Tact memory.
//!
//! [MemoryServer] authenticates each request before binding its namespace to a concrete
//! [crate::MemoryStore]. It owns HTTP parsing, authorization, stable protocol errors, tracing,
//! request timeouts, and concurrency limits. Backend implementations own durable transactions,
//! indexing, capacity, telemetry, and stable export pagination.
//!
//! Production deployments can bind PlanetScale, Cloudflare, PostgreSQL, or another store through
//! the same [crate::MemoryStore] contract. The workspace's `tact-memory-server-example` package
//! demonstrates the server with a process-lifetime in-memory backend.

mod credential;
pub mod protocol;
mod router;

pub use credential::{Credential, CredentialError};
#[cfg(test)]
pub(crate) use router::MAX_JSON_BODY_BYTES;
pub use router::{MemoryServer, ServerBuildError};

#[cfg(test)]
mod tests;
