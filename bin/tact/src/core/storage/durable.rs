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

#![cfg_attr(
    not(test),
    allow(dead_code, reason = "sequences are adopted by the state store")
)]

use super::{StorageError, conversation_search::conversation_text, query};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use std::path::Path;

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

    fn started(session_id: &str) -> Arc<TranscriptRecord> {
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

    fn prompt(sequence: u64, text: &str) -> Arc<TranscriptRecord> {
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

    fn assistant(
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
