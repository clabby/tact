//! Versioned wire types shared by the remote client and reference server.

use crate::{MemoryCandidate, MemoryKey, MemoryRecord};
use const_format::concatcp;
use serde::{Deserialize, Serialize};

/// Header carrying the authenticated namespace on every request.
pub const NAMESPACE_HEADER: &str = "x-tact-memory-namespace";
/// Maximum UTF-8 bytes accepted in a namespace.
pub const MAX_NAMESPACE_BYTES: usize = 128;
/// Header carrying an opaque server-issued bookmark between requests.
pub const BOOKMARK_HEADER: &str = "x-tact-memory-bookmark";
/// Maximum UTF-8 bytes accepted in a bookmark.
pub const MAX_BOOKMARK_BYTES: usize = 4 * 1024;

/// Session-negotiation route.
pub const SESSION_PATH: &str = concatcp!("v", crate::VERSION, "/session");
/// Semantic scan route.
pub const SCAN_PATH: &str = concatcp!("v", crate::VERSION, "/memories/scan");
/// Record-read route.
pub const READ_PATH: &str = concatcp!("v", crate::VERSION, "/memories/read");
/// Visible-record listing route.
pub const LIST_PATH: &str = concatcp!("v", crate::VERSION, "/memories/list");
/// Direct mutation route.
pub const PUT_PATH: &str = concatcp!("v", crate::VERSION, "/memories/put");
/// Compare-and-swap deletion route.
pub const DELETE_PATH: &str = concatcp!("v", crate::VERSION, "/memories/delete");
/// Authoritative namespace snapshot route.
pub const SYNC_PATH: &str = concatcp!("v", crate::VERSION, "/memories/sync");
/// Paginated server snapshot export route.
pub const EXPORT_PATH: &str = concatcp!("v", crate::VERSION, "/memories/export");
/// Maximum records returned by one export page.
pub const MAX_EXPORT_PAGE_RECORDS: usize = 128;

/// Returns whether a namespace is non-empty, bounded, and URL/header safe.
pub fn is_valid_namespace(namespace: &str) -> bool {
    !namespace.is_empty()
        && namespace.len() <= MAX_NAMESPACE_BYTES
        && namespace
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// Returns whether an opaque bookmark is non-empty and bounded.
pub fn is_valid_bookmark(bookmark: &str) -> bool {
    !bookmark.is_empty() && bookmark.len() <= MAX_BOOKMARK_BYTES
}

/// Authorization role returned by session negotiation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteRole {
    /// May scan, read, list, and export visible memories.
    Reader,
    /// May also mutate the credential's namespace.
    Writer,
}

/// Session negotiation result.
#[derive(Debug, Deserialize, Serialize)]
pub struct SessionResponse {
    /// Server wire protocol version.
    pub protocol_version: u32,
    /// Namespace bound to the presented credential.
    pub namespace: String,
    /// Operations authorized for the credential.
    pub role: RemoteRole,
}

/// Authenticated namespace scope for one scan request.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanScope {
    /// Every visible namespace, ranked as one corpus.
    All,
    /// Only the authenticated namespace.
    Own,
    /// Every visible namespace other than the authenticated namespace.
    Others,
}

/// Request for semantic retrieval.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScanRequest {
    /// Search text, bounded by [`crate::MemoryLimits::query_bytes`].
    pub query: String,
    /// Namespace scope derived relative to the authenticated principal.
    pub scope: ScanScope,
    /// Maximum candidates requested from this scope.
    pub limit: usize,
}

/// Response to [`ScanRequest`].
#[derive(Debug, Deserialize, Serialize)]
pub struct ScanResponse {
    /// Candidates from the requested scope in descending rank.
    pub candidates: Vec<MemoryCandidate>,
}

/// Request to read unversioned IDs and versioned keys.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReadRequest {
    /// IDs in the authenticated namespace.
    #[serde(default)]
    pub ids: Vec<i64>,
    /// Versioned keys, which may identify visible foreign namespaces.
    #[serde(default)]
    pub keys: Vec<MemoryKey>,
}

/// Response containing requested current records.
#[derive(Debug, Deserialize, Serialize)]
pub struct ReadResponse {
    /// Existing, current requested records.
    pub memories: Vec<MemoryRecord>,
}

/// Response containing a bounded deterministic window of visible records.
#[derive(Debug, Deserialize, Serialize)]
pub struct ListResponse {
    /// At most [`crate::MemoryLimits::records`] records in deterministic store order.
    pub memories: Vec<MemoryRecord>,
}

/// Request to insert or compare-and-swap replace a memory.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PutRequest {
    /// New record content.
    pub content: String,
    /// Current key of the record to replace, or `None` to insert.
    #[serde(default)]
    pub replacement: Option<MemoryKey>,
}

/// Response to [`PutRequest`].
#[derive(Debug, Deserialize, Serialize)]
pub struct PutResponse {
    /// Inserted or replaced current record.
    pub memory: MemoryRecord,
}

/// Request to compare-and-swap delete a memory.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteRequest {
    /// Current key of the record to delete.
    pub key: MemoryKey,
}

/// Authoritative full snapshot for one authenticated namespace.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SyncRequest {
    /// Complete local snapshot; absent server records are deleted.
    pub memories: Vec<MemoryRecord>,
}

/// Exclusive position in deterministic `(namespace, id)` export order.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExportCursor {
    /// Namespace of the last returned record.
    pub namespace: String,
    /// ID of the last returned record.
    pub id: i64,
}

/// Request for one deterministic snapshot export page.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExportRequest {
    /// `None` exports every namespace; an explicit list exports only those namespaces.
    pub namespaces: Option<Vec<String>>,
    /// Exclusive cursor returned by the preceding page.
    #[serde(default)]
    pub cursor: Option<ExportCursor>,
    /// Requested page size, clamped to [`MAX_EXPORT_PAGE_RECORDS`].
    pub limit: usize,
}

/// One deterministic snapshot export page.
#[derive(Debug, Deserialize, Serialize)]
pub struct ExportResponse {
    /// Records strictly after the request cursor in `(namespace, id)` order.
    pub memories: Vec<MemoryRecord>,
    /// Cursor for another page, or `None` when the snapshot is exhausted.
    pub next_cursor: Option<ExportCursor>,
}

/// Counts produced by applying an authoritative namespace snapshot.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SyncReport {
    /// Previously absent records inserted.
    pub inserted: usize,
    /// Existing records replaced by snapshot state.
    pub replaced: usize,
    /// Existing records already equal to snapshot state.
    pub unchanged: usize,
    /// Existing records absent from the snapshot and deleted.
    pub deleted: usize,
}

/// Stable machine-readable remote failure category.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteErrorCode {
    /// Request shape or values are invalid.
    BadRequest,
    /// Credential is missing or invalid.
    Unauthorized,
    /// Credential does not authorize the operation.
    Forbidden,
    /// Requested namespace differs from the credential namespace.
    NamespaceMismatch,
    /// Client and server protocol versions differ.
    UnsupportedProtocol,
    /// Scan query exceeds its byte limit.
    QueryTooLarge,
    /// One memory exceeds its content byte limit.
    ContentTooLarge,
    /// Namespace record capacity is exhausted.
    RecordCapacity,
    /// Namespace aggregate content capacity is exhausted.
    ContentCapacity,
    /// Equivalent content already exists.
    Duplicate,
    /// Requested record does not exist.
    NotFound,
    /// Compare-and-swap version or snapshot state conflicts.
    Conflict,
    /// Service cannot currently complete the operation.
    Unavailable,
    /// Server failed without a safe client-visible detail.
    Internal,
}

/// Error response returned for a failed protocol operation.
#[derive(Debug, Deserialize, Serialize)]
pub struct ErrorResponse {
    /// Stable failure category.
    pub code: RemoteErrorCode,
}
