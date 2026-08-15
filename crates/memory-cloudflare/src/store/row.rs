//! Exact conversion between D1 result objects and memory domain records.

use super::MessageError;
use serde::{Deserialize, Serialize};
use std::fmt::Display;
use tact_memory::{MemoryError, MemoryKey, MemoryRecord, normalize_identity};
use worker::d1::D1Result;

/// Typed decoding for D1 result objects returned by this backend's query contracts.
pub(super) trait DecodeResult {
    /// Decodes all records using the shared record projection.
    fn records(&self) -> Result<Vec<MemoryRecord>, MemoryError>;

    /// Decodes the one record returned by a successful mutation.
    fn record(&self) -> Result<MemoryRecord, MemoryError>;

    /// Decodes a query whose contract requires exactly one result row.
    fn one<T: for<'de> Deserialize<'de>>(&self) -> Result<T, MemoryError>;
}

impl DecodeResult for D1Result {
    fn records(&self) -> Result<Vec<MemoryRecord>, MemoryError> {
        self.results::<RecordRow>()
            .map_err(MessageError::backend)?
            .into_iter()
            .map(MemoryRecord::try_from)
            .collect()
    }

    fn record(&self) -> Result<MemoryRecord, MemoryError> {
        self.records()?
            .into_iter()
            .next()
            .ok_or_else(|| MessageError::backend("D1 mutation produced no record"))
    }

    fn one<T: for<'de> Deserialize<'de>>(&self) -> Result<T, MemoryError> {
        self.results::<T>()
            .map_err(MessageError::backend)?
            .into_iter()
            .next()
            .ok_or_else(|| MessageError::backend("D1 query produced no row"))
    }
}

/// D1 representation that preserves SQLite integers across JavaScript.
#[derive(Deserialize)]
struct RecordRow {
    namespace: String,
    id: String,
    version: String,
    content: String,
    created_at_ms: String,
    updated_at_ms: String,
    last_scanned_at_ms: Option<String>,
    scan_count: String,
    last_used_at_ms: Option<String>,
    use_count: String,
    probation_until_ms: Option<String>,
}

impl TryFrom<RecordRow> for MemoryRecord {
    type Error = MemoryError;

    fn try_from(row: RecordRow) -> Result<Self, Self::Error> {
        Ok(Self {
            key: MemoryKey::remote(row.namespace, parse(&row.id)?, parse(&row.version)?),
            content: row.content,
            created_at_ms: parse(&row.created_at_ms)?,
            updated_at_ms: parse(&row.updated_at_ms)?,
            last_scanned_at_ms: parse_optional(row.last_scanned_at_ms)?,
            scan_count: parse(&row.scan_count)?,
            last_used_at_ms: parse_optional(row.last_used_at_ms)?,
            use_count: parse(&row.use_count)?,
            probation_until_ms: parse_optional(row.probation_until_ms)?,
        })
    }
}

fn parse<T: std::str::FromStr>(value: &str) -> Result<T, MemoryError>
where
    T::Err: Display,
{
    value.parse().map_err(MessageError::backend)
}

fn parse_optional<T: std::str::FromStr>(value: Option<String>) -> Result<Option<T>, MemoryError>
where
    T::Err: Display,
{
    value.map(|value| parse(&value)).transpose()
}

/// Current version used to distinguish absent and stale deletes.
#[derive(Deserialize)]
pub(super) struct VersionRow {
    pub(super) version: String,
}

/// Shared corpus size measured before records enter the Worker isolate.
#[derive(Deserialize)]
pub(super) struct CorpusRow {
    pub(super) record_count: usize,
    pub(super) content_bytes: usize,
}

/// Namespace capacity and duplicate state measured before insertion.
#[derive(Deserialize)]
pub(super) struct CapacityRow {
    pub(super) duplicate: usize,
    pub(super) record_count: usize,
    pub(super) content_bytes: usize,
}

/// Existing-record state used to classify a rejected replacement.
#[derive(Deserialize)]
pub(super) struct ReplaceRow {
    pub(super) duplicate: usize,
    pub(super) version: Option<String>,
    pub(super) content_bytes: usize,
    pub(super) replaced_bytes: Option<usize>,
}

/// Snapshot representation encoded for exact `json_each` ingestion.
#[derive(Serialize)]
pub(super) struct SyncRow {
    id: String,
    version: String,
    content: String,
    identity: String,
    created_at_ms: String,
    updated_at_ms: String,
    last_scanned_at_ms: Option<String>,
    scan_count: String,
    last_used_at_ms: Option<String>,
    use_count: String,
    probation_until_ms: Option<String>,
}

impl From<&MemoryRecord> for SyncRow {
    fn from(record: &MemoryRecord) -> Self {
        Self {
            id: record.key.id.to_string(),
            version: record.key.version.to_string(),
            content: record.content.clone(),
            identity: normalize_identity(&record.content),
            created_at_ms: record.created_at_ms.to_string(),
            updated_at_ms: record.updated_at_ms.to_string(),
            last_scanned_at_ms: record.last_scanned_at_ms.map(|value| value.to_string()),
            scan_count: record.scan_count.to_string(),
            last_used_at_ms: record.last_used_at_ms.map(|value| value.to_string()),
            use_count: record.use_count.to_string(),
            probation_until_ms: record.probation_until_ms.map(|value| value.to_string()),
        }
    }
}
