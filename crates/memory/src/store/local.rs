//! Private schema-v1 SQLite storage.

use super::{MemoryError, MemoryStore, current_time_ms};
use crate::{
    MemoryImportReport, MemoryKey, MemoryLimits, MemoryRecord, MemoryScan,
    model::{StoredMemory, normalize_identity},
    secrets::contains_likely_secret,
    server::protocol::{self, ExportCursor, SyncReport},
};
use rusqlite::{
    Connection, ErrorCode, OptionalExtension, Transaction, TransactionBehavior, params,
};
use std::{
    collections::{HashMap, HashSet},
    fs,
    future::Future,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use thiserror::Error;

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const DATABASE_PAGE_SIZE_BYTES: usize = 4 * 1024;
const SCHEMA_VERSION: i64 = 1;
const ALLOCATOR_ROW_ID: i64 = 1;
const INSTALL_ALLOCATOR_TRIGGER: &str = "CREATE TRIGGER IF NOT EXISTS memory_id_allocator
     AFTER INSERT ON memories
     BEGIN
        SELECT CASE
            WHEN NEW.id < (SELECT next_id FROM memory_metadata WHERE id = 1)
            THEN RAISE(ABORT, 'memory id was already allocated')
        END;
        UPDATE memory_metadata SET next_id = NEW.id + 1
        WHERE id = 1 AND next_id <= NEW.id;
     END;";

#[derive(Debug, Error)]
enum LocalStoreError {
    #[error("could not prepare the memory directory")]
    Directory(#[source] std::io::Error),
    #[error("memory storage task stopped unexpectedly")]
    Task(#[source] tokio::task::JoinError),
}

/// Concrete private SQLite memory store.
#[derive(Clone, Debug)]
pub struct LocalMemoryStore {
    pub(crate) path: Arc<PathBuf>,
    limits: MemoryLimits,
}

impl LocalMemoryStore {
    /// Opens or creates a private local SQLite store at `path` on first use.
    pub fn new(path: impl Into<PathBuf>) -> Self {
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

    /// Searches visible memories and records scan telemetry at `now_ms`.
    async fn scan(
        &self,
        query: &str,
        limit: usize,
        now_ms: i64,
    ) -> Result<MemoryScan, MemoryError> {
        let store = self.clone();
        let query = query.to_owned();
        let limit = limit.min(self.limits.scan_results);
        run_local(move || store.scan_local(&query, limit, now_ms)).await
    }

    pub(crate) fn scan_local(
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
            .map_err(sqlite_error)?;
        prune_expired(&transaction, now_ms)?;

        let memories = load_all(&transaction)?
            .into_iter()
            .filter(|memory| !contains_likely_secret(&memory.content))
            .map(MemoryRecord::from)
            .collect::<Vec<_>>();
        let limit = limit.min(self.limits.scan_results);
        let scan = MemoryScan::rank(query, &memories, limit);

        for candidate in &scan.candidates {
            transaction
                .execute(
                    "UPDATE memories
                     SET last_scanned_at_ms = ?1, scan_count = scan_count + 1
                     WHERE id = ?2",
                    params![now_ms, candidate.key.id],
                )
                .map_err(sqlite_error)?;
        }
        transaction.commit().map_err(sqlite_error)?;

        Ok(scan)
    }

    pub(crate) fn read_local(
        &self,
        references: &[(i64, Option<u64>)],
        now_ms: i64,
    ) -> Result<Vec<MemoryRecord>, MemoryError> {
        let mut connection = self.open()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sqlite_error)?;
        prune_expired(&transaction, now_ms)?;

        let mut seen = HashSet::new();
        let mut records = Vec::with_capacity(references.len());
        for &(id, version) in references {
            let memory = load_one(&transaction, id)?;
            let Some(mut memory) = memory else {
                continue;
            };
            if version.is_some_and(|version| version != memory.version)
                || contains_likely_secret(&memory.content)
                || !seen.insert(id)
            {
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
                .map_err(sqlite_error)?;
            memory.last_used_at_ms = Some(now_ms);
            memory.use_count = memory.use_count.saturating_add(1);
            memory.probation_until_ms = None;
            records.push(memory.into());
        }
        transaction.commit().map_err(sqlite_error)?;
        Ok(records)
    }

    /// Reads records selected by unversioned IDs and versioned keys.
    ///
    /// IDs refer to the active local store or configured remote namespace. Keys retain their
    /// namespace and version semantics. Missing, stale, and duplicate records are omitted.
    /// Successful reads record use telemetry at `now_ms`.
    async fn read(
        &self,
        local_ids: &[i64],
        keys: &[MemoryKey],
        now_ms: i64,
    ) -> Result<Vec<MemoryRecord>, MemoryError> {
        let store = self.clone();
        let owned_keys = keys.to_vec();
        let read_ids = local_ids.to_vec();
        run_local(move || {
            let mut references = owned_keys
                .iter()
                .filter(|key| key.is_local())
                .map(|key| (key.id, Some(key.version)))
                .collect::<Vec<_>>();
            references.extend(distinct_ids(&read_ids).into_iter().map(|id| (id, None)));
            store.read_local(&references, now_ms)
        })
        .await
    }

    /// Inserts content or compare-and-swap replaces the record identified by `replacement`.
    async fn put(
        &self,
        content: &str,
        replacement: Option<MemoryKey>,
        now_ms: i64,
    ) -> Result<MemoryRecord, MemoryError> {
        if replacement.as_ref().is_some_and(|key| !key.is_local()) {
            return Err(MemoryError::RemoteReadOnly);
        }
        let store = self.clone();
        let content = content.to_owned();
        run_local(move || store.put_local(&content, replacement, now_ms)).await
    }

    pub(crate) fn put_local(
        &self,
        content: &str,
        replacement: Option<MemoryKey>,
        now_ms: i64,
    ) -> Result<MemoryRecord, MemoryError> {
        validate_content(content, &self.limits)?;
        let normalized_identity = normalize_identity(content);
        if normalized_identity.is_empty() {
            return Err(MemoryError::EmptyContent);
        }

        let mut connection = self.open()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sqlite_error)?;
        prune_expired(&transaction, now_ms)?;

        let result = match replacement {
            Some(key) => self.replace(&transaction, content, &normalized_identity, key, now_ms),
            None => self.insert(&transaction, content, &normalized_identity, now_ms),
        }?;
        transaction.commit().map_err(sqlite_error)?;
        Ok(result.into())
    }

    /// Compare-and-swap deletes `key` from its owning backend.
    async fn delete(&self, key: MemoryKey) -> Result<(), MemoryError> {
        if !key.is_local() {
            return Err(MemoryError::RemoteReadOnly);
        }
        let store = self.clone();
        run_local(move || store.delete_local(key)).await
    }

    pub(crate) fn delete_local(&self, key: MemoryKey) -> Result<(), MemoryError> {
        let mut connection = self.open()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sqlite_error)?;
        let current_version = transaction
            .query_row(
                "SELECT version FROM memories WHERE id = ?1",
                [key.id],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(sqlite_error)?;
        let Some(current_version) = current_version else {
            transaction.commit().map_err(sqlite_error)?;
            return Ok(());
        };
        if current_version as u64 != key.version {
            return Err(MemoryError::Conflict);
        }

        transaction
            .execute("DELETE FROM memories WHERE id = ?1", [key.id])
            .map_err(sqlite_error)?;
        transaction.commit().map_err(sqlite_error)?;
        Ok(())
    }

    /// Lists all visible records after pruning probation at `now_ms`.
    async fn list(&self, now_ms: i64) -> Result<Vec<MemoryRecord>, MemoryError> {
        let store = self.clone();
        run_local(move || store.list_local(now_ms)).await
    }

    /// Imports a validated remote snapshot into a local store as new probationary records.
    pub async fn merge_remote_export(
        &self,
        memories: Vec<MemoryRecord>,
    ) -> Result<MemoryImportReport, MemoryError> {
        if memories.is_empty() {
            return Ok(MemoryImportReport::default());
        }
        let store = self.clone();
        let now_ms = current_time_ms();
        run_local(move || store.merge_remote_export_local(memories, now_ms)).await
    }

    fn merge_remote_export_local(
        &self,
        mut memories: Vec<MemoryRecord>,
        now_ms: i64,
    ) -> Result<MemoryImportReport, MemoryError> {
        memories.sort_by(|left, right| {
            left.key
                .namespace
                .cmp(&right.key.namespace)
                .then_with(|| left.key.id.cmp(&right.key.id))
        });
        for memory in &memories {
            let namespace = memory
                .key
                .namespace
                .as_deref()
                .ok_or(MemoryError::Conflict)?;
            if !protocol::is_valid_namespace(namespace)
                || memory.key.id <= 0
                || memory.key.version == 0
            {
                return Err(MemoryError::Conflict);
            }
            validate_content(&memory.content, &self.limits)?;
        }

        let mut connection = self.open()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sqlite_error)?;
        prune_expired(&transaction, now_ms)?;
        let totals = totals(&transaction)?;
        let mut identities = load_all(&transaction)?
            .into_iter()
            .map(|memory| normalize_identity(&memory.content))
            .collect::<HashSet<_>>();
        let mut accepted = Vec::new();
        let mut skipped = 0;
        for memory in memories {
            let identity = normalize_identity(&memory.content);
            if !identities.insert(identity.clone()) {
                skipped += 1;
                continue;
            }
            accepted.push((memory.content, identity));
        }
        let resulting_records = totals.records.saturating_add(accepted.len() as u64);
        if resulting_records > self.limits.records as u64 {
            return Err(MemoryError::RecordCapacity {
                maximum: self.limits.records,
            });
        }
        let imported_bytes = accepted
            .iter()
            .map(|(content, _)| content.len() as u64)
            .sum::<u64>();
        if totals.content_bytes.saturating_add(imported_bytes)
            > self.limits.total_content_bytes as u64
        {
            return Err(MemoryError::ContentCapacity {
                maximum_bytes: self.limits.total_content_bytes,
            });
        }
        let probation_until_ms = now_ms.saturating_add(self.limits.probation_duration_ms);
        for (content, identity) in &accepted {
            let id = allocate_id(&transaction)?;
            transaction
                .execute(
                    "INSERT INTO memories (
                        id, content, normalized_identity, created_at_ms, updated_at_ms,
                        last_scanned_at_ms, scan_count, last_used_at_ms, use_count,
                        probation_until_ms, version
                     ) VALUES (?1, ?2, ?3, ?4, ?4, NULL, 0, NULL, 0, ?5, 1)",
                    params![id, content, identity, now_ms, probation_until_ms],
                )
                .map_err(sqlite_write_error)?;
        }
        transaction.commit().map_err(sqlite_write_error)?;
        Ok(MemoryImportReport {
            inserted: accepted.len(),
            skipped,
        })
    }

    pub(crate) fn list_local(&self, now_ms: i64) -> Result<Vec<MemoryRecord>, MemoryError> {
        let mut connection = self.open()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sqlite_error)?;
        prune_expired(&transaction, now_ms)?;
        let records = load_all(&transaction)?
            .into_iter()
            .filter(|memory| !contains_likely_secret(&memory.content))
            .map(MemoryRecord::from)
            .collect();
        transaction.commit().map_err(sqlite_error)?;
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
        let id = allocate_id(transaction)?;
        transaction
            .execute(
                "INSERT INTO memories (
                    id, content, normalized_identity, created_at_ms, updated_at_ms,
                    last_scanned_at_ms, scan_count, last_used_at_ms, use_count,
                    probation_until_ms, version
                 ) VALUES (?1, ?2, ?3, ?4, ?4, NULL, 0, NULL, 0, ?5, 1)",
                params![id, content, normalized_identity, now_ms, probation_until_ms],
            )
            .map_err(sqlite_write_error)?;
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
            .map_err(sqlite_error)?;
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
            .map_err(sqlite_write_error)?;
        load_one(transaction, key.id)?.ok_or(MemoryError::NotFound)
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

    pub(crate) fn open(&self) -> Result<Connection, MemoryError> {
        prepare_private_parent(&self.path)?;
        let mut connection = Connection::open(self.path.as_path()).map_err(sqlite_error)?;
        connection
            .busy_timeout(BUSY_TIMEOUT)
            .map_err(sqlite_error)?;
        let schema_version = connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .map_err(sqlite_error)?;
        if !matches!(schema_version, 0 | SCHEMA_VERSION) {
            return Err(MemoryError::UnsupportedSchemaVersion {
                found: schema_version,
                supported: SCHEMA_VERSION,
            });
        }
        connection
            .pragma_update(None, "journal_mode", "DELETE")
            .map_err(sqlite_error)?;
        connection
            .pragma_update(None, "page_size", DATABASE_PAGE_SIZE_BYTES as i64)
            .map_err(sqlite_error)?;
        let page_size = connection
            .query_row("PRAGMA page_size", [], |row| row.get::<_, i64>(0))
            .map_err(sqlite_error)? as usize;
        let maximum_pages = self.limits.database_bytes.div_ceil(page_size).max(1);
        connection
            .pragma_update(None, "max_page_count", maximum_pages as i64)
            .map_err(sqlite_error)?;
        // The allocator table is a backward-compatible schema-v1 extension. Older builds ignore
        // it; current builds retain identity history even when every memory row is deleted.
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sqlite_error)?;
        transaction
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
                 );
                 CREATE TABLE IF NOT EXISTS memory_metadata (
                    id INTEGER PRIMARY KEY CHECK (id = 1),
                    next_id INTEGER NOT NULL CHECK (next_id > 0)
                 );",
            )
            .map_err(sqlite_write_error)?;
        let maximum_id = transaction
            .query_row("SELECT COALESCE(MAX(id), 0) FROM memories", [], |row| {
                row.get::<_, i64>(0)
            })
            .map_err(sqlite_error)?;
        let next_id = maximum_id
            .checked_add(1)
            .ok_or(MemoryError::StorageCapacity)?;
        transaction
            .execute(
                "INSERT INTO memory_metadata (id, next_id) VALUES (?1, ?2)
                 ON CONFLICT(id) DO UPDATE SET next_id = excluded.next_id
                 WHERE memory_metadata.next_id < excluded.next_id",
                params![ALLOCATOR_ROW_ID, next_id],
            )
            .map_err(sqlite_write_error)?;
        transaction
            .execute_batch(INSTALL_ALLOCATOR_TRIGGER)
            .map_err(sqlite_write_error)?;
        if schema_version == 0 {
            transaction
                .pragma_update(None, "user_version", SCHEMA_VERSION)
                .map_err(sqlite_write_error)?;
        }
        transaction.commit().map_err(sqlite_write_error)?;
        Ok(connection)
    }

    async fn sync_local_snapshot(
        &self,
        memories: Vec<MemoryRecord>,
        now_ms: i64,
    ) -> Result<SyncReport, MemoryError> {
        let store = self.clone();
        run_local(move || store.sync_local_snapshot_blocking(&memories, now_ms)).await
    }

    fn sync_local_snapshot_blocking(
        &self,
        memories: &[MemoryRecord],
        now_ms: i64,
    ) -> Result<SyncReport, MemoryError> {
        let mut identities = HashSet::new();
        let mut ids = HashSet::new();
        let content_bytes = memories.iter().try_fold(0usize, |total, memory| {
            if !memory.key.is_local()
                || memory.key.id <= 0
                || memory.key.version == 0
                || !ids.insert(memory.key.id)
            {
                return Err(MemoryError::Conflict);
            }
            validate_content(&memory.content, &self.limits)?;
            if !identities.insert(normalize_identity(&memory.content)) {
                return Err(MemoryError::Duplicate);
            }
            total
                .checked_add(memory.content.len())
                .ok_or(MemoryError::ContentCapacity {
                    maximum_bytes: self.limits.total_content_bytes,
                })
        })?;
        if memories.len() > self.limits.records {
            return Err(MemoryError::RecordCapacity {
                maximum: self.limits.records,
            });
        }
        if content_bytes > self.limits.total_content_bytes {
            return Err(MemoryError::ContentCapacity {
                maximum_bytes: self.limits.total_content_bytes,
            });
        }

        let mut connection = self.open()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sqlite_error)?;
        prune_expired(&transaction, now_ms)?;
        let existing = load_all(&transaction)?;
        let previous = existing
            .iter()
            .cloned()
            .map(|memory| (memory.id, MemoryRecord::from(memory)))
            .collect::<HashMap<_, _>>();
        let incoming_ids = memories
            .iter()
            .map(|memory| memory.key.id)
            .collect::<HashSet<_>>();
        transaction
            .execute_batch("DROP TRIGGER memory_id_allocator")
            .map_err(sqlite_write_error)?;
        transaction
            .execute("DELETE FROM memories", [])
            .map_err(sqlite_write_error)?;
        let mut report = SyncReport {
            deleted: existing
                .iter()
                .filter(|memory| !incoming_ids.contains(&memory.id))
                .count(),
            ..SyncReport::default()
        };
        for memory in memories {
            observe_id(&transaction, memory.key.id)?;
            match previous.get(&memory.key.id) {
                Some(previous) if previous == memory => report.unchanged += 1,
                Some(_) => report.replaced += 1,
                None => report.inserted += 1,
            }
            transaction.execute(
                "INSERT INTO memories (id, content, normalized_identity, created_at_ms, updated_at_ms, last_scanned_at_ms, scan_count, last_used_at_ms, use_count, probation_until_ms, version) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![memory.key.id, memory.content, normalize_identity(&memory.content), memory.created_at_ms, memory.updated_at_ms, memory.last_scanned_at_ms, memory.scan_count as i64, memory.last_used_at_ms, memory.use_count as i64, memory.probation_until_ms, memory.key.version as i64],
            ).map_err(sqlite_write_error)?;
        }
        transaction
            .execute_batch(INSTALL_ALLOCATOR_TRIGGER)
            .map_err(sqlite_write_error)?;
        transaction.commit().map_err(sqlite_write_error)?;
        Ok(report)
    }

    async fn export_local_page(
        &self,
        cursor: Option<ExportCursor>,
        limit: usize,
        now_ms: i64,
    ) -> Result<(Vec<MemoryRecord>, Option<ExportCursor>), MemoryError> {
        let mut records = self.list(now_ms).await?;
        let after = cursor.map_or(0, |cursor| cursor.id);
        records.retain(|record| record.key.id > after);
        let limit = limit.clamp(1, protocol::MAX_EXPORT_PAGE_RECORDS);
        let has_more = records.len() > limit;
        records.truncate(limit);
        let next = has_more.then(|| ExportCursor {
            namespace: String::new(),
            id: records.last().expect("non-empty limited page").key.id,
        });
        Ok((records, next))
    }
}

impl MemoryStore for LocalMemoryStore {
    fn scan(
        &self,
        query: &str,
        limit: usize,
    ) -> impl Future<Output = Result<MemoryScan, MemoryError>> + Send {
        LocalMemoryStore::scan(self, query, limit, current_time_ms())
    }
    fn read(
        &self,
        ids: &[i64],
        keys: &[MemoryKey],
    ) -> impl Future<Output = Result<Vec<MemoryRecord>, MemoryError>> + Send {
        LocalMemoryStore::read(self, ids, keys, current_time_ms())
    }
    fn list(&self) -> impl Future<Output = Result<Vec<MemoryRecord>, MemoryError>> + Send {
        LocalMemoryStore::list(self, current_time_ms())
    }
    fn put(
        &self,
        content: &str,
        replacement: Option<MemoryKey>,
    ) -> impl Future<Output = Result<MemoryRecord, MemoryError>> + Send {
        LocalMemoryStore::put(self, content, replacement, current_time_ms())
    }
    fn delete(&self, key: MemoryKey) -> impl Future<Output = Result<(), MemoryError>> + Send {
        LocalMemoryStore::delete(self, key)
    }
    fn sync(
        &self,
        memories: &[MemoryRecord],
    ) -> impl Future<Output = Result<SyncReport, MemoryError>> + Send {
        let store = self.clone();
        let memories = memories.to_vec();
        let now_ms = current_time_ms();
        async move { store.sync_local_snapshot(memories, now_ms).await }
    }
    fn export_page(
        &self,
        _namespaces: Option<&[String]>,
        cursor: Option<&ExportCursor>,
        limit: usize,
    ) -> impl Future<Output = Result<(Vec<MemoryRecord>, Option<ExportCursor>), MemoryError>> + Send
    {
        let store = self.clone();
        let cursor = cursor.cloned();
        let now_ms = current_time_ms();
        async move { store.export_local_page(cursor, limit, now_ms).await }
    }
}

async fn run_local<T>(
    operation: impl FnOnce() -> Result<T, MemoryError> + Send + 'static,
) -> Result<T, MemoryError>
where
    T: Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|source| MemoryError::backend(LocalStoreError::Task(source)))?
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
        .map_err(sqlite_error)
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
        .map_err(sqlite_error)
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
        .map_err(sqlite_error)?;
    Ok(())
}

fn allocate_id(transaction: &Transaction<'_>) -> Result<i64, MemoryError> {
    let recorded = transaction
        .query_row(
            "SELECT next_id FROM memory_metadata WHERE id = ?1",
            [ALLOCATOR_ROW_ID],
            |row| row.get::<_, i64>(0),
        )
        .map_err(sqlite_error)?;
    let maximum_id = transaction
        .query_row("SELECT COALESCE(MAX(id), 0) FROM memories", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(sqlite_error)?;
    let id = recorded.max(
        maximum_id
            .checked_add(1)
            .ok_or(MemoryError::StorageCapacity)?,
    );
    id.checked_add(1).ok_or(MemoryError::StorageCapacity)?;
    Ok(id)
}

fn observe_id(transaction: &Transaction<'_>, id: i64) -> Result<(), MemoryError> {
    let next_id = id.checked_add(1).ok_or(MemoryError::StorageCapacity)?;
    transaction
        .execute(
            "UPDATE memory_metadata SET next_id = MAX(next_id, ?1) WHERE id = ?2",
            params![next_id, ALLOCATOR_ROW_ID],
        )
        .map_err(sqlite_write_error)?;
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
        .map_err(sqlite_error)?;
    let rows = statement
        .query_map([], row_to_memory)
        .map_err(sqlite_error)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)
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
        .map_err(sqlite_error)
}

fn row_to_memory(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredMemory> {
    Ok(StoredMemory {
        namespace: None,
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

fn sqlite_error(source: rusqlite::Error) -> MemoryError {
    let retryable = matches!(
        &source,
        rusqlite::Error::SqliteFailure(error, _)
            if matches!(error.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    );
    if retryable {
        MemoryError::unavailable(source)
    } else {
        MemoryError::backend(source)
    }
}

fn sqlite_write_error(source: rusqlite::Error) -> MemoryError {
    match &source {
        rusqlite::Error::SqliteFailure(error, _) if error.code == ErrorCode::DiskFull => {
            MemoryError::StorageCapacity
        }
        _ => sqlite_error(source),
    }
}

pub(crate) fn prepare_private_parent(path: &Path) -> Result<(), MemoryError> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    if parent.as_os_str().is_empty() {
        return Ok(());
    }
    fs::create_dir_all(parent)
        .map_err(|source| MemoryError::backend(LocalStoreError::Directory(source)))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .map_err(|source| MemoryError::backend(LocalStoreError::Directory(source)))?;
    }
    Ok(())
}

pub(crate) fn validate_content(content: &str, limits: &MemoryLimits) -> Result<(), MemoryError> {
    if content.trim().is_empty() {
        return Err(MemoryError::EmptyContent);
    }
    if content.len() > limits.content_bytes {
        return Err(MemoryError::ContentTooLarge {
            maximum_bytes: limits.content_bytes,
        });
    }
    if contains_likely_secret(content) {
        return Err(MemoryError::SecretRejected);
    }
    Ok(())
}

#[cfg(test)]
mod allocator_tests {
    use super::*;

    #[test]
    fn allocation_reconciles_rows_inserted_by_a_legacy_writer() {
        let directory = tempfile::tempdir().unwrap();
        let store = LocalMemoryStore::new(directory.path().join("memory.sqlite3"));
        let mut connection = store.open().unwrap();
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        transaction
            .execute(
                "INSERT INTO memories (
                    id, content, normalized_identity, created_at_ms, updated_at_ms, version
                 ) VALUES (1, 'legacy', 'legacy', 1, 1, 1)",
                [],
            )
            .unwrap();

        assert_eq!(allocate_id(&transaction).unwrap(), 2);
    }

    #[test]
    fn legacy_writers_cannot_reuse_retired_ids() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("memory.sqlite3");
        let store = LocalMemoryStore::new(&path);
        let memory = store.put_local("retired", None, 1).unwrap();
        store.delete_local(memory.key).unwrap();

        let legacy = Connection::open(path).unwrap();
        assert!(
            legacy
                .execute(
                    "INSERT INTO memories (
                        content, normalized_identity, created_at_ms, updated_at_ms, version
                     ) VALUES ('legacy', 'legacy', 2, 2, 1)",
                    [],
                )
                .is_err()
        );
    }
}
