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

#[cfg(feature = "server")]
mod credential;
pub mod protocol;
#[cfg(feature = "server")]
mod router;

#[cfg(feature = "server")]
pub use credential::{Credential, CredentialError};
#[cfg(test)]
pub(crate) use router::MAX_JSON_BODY_BYTES;
#[cfg(feature = "server")]
pub use router::{MemoryServer, ServerBuildError};

#[cfg(all(test, feature = "server"))]
mod tests;
