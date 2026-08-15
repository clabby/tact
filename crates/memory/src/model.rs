//! Domain models shared by local storage, remote storage, and the wire protocol.

use crate::server::protocol::RemoteRole;
use serde::{Deserialize, Serialize};

const PROBATION_DURATION_MS: i64 = 7 * 24 * 60 * 60 * 1_000;

/// Stable identity and optimistic-concurrency version of a memory.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct MemoryKey {
    /// Positive identifier allocated by the owning store.
    pub id: i64,
    /// Positive version required for compare-and-swap mutations.
    pub version: u64,
    /// Owning remote namespace, or `None` for a local memory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
}

impl MemoryKey {
    /// Creates a key owned by a local store.
    pub const fn local(id: i64, version: u64) -> Self {
        Self {
            id,
            version,
            namespace: None,
        }
    }

    /// Creates a key owned by `namespace` on a remote store.
    pub fn remote(namespace: String, id: i64, version: u64) -> Self {
        Self {
            id,
            version,
            namespace: Some(namespace),
        }
    }

    /// Returns whether this key belongs to a local store.
    pub const fn is_local(&self) -> bool {
        self.namespace.is_none()
    }
}

/// Complete durable state of a memory.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MemoryRecord {
    /// Stable identity and current version.
    pub key: MemoryKey,
    /// User-authored memory content.
    pub content: String,
    /// Unix timestamp in milliseconds when this identity was created.
    pub created_at_ms: i64,
    /// Unix timestamp in milliseconds when this version was written.
    pub updated_at_ms: i64,
    /// Unix timestamp in milliseconds of the most recent matching scan.
    pub last_scanned_at_ms: Option<i64>,
    /// Number of matching scans recorded for this version.
    pub scan_count: u64,
    /// Unix timestamp in milliseconds of the most recent read.
    pub last_used_at_ms: Option<i64>,
    /// Number of reads recorded for this version.
    pub use_count: u64,
    /// Expiry time for an unused probationary record, if it remains on probation.
    pub probation_until_ms: Option<i64>,
}

/// Ranked, bounded preview returned by a memory scan.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct MemoryCandidate {
    /// Identity and version of the matching memory.
    pub key: MemoryKey,
    /// Bounded excerpt suitable for choosing whether to read the record.
    pub preview: String,
    /// Retrieval score; larger values rank ahead of smaller values.
    pub score: f64,
}

/// Result of a semantic memory scan.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct MemoryScan {
    /// Whether retrieval intentionally returned no candidates.
    pub abstained: bool,
    /// Candidates in descending retrieval rank.
    pub candidates: Vec<MemoryCandidate>,
}

impl MemoryScan {
    /// Ranks an in-memory corpus with Tact's deterministic BM25 retrieval.
    ///
    /// Server backends that keep a bounded corpus in memory can use this to match local search
    /// tokenization, scoring, tie-breaking, and preview behavior.
    pub fn rank(query: &str, memories: &[MemoryRecord], limit: usize) -> Self {
        let candidates = crate::retrieval::rank(query, memories, limit);
        Self {
            abstained: candidates.is_empty(),
            candidates,
        }
    }
}

/// Resource limits enforced by memory stores and remote-response validation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemoryLimits {
    /// Maximum UTF-8 bytes in one memory.
    pub content_bytes: usize,
    /// Maximum records owned by one local store or remote namespace.
    pub records: usize,
    /// Maximum aggregate content bytes per store or namespace.
    pub total_content_bytes: usize,
    /// Maximum local SQLite database size.
    pub database_bytes: usize,
    /// Maximum candidates returned by one scan.
    pub scan_results: usize,
    /// Maximum UTF-8 bytes in one scan query.
    pub query_bytes: usize,
    /// Lifetime of a newly written record that has never been read.
    pub probation_duration_ms: i64,
}

impl MemoryLimits {
    /// Limits used by production local and reference remote stores.
    pub const PRODUCTION: Self = Self {
        content_bytes: 1_024,
        records: 512,
        total_content_bytes: 256 * 1_024,
        database_bytes: 4 * 1_024 * 1_024,
        scan_results: 5,
        query_bytes: 512,
        probation_duration_ms: PROBATION_DURATION_MS,
    };
}

/// Backend selected by a [`crate::MemoryStore`].
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemorySource {
    /// Private local SQLite storage.
    Local,
    /// Namespaced remote storage.
    Remote,
}

/// Negotiated access information for the active backend.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MemoryAccess {
    /// Selected backend kind.
    pub source: MemorySource,
    /// Configured namespace for a remote backend.
    pub namespace: Option<String>,
    /// Server-authorized role after remote session negotiation.
    pub role: Option<RemoteRole>,
}

/// Counts produced while importing a remote snapshot into local storage.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct MemoryImportReport {
    /// Records inserted into local storage.
    pub inserted: usize,
    /// Records omitted because equivalent content already existed.
    pub skipped: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct StoredMemory {
    pub(crate) namespace: Option<String>,
    pub(crate) id: i64,
    pub(crate) content: String,
    pub(crate) created_at_ms: i64,
    pub(crate) updated_at_ms: i64,
    pub(crate) last_scanned_at_ms: Option<i64>,
    pub(crate) scan_count: u64,
    pub(crate) last_used_at_ms: Option<i64>,
    pub(crate) use_count: u64,
    pub(crate) probation_until_ms: Option<i64>,
    pub(crate) version: u64,
}

impl StoredMemory {
    pub(crate) fn key(&self) -> MemoryKey {
        match &self.namespace {
            Some(namespace) => MemoryKey::remote(namespace.clone(), self.id, self.version),
            None => MemoryKey::local(self.id, self.version),
        }
    }
}

impl From<StoredMemory> for MemoryRecord {
    fn from(memory: StoredMemory) -> Self {
        Self {
            key: memory.key(),
            content: memory.content,
            created_at_ms: memory.created_at_ms,
            updated_at_ms: memory.updated_at_ms,
            last_scanned_at_ms: memory.last_scanned_at_ms,
            scan_count: memory.scan_count,
            last_used_at_ms: memory.last_used_at_ms,
            use_count: memory.use_count,
            probation_until_ms: memory.probation_until_ms,
        }
    }
}

pub(crate) fn normalize_identity(content: &str) -> String {
    content
        .split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}
