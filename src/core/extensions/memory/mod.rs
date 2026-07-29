//! Bounded global memory storage and deterministic retrieval.

mod retrieval;
mod secrets;
mod tool;

use retrieval::rank;
use rusqlite::{
    Connection, ErrorCode, OptionalExtension, Transaction, TransactionBehavior, params,
};
use secrets::contains_likely_secret;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use thiserror::Error;
pub(crate) use tool::MemoryTool;

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const DATABASE_PAGE_SIZE_BYTES: usize = 4 * 1024;
const PROBATION_DURATION_MS: i64 = 7 * 24 * 60 * 60 * 1_000;
const SCHEMA_VERSION: i64 = 1;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub(crate) struct MemoryKey {
    pub(crate) id: i64,
    pub(crate) version: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct MemoryRecord {
    pub(crate) key: MemoryKey,
    pub(crate) content: String,
    pub(crate) created_at_ms: i64,
    pub(crate) updated_at_ms: i64,
    pub(crate) last_scanned_at_ms: Option<i64>,
    pub(crate) scan_count: u64,
    pub(crate) last_used_at_ms: Option<i64>,
    pub(crate) use_count: u64,
    pub(crate) probation_until_ms: Option<i64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct MemoryCandidate {
    pub(crate) key: MemoryKey,
    pub(crate) preview: String,
    pub(crate) score: f64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct MemoryScan {
    pub(crate) abstained: bool,
    pub(crate) candidates: Vec<MemoryCandidate>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MemoryLimits {
    pub(crate) content_bytes: usize,
    pub(crate) records: usize,
    pub(crate) total_content_bytes: usize,
    pub(crate) database_bytes: usize,
    pub(crate) scan_results: usize,
    pub(crate) query_bytes: usize,
    pub(crate) probation_duration_ms: i64,
}

impl MemoryLimits {
    pub(crate) const PRODUCTION: Self = Self {
        content_bytes: 1_024,
        records: 512,
        total_content_bytes: 256 * 1_024,
        database_bytes: 4 * 1_024 * 1_024,
        scan_results: 5,
        query_bytes: 512,
        probation_duration_ms: PROBATION_DURATION_MS,
    };
}

#[derive(Clone, Debug)]
pub(crate) struct MemoryStore {
    path: Arc<PathBuf>,
    limits: MemoryLimits,
}

impl MemoryStore {
    pub(crate) fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: Arc::new(path.into()),
            limits: MemoryLimits::PRODUCTION,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_limits(path: impl Into<PathBuf>, limits: MemoryLimits) -> Self {
        Self {
            path: Arc::new(path.into()),
            limits,
        }
    }

    pub(crate) fn scan(
        &self,
        query: &str,
        limit: usize,
        now_ms: i64,
    ) -> Result<MemoryScan, MemoryError> {
        if query.len() > self.limits.query_bytes {
            return Err(MemoryError::QueryTooLarge {
                maximum_bytes: self.limits.query_bytes,
            });
        }

        let mut connection = self.open()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(MemoryError::from_sqlite)?;
        prune_expired(&transaction, now_ms)?;

        let memories = load_all(&transaction)?
            .into_iter()
            .filter(|memory| !contains_likely_secret(&memory.content))
            .collect::<Vec<_>>();
        let limit = limit.min(self.limits.scan_results);
        let candidates = rank(query, &memories, limit);

        for candidate in &candidates {
            transaction
                .execute(
                    "UPDATE memories
                     SET last_scanned_at_ms = ?1, scan_count = scan_count + 1
                     WHERE id = ?2",
                    params![now_ms, candidate.key.id],
                )
                .map_err(MemoryError::from_sqlite)?;
        }
        transaction.commit().map_err(MemoryError::from_sqlite)?;

        Ok(MemoryScan {
            abstained: candidates.is_empty(),
            candidates,
        })
    }

    pub(crate) fn read(&self, ids: &[i64], now_ms: i64) -> Result<Vec<MemoryRecord>, MemoryError> {
        let ids = distinct_ids(ids);
        let mut connection = self.open()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(MemoryError::from_sqlite)?;
        prune_expired(&transaction, now_ms)?;

        let mut records = Vec::with_capacity(ids.len());
        for id in ids {
            let memory = load_one(&transaction, id)?;
            let Some(mut memory) = memory else {
                continue;
            };
            if contains_likely_secret(&memory.content) {
                continue;
            }

            transaction
                .execute(
                    "UPDATE memories
                     SET last_used_at_ms = ?1, use_count = use_count + 1,
                         probation_until_ms = NULL
                     WHERE id = ?2",
                    params![now_ms, id],
                )
                .map_err(MemoryError::from_sqlite)?;
            memory.last_used_at_ms = Some(now_ms);
            memory.use_count = memory.use_count.saturating_add(1);
            memory.probation_until_ms = None;
            records.push(memory.into());
        }
        transaction.commit().map_err(MemoryError::from_sqlite)?;
        Ok(records)
    }

    pub(crate) fn put(
        &self,
        content: &str,
        replacement: Option<MemoryKey>,
        now_ms: i64,
    ) -> Result<MemoryRecord, MemoryError> {
        self.validate_content(content)?;
        let normalized_identity = normalize_identity(content);
        if normalized_identity.is_empty() {
            return Err(MemoryError::EmptyContent);
        }

        let mut connection = self.open()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(MemoryError::from_sqlite)?;
        prune_expired(&transaction, now_ms)?;

        let result = match replacement {
            Some(key) => self.replace(&transaction, content, &normalized_identity, key, now_ms),
            None => self.insert(&transaction, content, &normalized_identity, now_ms),
        }?;
        transaction.commit().map_err(MemoryError::from_sqlite)?;
        Ok(result.into())
    }

    pub(crate) fn delete(&self, key: MemoryKey, _now_ms: i64) -> Result<(), MemoryError> {
        let mut connection = self.open()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(MemoryError::from_sqlite)?;
        let current_version = transaction
            .query_row(
                "SELECT version FROM memories WHERE id = ?1",
                [key.id],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(MemoryError::from_sqlite)?;
        let Some(current_version) = current_version else {
            return Err(MemoryError::NotFound);
        };
        if current_version as u64 != key.version {
            return Err(MemoryError::Conflict);
        }

        transaction
            .execute("DELETE FROM memories WHERE id = ?1", [key.id])
            .map_err(MemoryError::from_sqlite)?;
        transaction.commit().map_err(MemoryError::from_sqlite)?;
        Ok(())
    }

    pub(crate) fn list(&self, now_ms: i64) -> Result<Vec<MemoryRecord>, MemoryError> {
        let mut connection = self.open()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(MemoryError::from_sqlite)?;
        prune_expired(&transaction, now_ms)?;
        let records = load_all(&transaction)?
            .into_iter()
            .filter(|memory| !contains_likely_secret(&memory.content))
            .map(MemoryRecord::from)
            .collect();
        transaction.commit().map_err(MemoryError::from_sqlite)?;
        Ok(records)
    }

    fn insert(
        &self,
        transaction: &Transaction<'_>,
        content: &str,
        normalized_identity: &str,
        now_ms: i64,
    ) -> Result<StoredMemory, MemoryError> {
        if identity_exists(transaction, normalized_identity, None)? {
            return Err(MemoryError::Duplicate);
        }
        let totals = totals(transaction)?;
        if totals.records >= self.limits.records as u64 {
            return Err(MemoryError::RecordCapacity {
                maximum: self.limits.records,
            });
        }
        self.check_content_capacity(totals.content_bytes, 0, content.len())?;

        let probation_until_ms = now_ms.saturating_add(self.limits.probation_duration_ms);
        transaction
            .execute(
                "INSERT INTO memories (
                    content, normalized_identity, created_at_ms, updated_at_ms,
                    last_scanned_at_ms, scan_count, last_used_at_ms, use_count,
                    probation_until_ms, version
                 ) VALUES (?1, ?2, ?3, ?3, NULL, 0, NULL, 0, ?4, 1)",
                params![content, normalized_identity, now_ms, probation_until_ms],
            )
            .map_err(MemoryError::from_write)?;
        let id = transaction.last_insert_rowid();
        load_one(transaction, id)?.ok_or(MemoryError::NotFound)
    }

    fn replace(
        &self,
        transaction: &Transaction<'_>,
        content: &str,
        normalized_identity: &str,
        key: MemoryKey,
        now_ms: i64,
    ) -> Result<StoredMemory, MemoryError> {
        let current = transaction
            .query_row(
                "SELECT version, length(CAST(content AS BLOB)) FROM memories WHERE id = ?1",
                [key.id],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(MemoryError::from_sqlite)?;
        let Some((current_version, previous_content_bytes)) = current else {
            return Err(MemoryError::NotFound);
        };
        if current_version as u64 != key.version {
            return Err(MemoryError::Conflict);
        }
        if identity_exists(transaction, normalized_identity, Some(key.id))? {
            return Err(MemoryError::Duplicate);
        }

        let totals = totals(transaction)?;
        self.check_content_capacity(
            totals.content_bytes,
            previous_content_bytes as usize,
            content.len(),
        )?;
        let next_version = current_version
            .checked_add(1)
            .ok_or(MemoryError::Conflict)?;
        let probation_until_ms = now_ms.saturating_add(self.limits.probation_duration_ms);
        transaction
            .execute(
                "UPDATE memories
                 SET content = ?1, normalized_identity = ?2, updated_at_ms = ?3,
                     last_scanned_at_ms = NULL, scan_count = 0,
                     last_used_at_ms = NULL, use_count = 0,
                     probation_until_ms = ?4, version = ?5
                 WHERE id = ?6 AND version = ?7",
                params![
                    content,
                    normalized_identity,
                    now_ms,
                    probation_until_ms,
                    next_version,
                    key.id,
                    current_version,
                ],
            )
            .map_err(MemoryError::from_write)?;
        load_one(transaction, key.id)?.ok_or(MemoryError::NotFound)
    }

    fn validate_content(&self, content: &str) -> Result<(), MemoryError> {
        if content.trim().is_empty() {
            return Err(MemoryError::EmptyContent);
        }
        if content.len() > self.limits.content_bytes {
            return Err(MemoryError::ContentTooLarge {
                maximum_bytes: self.limits.content_bytes,
            });
        }
        if contains_likely_secret(content) {
            return Err(MemoryError::SecretRejected);
        }
        Ok(())
    }

    fn check_content_capacity(
        &self,
        current_bytes: u64,
        replaced_bytes: usize,
        new_bytes: usize,
    ) -> Result<(), MemoryError> {
        let resulting_bytes = current_bytes
            .saturating_sub(replaced_bytes as u64)
            .saturating_add(new_bytes as u64);
        if resulting_bytes > self.limits.total_content_bytes as u64 {
            return Err(MemoryError::ContentCapacity {
                maximum_bytes: self.limits.total_content_bytes,
            });
        }
        Ok(())
    }

    fn open(&self) -> Result<Connection, MemoryError> {
        prepare_private_parent(&self.path)?;
        let connection = Connection::open(self.path.as_path()).map_err(MemoryError::from_sqlite)?;
        connection
            .busy_timeout(BUSY_TIMEOUT)
            .map_err(MemoryError::from_sqlite)?;
        let schema_version = connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .map_err(MemoryError::from_sqlite)?;
        if !matches!(schema_version, 0 | SCHEMA_VERSION) {
            return Err(MemoryError::UnsupportedSchemaVersion {
                found: schema_version,
                supported: SCHEMA_VERSION,
            });
        }
        connection
            .pragma_update(None, "journal_mode", "DELETE")
            .map_err(MemoryError::from_sqlite)?;
        connection
            .pragma_update(None, "page_size", DATABASE_PAGE_SIZE_BYTES as i64)
            .map_err(MemoryError::from_sqlite)?;
        let page_size = connection
            .query_row("PRAGMA page_size", [], |row| row.get::<_, i64>(0))
            .map_err(MemoryError::from_sqlite)? as usize;
        let maximum_pages = self.limits.database_bytes.div_ceil(page_size).max(1);
        connection
            .pragma_update(None, "max_page_count", maximum_pages as i64)
            .map_err(MemoryError::from_sqlite)?;
        connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS memories (
                    id INTEGER PRIMARY KEY,
                    content TEXT NOT NULL,
                    normalized_identity TEXT NOT NULL UNIQUE,
                    created_at_ms INTEGER NOT NULL,
                    updated_at_ms INTEGER NOT NULL,
                    last_scanned_at_ms INTEGER,
                    scan_count INTEGER NOT NULL DEFAULT 0 CHECK (scan_count >= 0),
                    last_used_at_ms INTEGER,
                    use_count INTEGER NOT NULL DEFAULT 0 CHECK (use_count >= 0),
                    probation_until_ms INTEGER,
                    version INTEGER NOT NULL DEFAULT 1 CHECK (version > 0)
                 );",
            )
            .map_err(MemoryError::from_write)?;
        if schema_version == 0 {
            connection
                .pragma_update(None, "user_version", SCHEMA_VERSION)
                .map_err(MemoryError::from_write)?;
        }
        Ok(connection)
    }
}

#[derive(Debug, Error)]
pub(crate) enum MemoryError {
    #[error("memory content is empty")]
    EmptyContent,
    #[error("memory content exceeds the {maximum_bytes}-byte limit")]
    ContentTooLarge { maximum_bytes: usize },
    #[error("memory query exceeds the {maximum_bytes}-byte limit")]
    QueryTooLarge { maximum_bytes: usize },
    #[error("memory record capacity of {maximum} was reached")]
    RecordCapacity { maximum: usize },
    #[error("memory content capacity of {maximum_bytes} bytes was reached")]
    ContentCapacity { maximum_bytes: usize },
    #[error("memory database capacity was reached")]
    DatabaseCapacity,
    #[error("memory content was rejected as a likely secret")]
    SecretRejected,
    #[error("an equivalent memory already exists")]
    Duplicate,
    #[error("memory was not found")]
    NotFound,
    #[error("memory changed since it was read")]
    Conflict,
    #[error(
        "memory schema version {found} is unsupported; this build supports version {supported}"
    )]
    UnsupportedSchemaVersion { found: i64, supported: i64 },
    #[error("could not prepare the memory directory")]
    Directory {
        #[source]
        source: std::io::Error,
    },
    #[error("memory storage operation failed")]
    Storage {
        #[source]
        source: rusqlite::Error,
    },
}

impl MemoryError {
    fn from_sqlite(source: rusqlite::Error) -> Self {
        Self::Storage { source }
    }

    fn from_write(source: rusqlite::Error) -> Self {
        match &source {
            rusqlite::Error::SqliteFailure(error, _) if error.code == ErrorCode::DiskFull => {
                Self::DatabaseCapacity
            }
            _ => Self::Storage { source },
        }
    }
}

#[derive(Clone, Debug)]
struct StoredMemory {
    id: i64,
    content: String,
    created_at_ms: i64,
    updated_at_ms: i64,
    last_scanned_at_ms: Option<i64>,
    scan_count: u64,
    last_used_at_ms: Option<i64>,
    use_count: u64,
    probation_until_ms: Option<i64>,
    version: u64,
}

impl StoredMemory {
    fn key(&self) -> MemoryKey {
        MemoryKey {
            id: self.id,
            version: self.version,
        }
    }

    #[cfg(test)]
    fn for_test(id: i64, content: &str) -> Self {
        Self {
            id,
            content: content.to_owned(),
            created_at_ms: 0,
            updated_at_ms: 0,
            last_scanned_at_ms: None,
            scan_count: 0,
            last_used_at_ms: None,
            use_count: 0,
            probation_until_ms: None,
            version: 1,
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

struct Totals {
    records: u64,
    content_bytes: u64,
}

fn totals(transaction: &Transaction<'_>) -> Result<Totals, MemoryError> {
    transaction
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(length(CAST(content AS BLOB))), 0) FROM memories",
            [],
            |row| {
                Ok(Totals {
                    records: row.get::<_, i64>(0)? as u64,
                    content_bytes: row.get::<_, i64>(1)? as u64,
                })
            },
        )
        .map_err(MemoryError::from_sqlite)
}

fn identity_exists(
    transaction: &Transaction<'_>,
    normalized_identity: &str,
    excluded_id: Option<i64>,
) -> Result<bool, MemoryError> {
    transaction
        .query_row(
            "SELECT EXISTS(
                SELECT 1 FROM memories
                WHERE normalized_identity = ?1 AND (?2 IS NULL OR id != ?2)
             )",
            params![normalized_identity, excluded_id],
            |row| row.get(0),
        )
        .map_err(MemoryError::from_sqlite)
}

fn prune_expired(transaction: &Transaction<'_>, now_ms: i64) -> Result<(), MemoryError> {
    transaction
        .execute(
            "DELETE FROM memories
             WHERE probation_until_ms IS NOT NULL
               AND probation_until_ms <= ?1
               AND use_count = 0",
            [now_ms],
        )
        .map_err(MemoryError::from_sqlite)?;
    Ok(())
}

fn load_all(transaction: &Transaction<'_>) -> Result<Vec<StoredMemory>, MemoryError> {
    let mut statement = transaction
        .prepare(
            "SELECT id, content, created_at_ms, updated_at_ms,
                    last_scanned_at_ms, scan_count, last_used_at_ms, use_count,
                    probation_until_ms, version
             FROM memories
             ORDER BY id",
        )
        .map_err(MemoryError::from_sqlite)?;
    let rows = statement
        .query_map([], row_to_memory)
        .map_err(MemoryError::from_sqlite)?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(MemoryError::from_sqlite)
}

fn load_one(transaction: &Transaction<'_>, id: i64) -> Result<Option<StoredMemory>, MemoryError> {
    transaction
        .query_row(
            "SELECT id, content, created_at_ms, updated_at_ms,
                    last_scanned_at_ms, scan_count, last_used_at_ms, use_count,
                    probation_until_ms, version
             FROM memories
             WHERE id = ?1",
            [id],
            row_to_memory,
        )
        .optional()
        .map_err(MemoryError::from_sqlite)
}

fn row_to_memory(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredMemory> {
    Ok(StoredMemory {
        id: row.get(0)?,
        content: row.get(1)?,
        created_at_ms: row.get(2)?,
        updated_at_ms: row.get(3)?,
        last_scanned_at_ms: row.get(4)?,
        scan_count: row.get::<_, i64>(5)? as u64,
        last_used_at_ms: row.get(6)?,
        use_count: row.get::<_, i64>(7)? as u64,
        probation_until_ms: row.get(8)?,
        version: row.get::<_, i64>(9)? as u64,
    })
}

fn distinct_ids(ids: &[i64]) -> Vec<i64> {
    let mut seen = HashSet::new();
    ids.iter().copied().filter(|id| seen.insert(*id)).collect()
}

fn normalize_identity(content: &str) -> String {
    content
        .split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

fn prepare_private_parent(path: &Path) -> Result<(), MemoryError> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    if parent.as_os_str().is_empty() {
        return Ok(());
    }
    fs::create_dir_all(parent).map_err(|source| MemoryError::Directory { source })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .map_err(|source| MemoryError::Directory { source })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
