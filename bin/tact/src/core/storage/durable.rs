//! Additive tables that let the session database serve nanocode's durability contract.
//!
//! A session's contract events are exactly its `events` rows with a non-null `seq`. The
//! sequence of a session is contiguous from 1 and append-only: streamed assistant drafts, which
//! the stream's final message deletes, never receive a sequence, and no other row is ever
//! deleted. Rows written before a session joined the contract, or by binaries that predate these
//! tables, are numbered in `event_id` order the next time an owner writes the session; such
//! adopted rows report revision 0 because no contract commit wrote them.
//!
//! The schema is additive and leaves the format version unchanged, so older binaries keep reading
//! and writing the same database.

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Tact reads sessions through its own queries; this serves contract readers"
    )
)]
mod reader;
mod store;

use super::{
    SessionStorage, StorageError, compress_resume_state, conversation_search::conversation_text,
    decode_record, from_sql_u64, query, write_record, write_resume_state,
};
use crate::core::transcript::{LocalKind, TranscriptRecord};
use nanocode::{
    agent::events::AgentEventKind,
    durability::{
        Committed, EventCursor, EventHit, EventHits, EventOrder, EventPage, EventQuery, EventRange,
        IndexedState, OwnedState, OwnerId, OwnerToken, StateCommit, StateCursor, StatePage,
        StateQuery, StateSummary, StoreError, StoreEvent, StoreRecord, StoreUpdate, StoredEvent,
        StoredState, SummaryUpdate,
    },
};
#[cfg(test)]
pub(crate) use reader::SqliteStateReader;
use rusqlite::{
    Connection, OptionalExtension, Row, Transaction, TransactionBehavior, params, params_from_iter,
    types::Value,
};
use std::{collections::HashMap, fmt::Display, io, path::Path, thread, time::Duration};
pub(crate) use store::{HostBatch, HostRow, SqliteStateStore};

/// Creates the durability tables and the event sequence columns once per database. The checks
/// repeat inside an immediate transaction so concurrent first opens add each column once.
pub(super) fn ensure_schema(connection: &Connection, path: &Path) -> Result<(), StorageError> {
    if schema_exists(connection, path)? {
        return Ok(());
    }
    let transaction = Transaction::new_unchecked(connection, TransactionBehavior::Immediate)
        .map_err(|source| query(path, source))?;
    transaction
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS durable_states(\n\
                 state_id TEXT PRIMARY KEY, revision INTEGER NOT NULL CHECK(revision > 0),\n\
                 payload TEXT NOT NULL\n\
             ) STRICT;\n\
             CREATE TABLE IF NOT EXISTS durable_owners(\n\
                 state_id TEXT PRIMARY KEY, owner_id TEXT NOT NULL, fence INTEGER NOT NULL\n\
             ) STRICT;\n\
             CREATE TABLE IF NOT EXISTS durable_records(\n\
                 state_id TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL,\n\
                 PRIMARY KEY(state_id, key)\n\
             ) STRICT, WITHOUT ROWID;\n\
             CREATE TABLE IF NOT EXISTS durable_summaries(\n\
                 state_id TEXT PRIMARY KEY, revision INTEGER NOT NULL,\n\
                 live INTEGER NOT NULL CHECK(live IN (0, 1)), scope TEXT, parent TEXT, title TEXT,\n\
                 preview TEXT, attributes TEXT NOT NULL, updated_at_ms INTEGER NOT NULL,\n\
                 value TEXT NOT NULL\n\
             ) STRICT;\n\
             CREATE INDEX IF NOT EXISTS durable_summaries_by_recency\n\
                 ON durable_summaries(updated_at_ms DESC, state_id DESC) WHERE live = 1;\n\
             CREATE INDEX IF NOT EXISTS durable_summaries_by_scope\n\
                 ON durable_summaries(scope, updated_at_ms DESC, state_id DESC) WHERE live = 1;",
        )
        .map_err(|source| query(path, source))?;
    if !has_sequence_columns(&transaction, path)? {
        transaction
            .execute_batch(
                "ALTER TABLE events ADD COLUMN seq INTEGER;\n\
                 ALTER TABLE events ADD COLUMN revision INTEGER;",
            )
            .map_err(|source| query(path, source))?;
    }
    transaction
        .execute_batch(
            "CREATE UNIQUE INDEX IF NOT EXISTS events_by_seq\n\
                 ON events(session_id, seq) WHERE seq IS NOT NULL;\n\
             CREATE TABLE IF NOT EXISTS durable_event_sequences(\n\
                 state_id TEXT PRIMARY KEY, last_seq INTEGER NOT NULL,\n\
                 sequenced_through INTEGER NOT NULL\n\
             ) STRICT;",
        )
        .map_err(|source| query(path, source))?;
    transaction.commit().map_err(|source| query(path, source))
}

/// Whether every durability table and column exists. The sequence table is created last in the
/// same transaction as the rest, so its presence implies the others.
pub(super) fn schema_exists(connection: &Connection, path: &Path) -> Result<bool, StorageError> {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema\n\
             WHERE type = 'table' AND name = 'durable_event_sequences')",
            [],
            |row| row.get(0),
        )
        .map_err(|source| query(path, source))
}

fn has_sequence_columns(connection: &Connection, path: &Path) -> Result<bool, StorageError> {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('events') WHERE name = 'seq')",
            [],
            |row| row.get(0),
        )
        .map_err(|source| query(path, source))
}

/// The end of a session's event sequence: the last assigned `seq` and the largest `event_id`
/// already considered for numbering.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct EventSequence {
    pub(super) last_seq: i64,
    pub(super) sequenced_through: i64,
}

impl EventSequence {
    /// Numbers the session's rows written since its sequence was last saved, other than drafts,
    /// and indexes their conversation text so that an event's text is fixed once it has a
    /// sequence. Runs inside the owner's write transaction.
    pub(super) fn adopt(
        transaction: &Transaction<'_>,
        path: &Path,
        state_id: &str,
    ) -> Result<Self, StorageError> {
        let stored = transaction
            .query_row(
                "SELECT last_seq, sequenced_through FROM durable_event_sequences\n\
                 WHERE state_id = ?1",
                [state_id],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(|source| query(path, source))?;
        let (last_seq, sequenced_through) = stored.unwrap_or_default();
        let numbered = transaction
            .execute(
                "WITH pending AS (\n\
                     SELECT event_id, ROW_NUMBER() OVER (ORDER BY event_id) AS n FROM events\n\
                     WHERE session_id = ?1 AND event_id > ?2\n\
                     AND assistant_stream IS NULL AND seq IS NULL\n\
                 )\n\
                 UPDATE events SET seq = ?3 + pending.n, revision = 0\n\
                 FROM pending WHERE events.event_id = pending.event_id",
                params![state_id, sequenced_through, last_seq],
            )
            .map_err(|source| query(path, source))?;
        let text = conversation_text("e");
        transaction
            .execute(
                &format!(
                    "INSERT OR IGNORE INTO conversation_messages(event_id, session_id, text)\n\
                     SELECT event_id, session_id, text FROM (\n\
                         SELECT e.event_id, e.session_id, {text} AS text FROM events e\n\
                         WHERE e.session_id = ?1 AND e.event_id > ?2 AND e.seq IS NOT NULL\n\
                     ) WHERE typeof(text) = 'text' AND text != ''"
                ),
                params![state_id, sequenced_through],
            )
            .map_err(|source| query(path, source))?;
        let last_seq = i64::try_from(numbered)
            .ok()
            .and_then(|numbered| last_seq.checked_add(numbered))
            .ok_or(StorageError::SequenceOverflow)?;
        let sequence = Self {
            last_seq,
            sequenced_through: last_event_id(transaction, path, state_id, sequenced_through)?,
        };
        sequence.save(transaction, path, state_id)?;
        Ok(sequence)
    }

    pub(super) fn save(
        self,
        transaction: &Transaction<'_>,
        path: &Path,
        state_id: &str,
    ) -> Result<(), StorageError> {
        transaction
            .execute(
                "INSERT INTO durable_event_sequences(state_id, last_seq, sequenced_through)\n\
                 VALUES (?1, ?2, ?3)\n\
                 ON CONFLICT(state_id) DO UPDATE SET\n\
                     last_seq = excluded.last_seq, sequenced_through = excluded.sequenced_through",
                params![state_id, self.last_seq, self.sequenced_through],
            )
            .map_err(|source| query(path, source))?;
        Ok(())
    }
}

fn last_event_id(
    transaction: &Transaction<'_>,
    path: &Path,
    state_id: &str,
    floor: i64,
) -> Result<i64, StorageError> {
    transaction
        .query_row(
            "SELECT COALESCE(MAX(event_id), ?2) FROM events WHERE session_id = ?1",
            params![state_id, floor],
            |row| row.get(0),
        )
        .map_err(|source| query(path, source))
}

/// A contract event's place in its session's sequence.
#[derive(Clone, Copy, Debug)]
pub(super) struct EventPosition {
    pub(super) seq: i64,
    pub(super) revision: i64,
}

/// How long a writer waits before retrying a transaction that another connection's write lock
/// blocked beyond the busy timeout.
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(50);

/// Splits transcript records into a commit's contract events and the host rows that place them
/// among streamed assistant drafts, which stay outside the contract.
pub(crate) fn transcript_commit<'a>(
    records: impl IntoIterator<Item = &'a TranscriptRecord>,
) -> Result<(Vec<StoreEvent>, Vec<HostRow>), serde_json::Error> {
    let mut events = Vec::new();
    let mut rows = Vec::new();
    for record in records {
        let body = serde_json::to_string(record)?;
        if record.agent_kind() == Some(AgentEventKind::AssistantDelta) {
            rows.push(HostRow::Draft(body.into_bytes()));
        } else {
            events.push(store_event(record, body)?);
            rows.push(HostRow::Event);
        }
    }
    Ok((events, rows))
}

/// The contract event of a transcript record. `body` is the record's JSON encoding, which is
/// stored verbatim; the other fields are derived from the record, so storage never duplicates them.
fn store_event(record: &TranscriptRecord, body: String) -> Result<StoreEvent, serde_json::Error> {
    Ok(StoreEvent {
        kind: record.kind().as_str().to_owned(),
        at_ms: record.recorded_at_unix_ms(),
        body,
        text: record.conversation_text()?,
    })
}

/// A transcript row of a commit, in the order the host recorded it.
struct PlannedRow<'a> {
    record: TranscriptRecord,
    encoded: &'a [u8],
    /// `None` for a draft, which is not a contract event; otherwise the event's validated text.
    event_text: Option<Option<String>>,
}

/// Decodes and checks a commit's rows before anything is written. Every event must be the
/// contract event of the transcript record in its body, and never a streamed draft, because a
/// stream's final message deletes its drafts while contract events are immutable. The host batch,
/// when present, places those events among its drafts, which must all be streamed drafts, and must
/// name each event exactly once.
fn plan_rows<'a>(
    events: &'a [StoreEvent],
    host: Option<&'a HostBatch>,
) -> Result<Vec<PlannedRow<'a>>, StoreError> {
    let mut planned = events.iter().map(|event| {
        let record = decode_record(event.body.as_bytes()).map_err(not_committed)?;
        if is_draft(&record) {
            return Err(StoreError::NotCommitted(
                "a streamed assistant draft cannot be a contract event".to_owned(),
            ));
        }
        let text = record.conversation_text().map_err(not_committed)?;
        if event.kind != record.kind().as_str()
            || event.at_ms != record.recorded_at_unix_ms()
            || event.text != text
        {
            return Err(StoreError::NotCommitted(format!(
                "event of kind {} does not describe the transcript record in its body",
                event.kind
            )));
        }
        Ok(PlannedRow {
            record,
            encoded: event.body.as_bytes(),
            event_text: Some(text),
        })
    });
    let Some(host) = host else {
        return planned.collect();
    };
    let misaligned =
        || StoreError::NotCommitted("the host batch does not match the committed events".into());
    let mut rows = Vec::with_capacity(host.rows.len());
    for row in &host.rows {
        rows.push(match row {
            HostRow::Event => planned.next().ok_or_else(misaligned)??,
            HostRow::Draft(encoded) => {
                let record = decode_record(encoded).map_err(not_committed)?;
                if !is_draft(&record) {
                    return Err(StoreError::NotCommitted(format!(
                        "a host draft must be a streamed assistant draft, not {}",
                        record.kind()
                    )));
                }
                PlannedRow {
                    record,
                    encoded,
                    event_text: None,
                }
            }
        });
    }
    if planned.next().is_some() {
        return Err(misaligned());
    }
    Ok(rows)
}

impl SessionStorage {
    /// Begins a write transaction, waiting out other connections' write locks for as long as they
    /// last, as the transcript writer always has.
    fn begin_write(&self) -> Result<Transaction<'_>, StorageError> {
        loop {
            match Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate) {
                Ok(transaction) => return Ok(transaction),
                Err(source) => {
                    let error = query(&self.path, source);
                    if !error.is_lock_contention() {
                        return Err(error);
                    }
                    thread::sleep(LOCK_RETRY_DELAY);
                }
            }
        }
    }

    fn acquire_state(
        &self,
        state_id: &str,
        owner_id: OwnerId,
        update: Option<StoreUpdate>,
    ) -> Result<OwnedState, StoreError> {
        let path = &self.path;
        let transaction = self.begin_write().map_err(not_committed)?;
        let fence = transaction
            .query_row(
                "SELECT fence FROM durable_owners WHERE state_id = ?1",
                [state_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(rejected(path))?
            .map_or(Some(1), |fence| from_sql_u64(fence).checked_add(1))
            .filter(|fence| i64::try_from(*fence).is_ok())
            .ok_or_else(|| StoreError::NotCommitted("owner fence overflow".to_owned()))?;
        transaction
            .execute(
                "INSERT INTO durable_owners(state_id, owner_id, fence) VALUES (?1, ?2, ?3)\n\
                 ON CONFLICT(state_id) DO UPDATE SET\n\
                     owner_id = excluded.owner_id, fence = excluded.fence",
                params![
                    state_id,
                    owner_id.as_str(),
                    sql_integer(fence, "owner fence")?
                ],
            )
            .map_err(rejected(path))?;
        EventSequence::adopt(&transaction, path, state_id).map_err(not_committed)?;
        let mut owned = OwnedState {
            owner: OwnerToken::new(owner_id, fence),
            state: read_state(&transaction, path, state_id).map_err(not_committed)?,
        };
        let replacement = match update.map(|update| update(&owned.state)).transpose() {
            Ok(replacement) => replacement.flatten(),
            Err(error) => {
                return Err(StoreError::NotCommitted(format!(
                    "state update failed: {error}"
                )));
            }
        };
        if let Some(replacement) = replacement {
            let revision = next_revision(owned.state.revision)?;
            write_records(&transaction, path, state_id, &replacement.records)?;
            write_state(&transaction, path, state_id, revision, &replacement.payload)?;
            owned.state = StoredState {
                revision,
                payload: Some(replacement.payload),
            };
        }
        transaction.commit().map_err(uncertain(path))?;
        Ok(owned)
    }

    /// Commits a fenced state transition, its records and events, the host's drafts and resume
    /// state, and the summary, all in one transaction.
    fn commit_state(
        &mut self,
        state_id: &str,
        owner: &OwnerToken,
        expected_revision: u64,
        commit: StateCommit<'_>,
        host: Option<&HostBatch>,
    ) -> Result<Committed, StoreError> {
        let rows = plan_rows(commit.events, host)?;
        let compressed_state = host
            .and_then(|host| host.resume_state.as_deref())
            .map(compress_resume_state)
            .transpose()
            .map_err(not_committed)?;
        let path = &self.path;
        let transaction = self.begin_write().map_err(not_committed)?;
        check_owner(&transaction, path, state_id, owner)?;
        let current = read_state(&transaction, path, state_id)
            .map_err(not_committed)?
            .revision;
        if current != expected_revision {
            return Err(StoreError::Conflict {
                expected: expected_revision,
                actual: current,
            });
        }
        let revision = next_revision(current)?;
        let stored_revision = sql_integer(revision, "state revision")?;
        let sequence = EventSequence::adopt(&transaction, path, state_id).map_err(not_committed)?;
        let appended = i64::try_from(commit.events.len())
            .ok()
            .and_then(|count| sequence.last_seq.checked_add(count));
        if appended.is_none() {
            return Err(StoreError::NotCommitted(
                "event sequence overflow".to_owned(),
            ));
        }
        write_records(&transaction, path, state_id, commit.records)?;
        let mut active_turn = self.active_turns.get(state_id).copied();
        let mut last_seq = sequence.last_seq;
        for row in &rows {
            let position = row.event_text.is_some().then(|| {
                last_seq += 1;
                EventPosition {
                    seq: last_seq,
                    revision: stored_revision,
                }
            });
            let event_id = write_record(
                &transaction,
                path,
                state_id,
                &mut active_turn,
                &row.record,
                row.encoded,
                position,
            )
            .map_err(not_committed)?;
            if let Some(Some(text)) = &row.event_text {
                index_conversation_text(&transaction, path, state_id, event_id, text)?;
            }
        }
        if let Some(compressed) = compressed_state {
            write_resume_state(&transaction, path, state_id, &compressed).map_err(not_committed)?;
        }
        EventSequence {
            last_seq,
            sequenced_through: last_event_id(
                &transaction,
                path,
                state_id,
                sequence.sequenced_through,
            )
            .map_err(not_committed)?,
        }
        .save(&transaction, path, state_id)
        .map_err(not_committed)?;
        write_state(&transaction, path, state_id, revision, commit.payload)?;
        write_summary(&transaction, path, state_id, revision, commit.summary)?;
        transaction.commit().map_err(uncertain(path))?;
        if let Some(active_turn) = active_turn {
            self.active_turns.insert(state_id.to_owned(), active_turn);
        }
        Ok(Committed {
            revision,
            first_event: (!commit.events.is_empty()).then(|| from_sql_u64(sequence.last_seq) + 1),
        })
    }

    fn state_ids(&self, limit: usize) -> Result<Vec<String>, StorageError> {
        let mut statement = self
            .connection
            .prepare("SELECT state_id FROM durable_states ORDER BY rowid DESC LIMIT ?1")
            .map_err(|source| query(&self.path, source))?;
        statement
            .query_map([i64::try_from(limit).unwrap_or(i64::MAX)], |row| row.get(0))
            .map_err(|source| query(&self.path, source))?
            .map(|row| row.map_err(|source| query(&self.path, source)))
            .collect()
    }

    /// Reads with a connection that may predate the durability tables, in which case no state
    /// exists yet.
    fn read_stored_state(&self, state_id: &str) -> Result<StoredState, StorageError> {
        if !schema_exists(&self.connection, &self.path)? {
            return Ok(StoredState::default());
        }
        read_state(&self.connection, &self.path, state_id)
    }

    fn read_stored_records(
        &self,
        state_id: &str,
        keys: &[String],
    ) -> Result<Vec<Option<String>>, StorageError> {
        if !schema_exists(&self.connection, &self.path)? {
            return Ok(vec![None; keys.len()]);
        }
        let mut statement = self
            .connection
            .prepare("SELECT value FROM durable_records WHERE state_id = ?1 AND key = ?2")
            .map_err(|source| query(&self.path, source))?;
        keys.iter()
            .map(|key| {
                statement
                    .query_row(params![state_id, key], |row| row.get(0))
                    .optional()
                    .map_err(|source| query(&self.path, source))
            })
            .collect()
    }
}

fn read_state(
    connection: &Connection,
    path: &Path,
    state_id: &str,
) -> Result<StoredState, StorageError> {
    Ok(connection
        .query_row(
            "SELECT revision, payload FROM durable_states WHERE state_id = ?1",
            [state_id],
            |row| {
                Ok(StoredState {
                    revision: from_sql_u64(row.get(0)?),
                    payload: Some(row.get(1)?),
                })
            },
        )
        .optional()
        .map_err(|source| query(path, source))?
        .unwrap_or_default())
}

fn check_owner(
    transaction: &Transaction<'_>,
    path: &Path,
    state_id: &str,
    owner: &OwnerToken,
) -> Result<(), StoreError> {
    let current = transaction
        .query_row(
            "SELECT owner_id, fence FROM durable_owners WHERE state_id = ?1",
            [state_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()
        .map_err(rejected(path))?
        .is_some_and(|(owner_id, fence)| {
            owner_id == owner.owner_id().as_str() && u64::try_from(fence) == Ok(owner.fence())
        });
    if current {
        Ok(())
    } else {
        Err(StoreError::Fenced)
    }
}

/// Converts a contract counter or time to SQLite's integer domain, rejecting rather than clamping
/// a value that SQLite cannot store.
fn sql_integer(value: u64, name: &str) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|_| {
        StoreError::NotCommitted(format!(
            "{name} {value} exceeds the largest storable integer"
        ))
    })
}

fn is_draft(record: &TranscriptRecord) -> bool {
    record.agent_kind() == Some(AgentEventKind::AssistantDelta)
}

/// Indexes a contract event's validated text. The conversation trigger recognizes only compactly
/// encoded bodies, so without this a valid body in another encoding would read back without its
/// text and escape conversation search.
fn index_conversation_text(
    transaction: &Transaction<'_>,
    path: &Path,
    state_id: &str,
    event_id: i64,
    text: &str,
) -> Result<(), StoreError> {
    if text.is_empty() {
        return Ok(());
    }
    transaction
        .execute(
            "INSERT OR IGNORE INTO conversation_messages(event_id, session_id, text)
             VALUES (?1, ?2, ?3)",
            params![event_id, state_id, text],
        )
        .map_err(rejected(path))?;
    Ok(())
}

fn next_revision(revision: u64) -> Result<u64, StoreError> {
    revision
        .checked_add(1)
        .filter(|revision| i64::try_from(*revision).is_ok())
        .ok_or_else(|| StoreError::NotCommitted("state revision overflow".to_owned()))
}

/// Inserts records that are absent and rejects, before writing any, a record whose content
/// differs from the stored or earlier staged value of its key.
fn write_records(
    transaction: &Transaction<'_>,
    path: &Path,
    state_id: &str,
    records: &[StoreRecord],
) -> Result<(), StoreError> {
    let mut staged = HashMap::new();
    for record in records {
        let stored = transaction
            .query_row(
                "SELECT value FROM durable_records WHERE state_id = ?1 AND key = ?2",
                params![state_id, record.key],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(rejected(path))?;
        let retained = stored
            .as_deref()
            .or_else(|| staged.get(record.key.as_str()).copied());
        if retained.is_some_and(|value| value != record.value) {
            return Err(StoreError::NotCommitted(format!(
                "durability record {} has conflicting content",
                record.key
            )));
        }
        staged.insert(record.key.as_str(), record.value.as_str());
    }
    for record in records {
        transaction
            .execute(
                "INSERT OR IGNORE INTO durable_records(state_id, key, value) VALUES (?1, ?2, ?3)",
                params![state_id, record.key, record.value],
            )
            .map_err(rejected(path))?;
    }
    Ok(())
}

fn write_state(
    transaction: &Transaction<'_>,
    path: &Path,
    state_id: &str,
    revision: u64,
    payload: &str,
) -> Result<(), StoreError> {
    transaction
        .execute(
            "INSERT INTO durable_states(state_id, revision, payload) VALUES (?1, ?2, ?3)\n\
             ON CONFLICT(state_id) DO UPDATE SET\n\
                 revision = excluded.revision, payload = excluded.payload",
            params![state_id, sql_integer(revision, "state revision")?, payload],
        )
        .map_err(rejected(path))?;
    Ok(())
}

/// Applies a summary only when its revision is newer than the stored one, and keeps a removed
/// summary as a tombstone so that a delayed older one cannot resurrect the state.
fn write_summary(
    transaction: &Transaction<'_>,
    path: &Path,
    state_id: &str,
    revision: u64,
    summary: &SummaryUpdate,
) -> Result<(), StoreError> {
    let (live, summary) = match summary {
        SummaryUpdate::Unchanged => return Ok(()),
        SummaryUpdate::Set(summary) => (true, Some(summary)),
        SummaryUpdate::Remove => (false, None),
    };
    let attributes = summary
        .map(|summary| serde_json::to_string(&summary.attributes))
        .transpose()
        .map_err(not_committed)?;
    transaction
        .execute(
            "INSERT INTO durable_summaries(\n\
                 state_id, revision, live, scope, parent, title, preview, attributes,\n\
                 updated_at_ms, value\n\
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, COALESCE(?8, '{}'), ?9, COALESCE(?10, ''))\n\
             ON CONFLICT(state_id) DO UPDATE SET\n\
                 revision = excluded.revision, live = excluded.live, scope = excluded.scope,\n\
                 parent = excluded.parent, title = excluded.title, preview = excluded.preview,\n\
                 attributes = excluded.attributes, updated_at_ms = excluded.updated_at_ms,\n\
                 value = excluded.value\n\
             WHERE excluded.revision > durable_summaries.revision",
            params![
                state_id,
                sql_integer(revision, "state revision")?,
                live,
                summary.and_then(|summary| summary.scope.as_deref()),
                summary.and_then(|summary| summary.parent.as_deref()),
                summary.and_then(|summary| summary.title.as_deref()),
                summary.and_then(|summary| summary.preview.as_deref()),
                attributes,
                summary
                    .map(|summary| sql_integer(summary.updated_at_ms, "summary time"))
                    .transpose()?
                    .unwrap_or_default(),
                summary.map(|summary| summary.value.as_str()),
            ],
        )
        .map_err(rejected(path))?;
    Ok(())
}

/// A failure before the transaction commits: SQLite rolls the transaction back, so nothing
/// changed.
fn not_committed(error: impl Display) -> StoreError {
    StoreError::NotCommitted(error.to_string())
}

fn rejected(path: &Path) -> impl Fn(rusqlite::Error) -> StoreError + '_ {
    move |source| not_committed(query(path, source))
}

/// A failed commit may or may not have reached the database.
fn uncertain(path: &Path) -> impl Fn(rusqlite::Error) -> StoreError + '_ {
    move |source| StoreError::Backend(query(path, source).to_string())
}

#[cfg(test)]
mod tests {
    use super::{EventSequence, schema_exists};
    use crate::{
        app::config::{ReasoningEffort, ReasoningMode, Speed},
        core::{
            storage::{SessionStorage, configure, database_path, initialize},
            transcript::{LocalEvent, SessionStarted, TranscriptRecord, TurnId, UserSubmitted},
        },
    };
    use nanocode::agent::events::{AgentEvent, AgentEventKind};
    use rusqlite::{Connection, params};
    use serde_json::{json, value::to_raw_value};
    use std::{fs, path::Path, sync::Arc, thread};
    use tempfile::tempdir;

    pub(super) fn started(session_id: &str) -> Arc<TranscriptRecord> {
        Arc::new(
            TranscriptRecord::from_local(
                1,
                100,
                LocalEvent::SessionStarted(SessionStarted {
                    session_id: session_id.to_owned(),
                    parent_session_id: None,
                    parent_sequence: None,
                    model: "model".to_owned(),
                    effort: ReasoningEffort::Medium,
                    reasoning_mode: ReasoningMode::Standard,
                    speed: Speed::Standard,
                    workspace: "/work".into(),
                    application_version: "test".to_owned(),
                }),
            )
            .unwrap(),
        )
    }

    pub(super) fn prompt(sequence: u64, text: &str) -> Arc<TranscriptRecord> {
        Arc::new(
            TranscriptRecord::from_local(
                sequence,
                100 + sequence,
                LocalEvent::UserSubmitted(UserSubmitted {
                    id: TurnId::new(sequence),
                    text: text.to_owned(),
                }),
            )
            .unwrap(),
        )
    }

    pub(super) fn assistant(
        sequence: u64,
        kind: AgentEventKind,
        model_call_index: u32,
        text: &str,
    ) -> Arc<TranscriptRecord> {
        Arc::new(TranscriptRecord::from_agent(
            sequence,
            100 + sequence,
            AgentEvent {
                protocol_version: 1,
                request_id: Arc::from("request"),
                seq: sequence,
                kind,
                payload: to_raw_value(&json!({
                    "model_call_index": model_call_index,
                    "item_id": "message",
                    "phase": "final_answer",
                    "text": text,
                }))
                .unwrap()
                .into(),
            },
        ))
    }

    /// A database in the shape every earlier binary created, holding one session's records.
    fn legacy_database(config: &Path, session_id: &str, records: &[Arc<TranscriptRecord>]) {
        let path = database_path(config);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let connection = Connection::open(&path).unwrap();
        configure(&connection, &path).unwrap();
        initialize(&connection, &path).unwrap();
        connection
            .execute(
                "INSERT INTO sessions(session_id, workspace, model, effort, reasoning_mode,\n\
                 fast_mode, application_version, started_at_ms, updated_at_ms, preview)\n\
                 VALUES (?1, '/work', 'model', '\"medium\"', '\"standard\"', 0, 'test', 100, 100,\n\
                 'No user prompt')",
                [session_id],
            )
            .unwrap();
        for record in records {
            connection
                .execute(
                    "INSERT INTO events(session_id, record_json) VALUES (?1, ?2)",
                    params![session_id, serde_json::to_vec(record.as_ref()).unwrap()],
                )
                .unwrap();
        }
    }

    fn sequences(config: &Path, session_id: &str) -> Vec<(Option<i64>, Option<i64>)> {
        let connection = Connection::open(database_path(config)).unwrap();
        let mut statement = connection
            .prepare("SELECT seq, revision FROM events WHERE session_id = ?1 ORDER BY event_id")
            .unwrap();
        statement
            .query_map([session_id], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    fn adopt(storage: &mut SessionStorage, session_id: &str) -> EventSequence {
        let path = storage.path.clone();
        let transaction = storage.connection.transaction().unwrap();
        let sequence = EventSequence::adopt(&transaction, &path, session_id).unwrap();
        transaction.commit().unwrap();
        sequence
    }

    #[test]
    fn adding_the_durable_schema_keeps_existing_data_and_the_format_version() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        legacy_database(&config, "legacy", &[started("legacy"), prompt(2, "hello")]);

        let reader = SessionStorage::open_read_only(&config).unwrap().unwrap();
        assert!(!schema_exists(&reader.connection, &reader.path).unwrap());
        assert_eq!(reader.load_records("legacy").unwrap().len(), 2);
        drop(reader);

        for _ in 0..2 {
            let storage = SessionStorage::open(&config).unwrap();
            assert!(schema_exists(&storage.connection, &storage.path).unwrap());
        }
        let reader = SessionStorage::open_read_only(&config).unwrap().unwrap();
        let version: i32 = reader
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 2);
        assert_eq!(reader.load_records("legacy").unwrap().len(), 2);
        assert_eq!(
            reader
                .list_sessions(Path::new("/work"), false)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(sequences(&config, "legacy"), [(None, None), (None, None)]);
    }

    #[test]
    fn concurrent_first_opens_add_the_durable_schema_once() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        legacy_database(&config, "legacy", &[started("legacy")]);

        let openers = (0..4)
            .map(|_| {
                let config = config.clone();
                thread::spawn(move || SessionStorage::open(&config).map(drop))
            })
            .collect::<Vec<_>>();
        for opener in openers {
            opener.join().unwrap().unwrap();
        }
        let storage = SessionStorage::open(&config).unwrap();
        assert!(schema_exists(&storage.connection, &storage.path).unwrap());
    }

    #[test]
    fn adoption_numbers_rows_in_order_skips_drafts_and_continues_after_older_writers() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let mut storage = SessionStorage::open(&config).unwrap();
        storage
            .append_records(
                "session",
                &[
                    started("session"),
                    prompt(2, "first"),
                    assistant(3, AgentEventKind::AssistantDelta, 1, "draft"),
                    assistant(4, AgentEventKind::AssistantMessage, 2, "reply"),
                ],
            )
            .unwrap();

        let sequence = adopt(&mut storage, "session");
        assert_eq!(sequence.last_seq, 3);
        assert_eq!(
            sequences(&config, "session"),
            [
                (Some(1), Some(0)),
                (Some(2), Some(0)),
                (None, None),
                (Some(3), Some(0))
            ]
        );
        assert_eq!(adopt(&mut storage, "session"), sequence);

        // A binary that predates the sequence columns appends without numbering its rows.
        storage
            .append_records("session", &[prompt(5, "second")])
            .unwrap();
        assert_eq!(adopt(&mut storage, "session").last_seq, 4);
        assert_eq!(
            sequences(&config, "session").last(),
            Some(&(Some(4), Some(0)))
        );
        let indexed: Vec<String> = {
            let mut statement = storage
                .connection
                .prepare(
                    "SELECT text FROM conversation_messages WHERE session_id = 'session'\n\
                     ORDER BY event_id",
                )
                .unwrap();
            statement
                .query_map([], |row| row.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        assert_eq!(indexed, ["first", "reply", "second"]);
    }
}
