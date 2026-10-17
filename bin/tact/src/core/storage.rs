//! SQLite-backed V2 session persistence.

mod conversation_search;
mod durable;

use crate::{
    app::config::{ReasoningEffort, ReasoningMode, Speed},
    core::{
        titles::{GENERATION_LIMIT, Title},
        transcript::{
            EffortChanged, FastModeChanged, LocalKind, SCHEMA_VERSION, SessionStarted,
            SpeedChanged, TerminalStopReason, TranscriptRecord, TurnId, WorkerTurnAccepted,
        },
    },
};
pub(crate) use conversation_search::{ConversationMatch, search_terms};
use durable::EventPosition;
#[cfg(test)]
pub(crate) use durable::SqliteStateReader;
pub(crate) use durable::{HostBatch, SqliteStateStore, transcript_commit};
use nanocode::agent::events::AgentEventKind;
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, ffi::ErrorCode, params,
    params_from_iter, types::Value,
};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use thiserror::Error;

const FORMAT_VERSION: i32 = 2;
/// How long a connection waits for another connection's write lock before reporting contention.
/// Tests use a shorter wait so that contention paths complete quickly.
pub(super) const BUSY_TIMEOUT: Duration = if cfg!(test) {
    Duration::from_millis(500)
} else {
    Duration::from_secs(5)
};
const STATE_COMPRESSION_LEVEL: i32 = 3;
/// Bounds the walk up a fork lineage, so a corrupt parent cycle cannot loop.
const MAX_LINEAGE_DEPTH: u32 = 10_000;

#[derive(Debug, Error)]
pub(crate) enum StorageError {
    #[error("failed to create private session directory {path}: {source}")]
    CreateDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to open session database {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
    #[error("failed to configure session database {path}: {source}")]
    Configure {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
    #[error("session database {path} uses format version {found}; expected {FORMAT_VERSION}")]
    UnsupportedVersion { path: PathBuf, found: i32 },
    #[error("failed to access session database {path}: {source}")]
    Query {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
    #[error("failed to encode session data: {0}")]
    Encode(#[from] serde_json::Error),
    #[error("the session event sequence exceeds the largest storable sequence")]
    SequenceOverflow,
    #[error("a host batch for session {state_id} is already staged for its writer's next commit")]
    HostBatchPending { state_id: String },
    #[error("failed to start the session store thread: {0}")]
    Thread(#[source] std::io::Error),
    #[error("failed to compress session data: {0}")]
    Compress(#[source] std::io::Error),
    #[error("failed to decode session data: {0}")]
    Decode(#[source] std::io::Error),
    #[error("stored transcript record uses schema version {found}; expected {SCHEMA_VERSION}")]
    UnsupportedRecordVersion { found: u32 },
}

impl StorageError {
    pub(crate) fn is_lock_contention(&self) -> bool {
        let source = match self {
            Self::Open { source, .. }
            | Self::Configure { source, .. }
            | Self::Query { source, .. } => source,
            _ => return false,
        };
        matches!(
            source.sqlite_error_code(),
            Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
        )
    }
}

#[derive(Debug)]
pub(crate) struct StoredSession {
    pub(crate) session_id: String,
    pub(crate) parent_session_id: Option<String>,
    pub(crate) started_at_unix_ms: u64,
    pub(crate) updated_at_unix_ms: u64,
    pub(crate) model: String,
    pub(crate) effort: ReasoningEffort,
    pub(crate) reasoning_mode: ReasoningMode,
    pub(crate) workspace: PathBuf,
    pub(crate) preview: String,
    /// The preview is a fallback title while a generated one is expected.
    pub(crate) title_pending: bool,
}

pub(crate) struct SessionSearchPage {
    pub(crate) sessions: Vec<StoredSession>,
    pub(crate) next_cursor: Option<(u64, String)>,
}

#[derive(Debug)]
pub(crate) struct StoredPrompt {
    pub(crate) text: String,
    pub(crate) recorded_at_unix_ms: u64,
    pub(crate) session_id: String,
    pub(crate) workspace: PathBuf,
}

#[derive(Debug)]
pub(crate) struct StoredRecord {
    pub(crate) event_id: i64,
    encoded: Vec<u8>,
}

#[derive(Debug)]
pub(crate) struct DecodedStoredRecord {
    pub(crate) event_id: i64,
    pub(crate) encoded_bytes: usize,
    pub(crate) record: TranscriptRecord,
}

impl StoredRecord {
    pub(crate) fn decode(self) -> Result<DecodedStoredRecord, StorageError> {
        let encoded_bytes = self.encoded.len();
        Ok(DecodedStoredRecord {
            event_id: self.event_id,
            encoded_bytes,
            record: decode_record(&self.encoded)?,
        })
    }
}

#[derive(Debug)]
pub(crate) struct StoredRecordPage {
    pub(crate) records: Vec<StoredRecord>,
    pub(crate) next_cursor: Option<i64>,
}

pub(crate) struct SessionStorage {
    path: PathBuf,
    connection: Connection,
    active_turns: HashMap<String, TurnId>,
}

pub(crate) struct RecordPrefix {
    pub(crate) records: Vec<Arc<TranscriptRecord>>,
    pub(crate) boundary_found: bool,
    pub(crate) session_found: bool,
}

impl SessionStorage {
    pub(crate) fn open(config_path: &Path) -> Result<Self, StorageError> {
        let path = database_path(config_path);
        let directory = path.parent().unwrap_or_else(|| Path::new("."));
        create_private_directory(directory)?;
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let connection =
            Connection::open_with_flags(&path, flags).map_err(|source| StorageError::Open {
                path: path.clone(),
                source,
            })?;
        configure(&connection, &path)?;
        initialize(&connection, &path)?;
        ensure_discovery_indexes(&connection, &path)?;
        conversation_search::ensure_schema(&connection, &path)?;
        ensure_titles(&connection, &path)?;
        durable::ensure_schema(&connection, &path)?;
        set_private_file_permissions(&path)?;
        Ok(Self {
            path,
            connection,
            active_turns: HashMap::new(),
        })
    }

    pub(crate) fn open_read_only(config_path: &Path) -> Result<Option<Self>, StorageError> {
        let path = database_path(config_path);
        if !path.exists() {
            return Ok(None);
        }
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let connection =
            Connection::open_with_flags(&path, flags).map_err(|source| StorageError::Open {
                path: path.clone(),
                source,
            })?;
        connection
            .busy_timeout(BUSY_TIMEOUT)
            .and_then(|_| connection.execute_batch("PRAGMA query_only = ON;"))
            .map_err(|source| StorageError::Configure {
                path: path.clone(),
                source,
            })?;
        let version = database_version(&connection, &path)?;
        if version == 0 {
            return Ok(None);
        }
        if version != FORMAT_VERSION {
            return Err(StorageError::UnsupportedVersion {
                path,
                found: version,
            });
        }
        Ok(Some(Self {
            path,
            connection,
            active_turns: HashMap::new(),
        }))
    }
    /// Writes records outside the durability contract, as binaries that predate it do.
    #[cfg(test)]
    pub(crate) fn append_records(
        &mut self,
        session_id: &str,
        records: &[Arc<TranscriptRecord>],
    ) -> Result<(), StorageError> {
        self.append_records_and_resume_state(session_id, records, None)
    }

    #[cfg(test)]
    pub(crate) fn append_records_and_resume_state(
        &mut self,
        session_id: &str,
        records: &[Arc<TranscriptRecord>],
        resume_state: Option<&[u8]>,
    ) -> Result<(), StorageError> {
        if records.is_empty() && resume_state.is_none() {
            return Ok(());
        }
        let compressed_state = resume_state.map(compress_resume_state).transpose()?;
        let path = self.path.clone();
        let transaction = self
            .connection
            .transaction()
            .map_err(|source| query(&path, source))?;
        let mut active_turn = self.active_turns.get(session_id).copied();
        let mut encoded = Vec::new();
        for record in records {
            encoded.clear();
            serde_json::to_writer(&mut encoded, record.as_ref())?;
            write_record(
                &transaction,
                &path,
                session_id,
                &mut active_turn,
                record,
                &encoded,
                None,
            )?;
        }
        if let Some(compressed) = compressed_state {
            write_resume_state(&transaction, &path, session_id, &compressed)?;
        }
        transaction
            .commit()
            .map_err(|source| query(&path, source))?;
        if let Some(active_turn) = active_turn {
            self.active_turns.insert(session_id.to_owned(), active_turn);
        }
        Ok(())
    }

    pub(crate) fn load_records(
        &self,
        session_id: &str,
    ) -> Result<Vec<Arc<TranscriptRecord>>, StorageError> {
        Ok(self.load_record_prefix(session_id, None)?.records)
    }

    pub(crate) fn load_records_through(
        &self,
        session_id: &str,
        through_sequence: u64,
    ) -> Result<RecordPrefix, StorageError> {
        if through_sequence == 0 {
            return Ok(RecordPrefix {
                records: Vec::new(),
                boundary_found: true,
                session_found: false,
            });
        }
        self.load_record_prefix(session_id, Some(through_sequence))
    }

    fn load_record_prefix(
        &self,
        session_id: &str,
        through_sequence: Option<u64>,
    ) -> Result<RecordPrefix, StorageError> {
        let mut statement = self
            .connection
            .prepare("SELECT record_json FROM events WHERE session_id = ?1 ORDER BY event_id")
            .map_err(|source| query(&self.path, source))?;
        let mut rows = statement
            .query([session_id])
            .map_err(|source| query(&self.path, source))?;
        let mut records = Vec::new();
        let mut session_found = false;
        while let Some(row) = rows.next().map_err(|source| query(&self.path, source))? {
            session_found = true;
            let encoded = row
                .get_ref(0)
                .map_err(|source| query(&self.path, source))?
                .as_blob()
                .map_err(|source| {
                    query(
                        &self.path,
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Blob,
                            Box::new(source),
                        ),
                    )
                })?;
            let record = Arc::new(decode_record(encoded)?);
            if through_sequence.is_some_and(|end| record.sequence() > end) {
                break;
            }
            let boundary_found = through_sequence == Some(record.sequence());
            records.push(record);
            if boundary_found {
                return Ok(RecordPrefix {
                    records,
                    boundary_found: true,
                    session_found,
                });
            }
        }
        Ok(RecordPrefix {
            records,
            boundary_found: through_sequence.is_none(),
            session_found,
        })
    }

    pub(crate) fn load_record_page(
        &self,
        session_id: &str,
        after_event_id: Option<i64>,
        limit: usize,
        max_bytes: usize,
    ) -> Result<Option<StoredRecordPage>, StorageError> {
        let exists = self
            .connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sessions WHERE session_id = ?1)",
                [session_id],
                |row| row.get::<_, bool>(0),
            )
            .map_err(|source| query(&self.path, source))?;
        if !exists {
            return Ok(None);
        }

        let after_event_id = after_event_id.unwrap_or(0);
        let row_limit = i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX);
        let mut statement = self
            .connection
            .prepare(
                "SELECT event_id, length(record_json), record_json FROM events\n\
                 WHERE session_id = ?1 AND event_id > ?2\n\
                 ORDER BY event_id LIMIT ?3",
            )
            .map_err(|source| query(&self.path, source))?;
        let mut rows = statement
            .query(params![session_id, after_event_id, row_limit])
            .map_err(|source| query(&self.path, source))?;
        let mut records = Vec::new();
        let mut loaded_bytes = 0usize;
        let mut has_more = false;
        while let Some(row) = rows.next().map_err(|source| query(&self.path, source))? {
            if records.len() == limit {
                has_more = true;
                break;
            }
            let encoded_bytes = row
                .get::<_, i64>(1)
                .map_err(|source| query(&self.path, source))?;
            let encoded_bytes = usize::try_from(encoded_bytes).unwrap_or(usize::MAX);
            // The first record is always admitted so a caller can advance past a single oversized
            // event. Every additional record must fit within the requested page byte budget.
            if !records.is_empty() && loaded_bytes.saturating_add(encoded_bytes) > max_bytes {
                has_more = true;
                break;
            }
            let encoded = row
                .get::<_, Vec<u8>>(2)
                .map_err(|source| query(&self.path, source))?;
            loaded_bytes = loaded_bytes.saturating_add(encoded.len());
            records.push(StoredRecord {
                event_id: row.get(0).map_err(|source| query(&self.path, source))?,
                encoded,
            });
        }
        let next_cursor = has_more
            .then(|| records.last().map(|record| record.event_id))
            .flatten();
        Ok(Some(StoredRecordPage {
            records,
            next_cursor,
        }))
    }

    #[cfg(test)]
    pub(crate) fn save_resume_state(
        &mut self,
        session_id: &str,
        encoded: &[u8],
    ) -> Result<(), StorageError> {
        let compressed = compress_resume_state(encoded)?;
        write_resume_state(&self.connection, &self.path, session_id, &compressed)
    }

    #[cfg(test)]
    pub(crate) fn append_raw_record(
        &self,
        session_id: &str,
        encoded: &[u8],
    ) -> Result<(), StorageError> {
        self.connection
            .execute(
                "INSERT INTO events(session_id, record_json) VALUES (?1, ?2)",
                params![session_id, encoded],
            )
            .map_err(|source| query(&self.path, source))?;
        Ok(())
    }

    pub(crate) fn terminal_stop(
        &self,
        session_id: &str,
    ) -> Result<Option<TerminalStopReason>, StorageError> {
        let reason = self.connection.query_row(
            "SELECT json_quote(json_extract(CAST(record_json AS TEXT), '$.payload.terminal_stop'))
             FROM events WHERE session_id = ?1
             AND json_extract(CAST(record_json AS TEXT), '$.source') = 'tact'
             AND json_extract(CAST(record_json AS TEXT), '$.type') IN ('worker.turn_finished', 'compaction.finished')
             AND json_type(CAST(record_json AS TEXT), '$.payload.terminal_stop') != 'null'
             ORDER BY event_id LIMIT 1",
            [session_id],
            |row| row.get::<_, String>(0),
        ).optional().map_err(|source| query(&self.path, source))?;
        reason
            .map(|reason| serde_json::from_str(&reason).map_err(Into::into))
            .transpose()
    }

    pub(crate) fn load_resume_state(
        &self,
        session_id: &str,
    ) -> Result<Option<Vec<u8>>, StorageError> {
        let compressed = self
            .connection
            .query_row(
                "SELECT state_zstd FROM resume_states WHERE session_id = ?1",
                [session_id],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(|source| query(&self.path, source))?;
        compressed
            .map(|compressed| zstd::decode_all(compressed.as_slice()).map_err(StorageError::Decode))
            .transpose()
    }

    /// A generated title is pending while its claim is younger than [`GENERATION_LIMIT`]; an older
    /// claim belongs to a process that exited mid-generation, so its fallback is final.
    pub(crate) fn titles(&self) -> Result<HashMap<String, Title>, StorageError> {
        let exists: bool = self
            .connection
            .query_row(
                "SELECT COUNT(*) = 2 FROM sqlite_schema
                 WHERE name IN ('session_titles', 'title_claims')",
                [],
                |row| row.get(0),
            )
            .map_err(|source| query(&self.path, source))?;
        if !exists {
            return Ok(HashMap::new());
        }
        let fresh_after = unix_ms().saturating_sub(GENERATION_LIMIT.as_millis() as i64);
        let mut statement = self
            .connection
            .prepare(
                "SELECT t.session_id, t.title,
                     t.source = 'generated' AND c.claimed_at_ms IS NOT NULL AND c.claimed_at_ms > ?1
                 FROM session_titles t LEFT JOIN title_claims c ON c.session_id = t.session_id",
            )
            .map_err(|source| query(&self.path, source))?;
        statement
            .query_map([fresh_after], |row| {
                Ok((
                    row.get(0)?,
                    Title {
                        text: row.get(1)?,
                        pending: row.get(2)?,
                    },
                ))
            })
            .map_err(|source| query(&self.path, source))?
            .map(|row| row.map_err(|source| query(&self.path, source)))
            .collect()
    }

    pub(crate) fn rename_session(&self, session_id: &str, title: &str) -> Result<(), StorageError> {
        self.connection
            .execute(
                "INSERT INTO session_titles(session_id, title, source) VALUES (?1, ?2, 'manual')
             ON CONFLICT(session_id) DO UPDATE SET title = excluded.title, source = 'manual'",
                params![session_id, title],
            )
            .map_err(|source| query(&self.path, source))?;
        Ok(())
    }

    /// Claiming persists the fallback before a model call, so a crash or another process cannot
    /// cause a session to be titled repeatedly. Manual titles own the row permanently. A claim
    /// made while `generating` keeps the title pending until it is completed or settled.
    pub(crate) fn claim_title(
        &self,
        session_id: &str,
        fallback: &str,
        generating: bool,
    ) -> Result<bool, StorageError> {
        let transaction = self
            .connection
            .unchecked_transaction()
            .map_err(|source| query(&self.path, source))?;
        let claimed = transaction
            .execute(
                "INSERT INTO session_titles(session_id, title, source) VALUES (?1, ?2, 'generated')
             ON CONFLICT(session_id) DO NOTHING",
                params![session_id, fallback],
            )
            .map_err(|source| query(&self.path, source))?
            == 1;
        if claimed && generating {
            transaction
                .execute(
                    "INSERT OR REPLACE INTO title_claims(session_id, claimed_at_ms) VALUES (?1, ?2)",
                    params![session_id, unix_ms()],
                )
                .map_err(|source| query(&self.path, source))?;
        }
        transaction
            .commit()
            .map_err(|source| query(&self.path, source))?;
        Ok(claimed)
    }

    pub(crate) fn complete_title(&self, session_id: &str, title: &str) -> Result<(), StorageError> {
        self.connection.execute(
            "UPDATE session_titles SET title = ?2 WHERE session_id = ?1 AND source = 'generated'",
            params![session_id, title],
        ).map_err(|source| query(&self.path, source))?;
        self.settle_title(session_id)
    }

    /// Ends a pending claim without a generated title, leaving the fallback as the final title.
    pub(crate) fn settle_title(&self, session_id: &str) -> Result<(), StorageError> {
        self.connection
            .execute(
                "DELETE FROM title_claims WHERE session_id = ?1",
                [session_id],
            )
            .map_err(|source| query(&self.path, source))?;
        Ok(())
    }

    /// A bounded batch, newest first, excludes forks and sessions without a user prompt.
    pub(crate) fn untitled_prompts(
        &self,
        limit: usize,
    ) -> Result<Vec<(String, String)>, StorageError> {
        let mut statement = self.connection.prepare(
            "SELECT s.session_id, (SELECT e.prompt_text FROM events e
                 WHERE e.session_id = s.session_id AND e.prompt_text IS NOT NULL
                 ORDER BY e.event_id LIMIT 1)
             FROM sessions s WHERE s.parent_session_id IS NULL
                 AND EXISTS(SELECT 1 FROM events e WHERE e.session_id = s.session_id AND e.prompt_text IS NOT NULL)
                 AND NOT EXISTS(SELECT 1 FROM session_titles t WHERE t.session_id = s.session_id)
             ORDER BY s.updated_at_ms DESC LIMIT ?1",
        ).map_err(|source| query(&self.path, source))?;
        statement
            .query_map([limit as i64], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(|source| query(&self.path, source))?
            .map(|row| row.map_err(|source| query(&self.path, source)))
            .collect()
    }

    pub(crate) fn inherit_titles(&self) -> Result<(), StorageError> {
        let mut statement = self
            .connection
            .prepare(
                "WITH RECURSIVE lineage(session_id, depth) AS (
                 SELECT session_id, 0 FROM sessions WHERE parent_session_id IS NULL
                 UNION ALL
                 SELECT s.session_id, l.depth + 1 FROM sessions s
                 JOIN lineage l ON s.parent_session_id = l.session_id
             )
             SELECT s.session_id, s.parent_session_id FROM sessions s
             JOIN lineage l ON l.session_id = s.session_id
             WHERE s.parent_session_id IS NOT NULL
                 AND NOT EXISTS(SELECT 1 FROM session_titles t WHERE t.session_id = s.session_id)
             ORDER BY l.depth",
            )
            .map_err(|source| query(&self.path, source))?;
        let forks = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|source| query(&self.path, source))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|source| query(&self.path, source))?;
        for (session, parent) in forks {
            inherit_title(&self.connection, &self.path, &session, &parent)?;
        }
        Ok(())
    }

    /// The root of the fork lineage that contains `session_id`, and the session's depth below it.
    /// A session without a stored row is its own root.
    pub(crate) fn lineage_root(&self, session_id: &str) -> Result<(String, u32), StorageError> {
        let root = self
            .connection
            .query_row(
                "WITH RECURSIVE chain(session_id, parent_session_id, depth) AS (
                     SELECT session_id, parent_session_id, 0 FROM sessions WHERE session_id = ?1
                     UNION ALL
                     SELECT s.session_id, s.parent_session_id, c.depth + 1 FROM sessions s
                     JOIN chain c ON s.session_id = c.parent_session_id
                     WHERE c.depth < ?2
                 )
                 SELECT session_id, depth FROM chain ORDER BY depth DESC LIMIT 1",
                params![session_id, MAX_LINEAGE_DEPTH],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?)),
            )
            .optional()
            .map_err(|source| query(&self.path, source))?;
        Ok(root.unwrap_or_else(|| (session_id.to_owned(), 0)))
    }

    pub(crate) fn list_sessions(
        &self,
        workspace: &Path,
        resumable_only: bool,
    ) -> Result<Vec<StoredSession>, StorageError> {
        let workspace = workspace.to_string_lossy();
        let statement_sql = if resumable_only {
            "SELECT s.session_id, s.started_at_ms, s.model, s.effort, s.reasoning_mode,\n\
                    s.workspace, s.preview, s.updated_at_ms, s.parent_session_id\n\
             FROM sessions s INNER JOIN resume_states r ON r.session_id = s.session_id\n\
             WHERE s.workspace = ?1\n\
             ORDER BY s.updated_at_ms DESC, s.session_id"
        } else {
            "SELECT s.session_id, s.started_at_ms, s.model, s.effort, s.reasoning_mode,\n\
                    s.workspace, s.preview, s.updated_at_ms, s.parent_session_id\n\
             FROM sessions s\n\
             WHERE s.workspace = ?1\n\
             ORDER BY s.updated_at_ms DESC, s.session_id"
        };
        let mut statement = self
            .connection
            .prepare(statement_sql)
            .map_err(|source| query(&self.path, source))?;
        let rows = statement
            .query_map([workspace.as_ref()], decode_session)
            .map_err(|source| query(&self.path, source))?;
        let titles = self.titles()?;
        rows.map(|row| {
            let mut session = row.map_err(|source| query(&self.path, source))?;
            if let Some(title) = titles.get(&session.session_id) {
                session.preview.clone_from(&title.text);
                session.title_pending = title.pending;
            }
            Ok(session)
        })
        .collect()
    }

    pub(crate) fn find_sessions(
        &self,
        current_session: &str,
        workspace: Option<&Path>,
        only_sessions: Option<&[String]>,
        contains_any: Option<&[String]>,
        cursor: Option<(u64, &str)>,
        limit: usize,
    ) -> Result<SessionSearchPage, StorageError> {
        if only_sessions.is_some_and(<[String]>::is_empty) {
            return Ok(SessionSearchPage {
                sessions: Vec::new(),
                next_cursor: None,
            });
        }
        let mut parameters = vec![Value::Text(current_session.to_owned())];
        let mut candidate_filters = String::new();
        if let Some(workspace) = workspace {
            parameters.push(Value::Text(workspace.to_string_lossy().into_owned()));
            candidate_filters.push_str(&format!("AND s.workspace = ?{}\n", parameters.len()));
        }
        if let Some(session_ids) = only_sessions {
            let placeholders = session_ids
                .iter()
                .map(|session_id| {
                    parameters.push(Value::Text(session_id.clone()));
                    format!("?{}", parameters.len())
                })
                .collect::<Vec<_>>()
                .join(", ");
            candidate_filters.push_str(&format!("AND s.session_id IN ({placeholders})\n"));
        }
        if let Some((updated_at, session_id)) = cursor {
            parameters.push(Value::Integer(to_sql_u64(updated_at)));
            let updated_at_parameter = parameters.len();
            parameters.push(Value::Text(session_id.to_owned()));
            let session_id_parameter = parameters.len();
            candidate_filters.push_str(&format!(
                "AND (s.updated_at_ms < ?{updated_at_parameter}\n\
                 OR (s.updated_at_ms = ?{updated_at_parameter}\n\
                     AND s.session_id > ?{session_id_parameter}))\n"
            ));
        }
        parameters.push(Value::Integer(
            i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX),
        ));
        let limit_parameter = parameters.len();
        let matches = contains_any.map_or_else(
            || "1".to_owned(),
            |patterns| {
                let search_parameters = patterns
                    .iter()
                    .enumerate()
                    .map(|(index, pattern)| {
                        parameters.push(Value::Text(pattern.to_owned()));
                        index.saturating_add(limit_parameter).saturating_add(1)
                    })
                    .collect::<Vec<_>>();
                let preview = search_parameters
                    .iter()
                    .map(|parameter| {
                        format!("instr(lower(c.search_preview), lower(?{parameter})) > 0")
                    })
                    .collect::<Vec<_>>()
                    .join(" OR ");
                let prompts = search_parameters
                    .iter()
                    .map(|parameter| {
                        format!("instr(lower(e.prompt_text), lower(?{parameter})) > 0")
                    })
                    .collect::<Vec<_>>()
                    .join(" OR ");
                format!(
                    "(({preview}) OR EXISTS(\n\
                     SELECT 1 FROM events e WHERE e.session_id = c.session_id\n\
                     AND e.prompt_text IS NOT NULL AND ({prompts})\n\
                 ))"
                )
            },
        );
        let query_sql = format!(
            "WITH candidates AS MATERIALIZED (\n\
                 SELECT s.session_id, s.started_at_ms, s.model, s.effort, s.reasoning_mode,\n\
                        s.workspace, s.preview AS search_preview, s.updated_at_ms,\n\
                        s.parent_session_id\n\
                 FROM sessions s\n\
                 WHERE s.session_id != ?1\n\
                   {candidate_filters}\
                 ORDER BY s.updated_at_ms DESC, s.session_id\n\
                 LIMIT ?{limit_parameter}\n\
             )\n\
             SELECT c.session_id, c.started_at_ms, c.model, c.effort, c.reasoning_mode,\n\
                    c.workspace, substr(c.search_preview, 1, 513), c.updated_at_ms,\n\
                    c.parent_session_id, {matches}\n\
             FROM candidates c\n\
             ORDER BY c.updated_at_ms DESC, c.session_id"
        );
        let mut statement = self
            .connection
            .prepare(&query_sql)
            .map_err(|source| query(&self.path, source))?;
        let rows = statement
            .query_map(params_from_iter(parameters), |row| {
                Ok((decode_session(row)?, row.get::<_, bool>(9)?))
            })
            .map_err(|source| query(&self.path, source))?;
        let mut candidates = rows
            .map(|row| row.map_err(|source| query(&self.path, source)))
            .collect::<Result<Vec<_>, _>>()?;
        let has_more = candidates.len() > limit;
        candidates.truncate(limit);
        let next_cursor = if has_more {
            candidates
                .last()
                .map(|session| (session.0.updated_at_unix_ms, session.0.session_id.clone()))
        } else {
            None
        };
        Ok(SessionSearchPage {
            sessions: candidates
                .into_iter()
                .filter_map(|(session, matches)| matches.then_some(session))
                .collect(),
            next_cursor,
        })
    }

    pub(crate) fn recent_prompts(&self, limit: usize) -> Result<Vec<StoredPrompt>, StorageError> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut statement = self
            .connection
            .prepare(
                "SELECT e.prompt_text, e.prompt_recorded_at_ms, e.session_id, s.workspace\n\
                 FROM events e INNER JOIN sessions s ON s.session_id = e.session_id\n\
                 WHERE e.prompt_text IS NOT NULL\n\
                 ORDER BY e.prompt_recorded_at_ms DESC, e.event_id DESC LIMIT ?1",
            )
            .map_err(|source| query(&self.path, source))?;
        let rows = statement
            .query_map([limit], |row| {
                Ok(StoredPrompt {
                    text: row.get(0)?,
                    recorded_at_unix_ms: from_sql_u64(row.get(1)?),
                    session_id: row.get(2)?,
                    workspace: PathBuf::from(row.get::<_, String>(3)?),
                })
            })
            .map_err(|source| query(&self.path, source))?;
        rows.map(|row| row.map_err(|source| query(&self.path, source)))
            .collect()
    }
}

fn compress_resume_state(state: &[u8]) -> Result<Vec<u8>, StorageError> {
    zstd::encode_all(state, STATE_COMPRESSION_LEVEL).map_err(StorageError::Compress)
}

fn write_resume_state(
    connection: &Connection,
    path: &Path,
    session_id: &str,
    compressed: &[u8],
) -> Result<(), StorageError> {
    connection
        .execute(
            "INSERT INTO resume_states(session_id, state_zstd) VALUES (?1, ?2)\n\
             ON CONFLICT(session_id) DO UPDATE SET state_zstd = excluded.state_zstd",
            params![session_id, compressed],
        )
        .map_err(|source| query(path, source))?;
    Ok(())
}

fn decode_record(encoded: &[u8]) -> Result<TranscriptRecord, StorageError> {
    let record = serde_json::from_slice::<TranscriptRecord>(encoded)?;
    if record.schema_version() != SCHEMA_VERSION {
        return Err(StorageError::UnsupportedRecordVersion {
            found: record.schema_version(),
        });
    }
    Ok(record)
}

pub(crate) fn database_path(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("sessions/v2.sqlite3")
}

fn configure(connection: &Connection, path: &Path) -> Result<(), StorageError> {
    connection
        .busy_timeout(BUSY_TIMEOUT)
        .map_err(|source| StorageError::Configure {
            path: path.to_path_buf(),
            source,
        })?;
    connection
        .execute_batch(
            "PRAGMA foreign_keys = ON;\n\
             PRAGMA journal_mode = WAL;\n\
             PRAGMA synchronous = FULL;\n\
             PRAGMA wal_autocheckpoint = 1000;",
        )
        .map_err(|source| StorageError::Configure {
            path: path.to_path_buf(),
            source,
        })
}

fn initialize(connection: &Connection, path: &Path) -> Result<(), StorageError> {
    let version = database_version(connection, path)?;
    if version == FORMAT_VERSION {
        return Ok(());
    }
    if version != 0 {
        return Err(StorageError::UnsupportedVersion {
            path: path.to_path_buf(),
            found: version,
        });
    }
    connection
        .execute_batch(
            "BEGIN IMMEDIATE;\n\
             CREATE TABLE IF NOT EXISTS sessions(\n\
                 session_id TEXT PRIMARY KEY, parent_session_id TEXT, workspace TEXT NOT NULL,\n\
                 model TEXT NOT NULL, effort TEXT NOT NULL, reasoning_mode TEXT NOT NULL,\n\
                 fast_mode INTEGER NOT NULL CHECK(fast_mode IN (0, 1)),\n\
                 application_version TEXT NOT NULL, started_at_ms INTEGER NOT NULL,\n\
                 updated_at_ms INTEGER NOT NULL, preview TEXT NOT NULL\n\
             ) STRICT;\n\
             CREATE TABLE IF NOT EXISTS events(\n\
                 event_id INTEGER PRIMARY KEY AUTOINCREMENT,\n\
                 session_id TEXT NOT NULL REFERENCES sessions(session_id),\n\
                 record_json BLOB NOT NULL, prompt_text TEXT, prompt_recorded_at_ms INTEGER,\n\
                 assistant_stream TEXT,\n\
                 CHECK((prompt_text IS NULL) = (prompt_recorded_at_ms IS NULL))\n\
             ) STRICT;\n\
             CREATE INDEX IF NOT EXISTS events_by_session ON events(session_id, event_id);\n\
             CREATE INDEX IF NOT EXISTS recent_prompts\n\
                 ON events(prompt_recorded_at_ms DESC, event_id DESC)\n\
                 WHERE prompt_text IS NOT NULL;\n\
             CREATE INDEX IF NOT EXISTS assistant_streams\n\
                 ON events(session_id, assistant_stream) WHERE assistant_stream IS NOT NULL;\n\
             CREATE TABLE IF NOT EXISTS resume_states(\n\
                 session_id TEXT PRIMARY KEY, state_zstd BLOB NOT NULL\n\
             ) STRICT;\n\
             PRAGMA user_version = 2; COMMIT;",
        )
        .map_err(|source| query(path, source))
}

fn ensure_discovery_indexes(connection: &Connection, path: &Path) -> Result<(), StorageError> {
    connection
        .execute_batch(
            "CREATE INDEX IF NOT EXISTS sessions_by_recency\n\
                 ON sessions(updated_at_ms DESC, session_id);\n\
             CREATE INDEX IF NOT EXISTS sessions_by_workspace_recency\n\
                 ON sessions(workspace, updated_at_ms DESC, session_id);",
        )
        .map_err(|source| query(path, source))
}

fn ensure_titles(connection: &Connection, path: &Path) -> Result<(), StorageError> {
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS session_titles(
             session_id TEXT PRIMARY KEY, title TEXT NOT NULL,
             source TEXT NOT NULL CHECK(source IN ('generated', 'manual'))
         ) STRICT;
         CREATE TABLE IF NOT EXISTS title_claims(
             session_id TEXT PRIMARY KEY, claimed_at_ms INTEGER NOT NULL
         ) STRICT;",
        )
        .map_err(|source| query(path, source))
}

fn unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as i64)
}

fn database_version(connection: &Connection, path: &Path) -> Result<i32, StorageError> {
    connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|source| query(path, source))
}

/// Appends one record and returns its event id, tracking the turn whose assistant stream the
/// following records belong to.
fn write_record(
    transaction: &Transaction<'_>,
    path: &Path,
    session_id: &str,
    active_turn: &mut Option<TurnId>,
    record: &TranscriptRecord,
    encoded: &[u8],
    position: Option<EventPosition>,
) -> Result<i64, StorageError> {
    if record.local_kind() == Some(LocalKind::WorkerTurnAccepted) {
        *active_turn = Some(record.decode_payload::<WorkerTurnAccepted>()?.id);
    }
    append_record(
        transaction,
        path,
        session_id,
        *active_turn,
        record,
        encoded,
        position,
    )
}

fn append_record(
    transaction: &Transaction<'_>,
    path: &Path,
    session_id: &str,
    active_turn: Option<TurnId>,
    record: &TranscriptRecord,
    encoded: &[u8],
    position: Option<EventPosition>,
) -> Result<i64, StorageError> {
    if record.local_kind() == Some(LocalKind::SessionStarted) {
        let started = record.decode_payload::<SessionStarted>()?;
        upsert_session(transaction, path, record, &started)?;
    }
    let prompt_text = record.prompt_text()?;
    let assistant_stream = assistant_stream(active_turn, record)?;
    if record.agent_kind() == Some(AgentEventKind::AssistantMessage)
        && let Some(stream) = &assistant_stream
    {
        // Sequenced rows are immutable contract events; a final message replaces only drafts.
        transaction
            .execute(
                "DELETE FROM events WHERE session_id = ?1 AND assistant_stream = ?2 AND seq IS NULL",
                params![session_id, stream],
            )
            .map_err(|source| query(path, source))?;
    }
    transaction
        .execute(
            "INSERT INTO events(\n\
                 session_id, record_json, prompt_text, prompt_recorded_at_ms, assistant_stream,\n\
                 seq, revision\n\
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                session_id,
                encoded,
                prompt_text,
                prompt_text
                    .as_ref()
                    .map(|_| to_sql_u64(record.recorded_at_unix_ms())),
                (record.agent_kind() == Some(AgentEventKind::AssistantDelta))
                    .then_some(assistant_stream)
                    .flatten(),
                position.map(|position| position.seq),
                position.map(|position| position.revision)
            ],
        )
        .map_err(|source| query(path, source))?;
    let event_id = transaction.last_insert_rowid();
    transaction
        .execute(
            "UPDATE sessions SET updated_at_ms = ?2 WHERE session_id = ?1",
            params![session_id, to_sql_u64(record.recorded_at_unix_ms())],
        )
        .map_err(|source| query(path, source))?;
    if let Some(prompt) = prompt_text {
        let preview = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
        if !preview.is_empty() {
            transaction
                .execute(
                    "UPDATE sessions SET preview = ?2\n\
                     WHERE session_id = ?1 AND preview = 'No user prompt'",
                    params![session_id, preview],
                )
                .map_err(|source| query(path, source))?;
        }
    }
    update_settings(transaction, path, session_id, record)?;
    Ok(event_id)
}

fn upsert_session(
    transaction: &Transaction<'_>,
    path: &Path,
    record: &TranscriptRecord,
    started: &SessionStarted,
) -> Result<(), StorageError> {
    let effort = serde_json::to_string(&started.effort)?;
    let reasoning_mode = serde_json::to_string(&started.reasoning_mode)?;
    transaction
        .execute(
            "INSERT INTO sessions(\n\
                 session_id, parent_session_id, workspace, model, effort, reasoning_mode,\n\
                 fast_mode, application_version, started_at_ms, updated_at_ms, preview\n\
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9, 'No user prompt')\n\
             ON CONFLICT(session_id) DO UPDATE SET\n\
                 workspace=excluded.workspace, model=excluded.model, effort=excluded.effort,\n\
                 reasoning_mode=excluded.reasoning_mode, fast_mode=excluded.fast_mode,\n\
                 application_version=excluded.application_version,\n\
                 started_at_ms=excluded.started_at_ms, updated_at_ms=excluded.updated_at_ms",
            params![
                started.session_id,
                started.parent_session_id,
                started.workspace.to_string_lossy(),
                started.model,
                effort,
                reasoning_mode,
                started.speed != Speed::Standard,
                started.application_version,
                to_sql_u64(record.recorded_at_unix_ms())
            ],
        )
        .map_err(|source| query(path, source))?;
    if let Some(parent) = &started.parent_session_id {
        inherit_title(transaction, path, &started.session_id, parent)?;
    }
    Ok(())
}

fn inherit_title(
    connection: &Connection,
    path: &Path,
    session: &str,
    parent: &str,
) -> Result<(), StorageError> {
    let inherited = connection
        .query_row(
            "SELECT t.title, (SELECT e.prompt_text FROM events e
             WHERE e.session_id = s.session_id AND e.prompt_text IS NOT NULL
             ORDER BY e.event_id LIMIT 1)
         FROM sessions s LEFT JOIN session_titles t ON t.session_id = s.session_id
         WHERE s.session_id = ?1",
            [parent],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                ))
            },
        )
        .optional()
        .map_err(|source| query(path, source))?;
    if let Some((title, prompt)) = inherited {
        let title = title.unwrap_or_else(|| {
            crate::core::titles::fallback(prompt.as_deref().unwrap_or_default())
        });
        connection
            .execute(
                "INSERT INTO session_titles(session_id, title, source) VALUES (?1, ?2, 'generated')
             ON CONFLICT(session_id) DO NOTHING",
                params![session, title],
            )
            .map_err(|source| query(path, source))?;
    }
    Ok(())
}

fn assistant_stream(
    active_turn: Option<TurnId>,
    record: &TranscriptRecord,
) -> Result<Option<String>, serde_json::Error> {
    #[derive(serde::Deserialize, serde::Serialize)]
    struct AssistantRecord {
        model_call_index: u32,
        phase: Option<serde_json::Value>,
    }
    if !matches!(
        record.agent_kind(),
        Some(AgentEventKind::AssistantDelta | AgentEventKind::AssistantMessage)
    ) {
        return Ok(None);
    }
    let assistant = record.decode_payload::<AssistantRecord>()?;
    serde_json::to_string(&(active_turn, assistant.model_call_index, assistant.phase)).map(Some)
}

fn update_settings(
    transaction: &Transaction<'_>,
    path: &Path,
    session_id: &str,
    record: &TranscriptRecord,
) -> Result<(), StorageError> {
    let speed = match record.local_kind() {
        Some(LocalKind::EffortChanged) => {
            let effort = serde_json::to_string(&record.decode_payload::<EffortChanged>()?.to)?;
            transaction
                .execute(
                    "UPDATE sessions SET effort=?2 WHERE session_id=?1",
                    params![session_id, effort],
                )
                .map_err(|source| query(path, source))?;
            return Ok(());
        }
        Some(LocalKind::SpeedChanged) => record.decode_payload::<SpeedChanged>()?.to,
        Some(LocalKind::FastModeChanged) => record.decode_payload::<FastModeChanged>()?.speed(),
        _ => return Ok(()),
    };
    transaction
        .execute(
            "UPDATE sessions SET fast_mode=?2 WHERE session_id=?1",
            params![session_id, speed != Speed::Standard],
        )
        .map_err(|source| query(path, source))?;
    Ok(())
}

fn decode_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredSession> {
    let effort = row.get::<_, String>(3)?;
    let reasoning_mode = row.get::<_, String>(4)?;
    Ok(StoredSession {
        session_id: row.get(0)?,
        parent_session_id: row.get(8)?,
        started_at_unix_ms: from_sql_u64(row.get(1)?),
        updated_at_unix_ms: from_sql_u64(row.get(7)?),
        model: row.get(2)?,
        effort: decode_json_column(3, &effort)?,
        reasoning_mode: decode_json_column(4, &reasoning_mode)?,
        workspace: PathBuf::from(row.get::<_, String>(5)?),
        preview: row.get(6)?,
        title_pending: false,
    })
}

fn decode_json_column<T: serde::de::DeserializeOwned>(
    index: usize,
    value: &str,
) -> rusqlite::Result<T> {
    serde_json::from_str(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

fn query(path: &Path, source: rusqlite::Error) -> StorageError {
    StorageError::Query {
        path: path.to_path_buf(),
        source,
    }
}

fn to_sql_u64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}
fn from_sql_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or_default()
}

fn create_private_directory(path: &Path) -> Result<(), StorageError> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(path)
        .map_err(|source| StorageError::CreateDirectory {
            path: path.to_path_buf(),
            source,
        })
}

fn set_private_file_permissions(path: &Path) -> Result<(), StorageError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|source| {
            StorageError::CreateDirectory {
                path: path.to_path_buf(),
                source,
            }
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{SessionStorage, StorageError, database_path};
    use crate::{
        app::config::{ReasoningEffort, ReasoningMode, Speed},
        core::transcript::{
            LocalEvent, LocalKind, SessionStarted, SpeedChanged, TranscriptRecord, TurnId,
            UserSubmitted,
        },
    };
    use rusqlite::Connection;
    use serde_json::{Value, json};
    use std::{fs, path::Path, sync::Arc};
    use tempfile::tempdir;

    #[test]
    fn speed_records_round_trip_with_boolean_metadata_projection() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let mut storage = SessionStorage::open(&config).unwrap();
        append_prompt(&mut storage, "session", 100, "prompt");

        for (index, from, to) in [
            (0, Speed::Standard, Speed::Ultrafast),
            (1, Speed::Ultrafast, Speed::Fast),
            (2, Speed::Fast, Speed::Standard),
        ] {
            let record = Arc::new(
                TranscriptRecord::from_local(
                    index + 3,
                    index + 101,
                    LocalEvent::SpeedChanged(SpeedChanged { from, to }),
                )
                .unwrap(),
            );
            storage.append_records("session", &[record]).unwrap();
            let enabled: bool = storage
                .connection
                .query_row(
                    "SELECT fast_mode FROM sessions WHERE session_id='session'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(enabled, to != Speed::Standard);
        }
        drop(storage);

        let storage = SessionStorage::open(&config).unwrap();
        let records = storage.load_records("session").unwrap();
        assert_eq!(records.len(), 5);
        assert_eq!(
            records[0].decode_payload::<SessionStarted>().unwrap().speed,
            Speed::Standard
        );
        for (record, to) in
            records[2..]
                .iter()
                .zip([Speed::Ultrafast, Speed::Fast, Speed::Standard])
        {
            assert_eq!(record.local_kind(), Some(LocalKind::SpeedChanged));
            assert_eq!(record.decode_payload::<Value>().unwrap()["to"], json!(to));
        }
    }

    #[test]
    fn historical_speed_records_remain_unchanged_when_loaded_and_extended() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let mut storage = SessionStorage::open(&config).unwrap();
        let legacy = [
            json!({
                "schema_version": 2,
                "sequence": 1,
                "recorded_at_unix_ms": 100,
                "source": "tact",
                "type": "session.started",
                "payload": {
                    "session_id": "legacy", "model": "gpt-6-astra", "effort": "medium",
                    "reasoning_mode": "standard", "fast_mode": true,
                    "workspace": "/work", "application_version": "test",
                },
            }),
            json!({
                "schema_version": 2,
                "sequence": 2,
                "recorded_at_unix_ms": 101,
                "source": "tact",
                "type": "fast_mode.changed",
                "payload": { "from": true, "to": false },
            }),
        ];
        let records = legacy
            .iter()
            .map(|encoded| {
                Arc::new(serde_json::from_value::<TranscriptRecord>(encoded.clone()).unwrap())
            })
            .collect::<Vec<_>>();
        let original = records
            .iter()
            .map(|record| serde_json::to_vec(record.as_ref()).unwrap())
            .collect::<Vec<_>>();
        storage.append_records("legacy", &records).unwrap();
        let enabled: bool = storage
            .connection
            .query_row(
                "SELECT fast_mode FROM sessions WHERE session_id='legacy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!enabled);
        drop(storage);

        let mut storage = SessionStorage::open(&config).unwrap();
        let loaded = storage.load_records("legacy").unwrap();
        assert_eq!(loaded.len(), legacy.len());
        assert_eq!(
            loaded[0].decode_payload::<SessionStarted>().unwrap().speed,
            Speed::Fast
        );
        for (record, encoded) in loaded.iter().zip(&legacy) {
            assert_eq!(&serde_json::to_value(record.as_ref()).unwrap(), encoded);
        }
        let changed = Arc::new(
            TranscriptRecord::from_local(
                3,
                102,
                LocalEvent::SpeedChanged(SpeedChanged {
                    from: Speed::Standard,
                    to: Speed::Ultrafast,
                }),
            )
            .unwrap(),
        );
        storage.append_records("legacy", &[changed]).unwrap();

        let enabled: bool = storage
            .connection
            .query_row(
                "SELECT fast_mode FROM sessions WHERE session_id='legacy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(enabled);

        let mut statement = storage.connection.prepare(
            "SELECT record_json FROM events WHERE session_id='legacy' ORDER BY event_id LIMIT 2",
        ).unwrap();
        let retained = statement
            .query_map([], |row| row.get::<_, Vec<u8>>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(retained, original);
    }

    #[test]
    fn rejects_an_unknown_database_version() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let path = database_path(&config);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let connection = Connection::open(&path).unwrap();
        connection.pragma_update(None, "user_version", 3).unwrap();
        drop(connection);

        let error = SessionStorage::open(&config).err().unwrap();
        assert!(matches!(
            error,
            StorageError::UnsupportedVersion { found: 3, .. }
        ));
    }

    #[test]
    fn opening_v2_storage_does_not_touch_v1_artifacts() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let artifacts = [
            "transcripts/sentinel",
            "checkpoints/sentinel",
            "transcript-projections/sentinel",
            "session-summaries/sentinel",
        ];
        for artifact in artifacts {
            let path = directory.path().join(artifact);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"leave untouched").unwrap();
        }

        drop(SessionStorage::open(&config).unwrap());

        for artifact in artifacts {
            assert_eq!(
                fs::read(directory.path().join(artifact)).unwrap(),
                b"leave untouched"
            );
        }
    }

    #[test]
    fn read_only_open_does_not_create_missing_v2_storage() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");

        assert!(SessionStorage::open_read_only(&config).unwrap().is_none());
        assert!(!database_path(&config).exists());
    }

    #[test]
    fn recent_prompts_use_occurrence_time_not_writer_commit_order() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let mut storage = SessionStorage::open(&config).unwrap();
        append_prompt(&mut storage, "newer", 200, "newer prompt");
        append_prompt(&mut storage, "older", 100, "older prompt");

        let prompts = storage.recent_prompts(10).unwrap();
        assert_eq!(prompts[0].text, "newer prompt");
        assert_eq!(prompts[1].text, "older prompt");
    }

    #[test]
    fn transcript_pages_continue_by_event_id_without_loading_the_session() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let mut storage = SessionStorage::open(&config).unwrap();
        append_prompt(&mut storage, "session", 100, "first prompt");
        append_prompt_record(&mut storage, "session", 3, 200, "second prompt");

        let first = storage
            .load_record_page("session", None, 2, usize::MAX)
            .unwrap()
            .unwrap();
        assert_eq!(first.records.len(), 2);
        assert!(first.next_cursor.is_some());

        let second = storage
            .load_record_page("session", first.next_cursor, 2, usize::MAX)
            .unwrap()
            .unwrap();
        assert_eq!(second.records.len(), 1);
        assert!(second.next_cursor.is_none());
        assert!(second.records[0].event_id > first.records[1].event_id);
    }

    #[test]
    fn transcript_page_does_not_decode_its_lookahead_record() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let mut storage = SessionStorage::open(&config).unwrap();
        append_prompt(&mut storage, "session", 100, "prompt");
        storage.append_raw_record("session", b"not-json").unwrap();

        let page = storage
            .load_record_page("session", None, 2, usize::MAX)
            .unwrap()
            .unwrap();

        assert_eq!(page.records.len(), 2);
        assert!(page.next_cursor.is_some());
    }

    #[test]
    fn transcript_page_byte_budget_allows_one_oversized_record_for_progress() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let mut storage = SessionStorage::open(&config).unwrap();
        append_prompt(&mut storage, "session", 100, "prompt");

        let first = storage
            .load_record_page("session", None, 100, 1)
            .unwrap()
            .unwrap();

        assert_eq!(first.records.len(), 1);
        assert!(first.next_cursor.is_some());
        let second = storage
            .load_record_page("session", first.next_cursor, 100, 1)
            .unwrap()
            .unwrap();
        assert_eq!(second.records.len(), 1);
        assert!(second.next_cursor.is_none());
    }

    #[test]
    fn transcript_page_distinguishes_a_missing_session_from_an_empty_page() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let storage = SessionStorage::open(&config).unwrap();

        assert!(
            storage
                .load_record_page("missing", None, 10, usize::MAX)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn mentionable_sessions_include_sessions_without_resume_state() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let mut storage = SessionStorage::open(&config).unwrap();
        append_prompt(&mut storage, "resumable", 100, "resumable prompt");
        append_prompt(&mut storage, "transcript-only", 200, "failed turn prompt");
        storage.save_resume_state("resumable", b"state").unwrap();

        let resumable = storage.list_sessions(Path::new("/work"), true).unwrap();
        let mentionable = storage.list_sessions(Path::new("/work"), false).unwrap();

        assert_eq!(
            resumable
                .iter()
                .map(|session| session.session_id.as_str())
                .collect::<Vec<_>>(),
            ["resumable"]
        );
        assert_eq!(mentionable.len(), 2);
    }

    #[test]
    fn session_discovery_filters_excludes_and_continues_in_stable_order() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let mut storage = SessionStorage::open(&config).unwrap();
        append_prompt(&mut storage, "current", 300, "needle");
        append_prompt(&mut storage, "alpha", 200, "first needle");
        append_prompt(&mut storage, "beta", 200, "second NEEDLE");
        append_prompt(&mut storage, "unrelated", 100, "different topic");
        let patterns = vec!["needle".to_owned()];

        let first = storage
            .find_sessions(
                "current",
                Some(Path::new("/work")),
                None,
                Some(&patterns),
                None,
                1,
            )
            .unwrap();
        assert_eq!(first.sessions.len(), 1);
        assert_eq!(first.sessions[0].session_id, "alpha");
        assert_eq!(first.sessions[0].updated_at_unix_ms, 200);
        assert_eq!(first.next_cursor, Some((200, "alpha".to_owned())));

        let second = storage
            .find_sessions(
                "current",
                Some(Path::new("/work")),
                None,
                Some(&patterns),
                Some((200, "alpha")),
                1,
            )
            .unwrap();
        assert_eq!(second.sessions.len(), 1);
        assert_eq!(second.sessions[0].session_id, "beta");
    }

    #[test]
    fn session_discovery_can_be_restricted_to_listed_sessions() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let mut storage = SessionStorage::open(&config).unwrap();
        append_prompt(&mut storage, "alpha", 200, "one");
        append_prompt(&mut storage, "beta", 100, "two");

        let only_beta = ["beta".to_owned()];
        let page = storage
            .find_sessions("current", None, Some(&only_beta), None, None, 10)
            .unwrap();
        let ids = page.sessions.iter().map(|s| s.session_id.as_str());
        assert_eq!(ids.collect::<Vec<_>>(), ["beta"]);

        let none = storage
            .find_sessions("current", None, Some(&[]), None, None, 10)
            .unwrap();
        assert!(none.sessions.is_empty());
    }

    #[test]
    fn title_claims_are_idempotent_and_manual_renames_win_late_generation() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let mut first = SessionStorage::open(&config).unwrap();
        append_prompt(&mut first, "session", 100, "original prompt");
        let second = SessionStorage::open(&config).unwrap();
        assert!(
            first
                .claim_title("session", "original prompt", true)
                .unwrap()
        );
        assert!(
            !second
                .claim_title("session", "competing fallback", true)
                .unwrap()
        );
        first.complete_title("session", "Generated title").unwrap();
        assert_eq!(second.titles().unwrap()["session"].text, "Generated title");
        second.rename_session("session", "My title").unwrap();
        first
            .complete_title("session", "Late generated title")
            .unwrap();
        assert!(
            !first
                .claim_title("session", "another attempt", true)
                .unwrap()
        );
        let reopened = SessionStorage::open(&config).unwrap();
        assert_eq!(reopened.titles().unwrap()["session"].text, "My title");
        assert_eq!(
            reopened.list_sessions(Path::new("/work"), false).unwrap()[0].preview,
            "My title"
        );
        assert!(reopened.untitled_prompts(2).unwrap().is_empty());
        assert_eq!(
            super::database_version(&reopened.connection, &reopened.path).unwrap(),
            2
        );
    }

    #[test]
    fn generation_claims_pend_until_they_reach_a_terminal_outcome() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let mut storage = SessionStorage::open(&config).unwrap();
        for (index, session) in ["generated", "failed", "abandoned", "renamed", "offline"]
            .into_iter()
            .enumerate()
        {
            append_prompt(&mut storage, session, 100 + index as u64, "first prompt");
            let generating = session != "offline";
            assert!(
                storage
                    .claim_title(session, "first prompt", generating)
                    .unwrap()
            );
        }
        let pending = |storage: &SessionStorage| {
            let mut pending = storage
                .titles()
                .unwrap()
                .into_iter()
                .filter(|(_, title)| title.pending)
                .map(|(session, _)| session)
                .collect::<Vec<_>>();
            pending.sort();
            pending
        };
        assert_eq!(
            pending(&storage),
            ["abandoned", "failed", "generated", "renamed"]
        );
        let listed = storage.list_sessions(Path::new("/work"), false).unwrap();
        assert!(
            listed
                .iter()
                .any(|session| session.session_id == "failed" && session.title_pending)
        );

        storage
            .complete_title("generated", "Generated title")
            .unwrap();
        storage.settle_title("failed").unwrap();
        storage
            .connection
            .execute(
                "UPDATE title_claims SET claimed_at_ms = ?1 WHERE session_id = 'abandoned'",
                [super::unix_ms() - super::GENERATION_LIMIT.as_millis() as i64],
            )
            .unwrap();
        storage.rename_session("renamed", "My title").unwrap();

        let reader = SessionStorage::open_read_only(&config).unwrap().unwrap();
        assert!(pending(&reader).is_empty());
        let titles = reader.titles().unwrap();
        assert_eq!(titles["generated"].text, "Generated title");
        assert_eq!(titles["failed"].text, "first prompt");
        assert_eq!(titles["abandoned"].text, "first prompt");
        assert_eq!(titles["renamed"].text, "My title");
        assert!(
            reader
                .list_sessions(Path::new("/work"), false)
                .unwrap()
                .iter()
                .all(|session| !session.title_pending)
        );
    }

    #[test]
    fn forks_inherit_a_snapshot_of_the_parent_title_without_generation() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let mut storage = SessionStorage::open(&config).unwrap();
        append_prompt(&mut storage, "parent", 100, "parent prompt");
        storage.rename_session("parent", "Parent title").unwrap();
        append_fork(&mut storage, "fork", "parent");
        storage
            .rename_session("parent", "Later parent title")
            .unwrap();
        storage.inherit_titles().unwrap();
        assert_eq!(storage.titles().unwrap()["fork"].text, "Parent title");
        assert!(storage.untitled_prompts(2).unwrap().is_empty());
        storage.rename_session("fork", "Fork title").unwrap();
        storage.inherit_titles().unwrap();
        assert_eq!(storage.titles().unwrap()["fork"].text, "Fork title");
    }

    #[test]
    fn forks_bound_fallback_titles_and_backfill_legacy_lineages_in_parent_order() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let mut storage = SessionStorage::open(&config).unwrap();
        append_prompt(
            &mut storage,
            "parent",
            300,
            &format!("\n First line\n{}", "long context ".repeat(100)),
        );
        append_fork(&mut storage, "fork", "parent");
        assert_eq!(storage.titles().unwrap()["fork"].text, "First line");
        append_prompt(&mut storage, "legacy_child", 200, "child's own prompt");
        append_prompt(
            &mut storage,
            "legacy_grandchild",
            100,
            "grandchild's own prompt",
        );
        storage.connection.execute("UPDATE sessions SET parent_session_id = 'parent' WHERE session_id = 'legacy_child'", []).unwrap();
        storage.connection.execute("UPDATE sessions SET parent_session_id = 'legacy_child' WHERE session_id = 'legacy_grandchild'", []).unwrap();
        storage.inherit_titles().unwrap();
        let titles = storage.titles().unwrap();
        assert_eq!(titles["legacy_child"].text, "First line");
        assert_eq!(titles["legacy_grandchild"].text, "First line");
        let manual = "界".repeat(120);
        storage.rename_session("parent", &manual).unwrap();
        append_fork(&mut storage, "new_fork", "parent");
        assert_eq!(storage.titles().unwrap()["new_fork"].text, manual);
    }

    fn append_fork(storage: &mut SessionStorage, session: &str, parent: &str) {
        let fork = Arc::new(
            TranscriptRecord::from_local(
                1,
                200,
                LocalEvent::SessionStarted(SessionStarted {
                    session_id: session.to_owned(),
                    parent_session_id: Some(parent.to_owned()),
                    parent_sequence: Some(2),
                    model: "gpt-6-luna".to_owned(),
                    effort: ReasoningEffort::Low,
                    reasoning_mode: ReasoningMode::Standard,
                    speed: Speed::Standard,
                    workspace: "/work".into(),
                    application_version: "test".to_owned(),
                }),
            )
            .unwrap(),
        );
        storage.append_records(session, &[fork]).unwrap();
    }

    #[test]
    fn title_backfill_is_bounded_and_uses_only_the_first_prompt() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let mut storage = SessionStorage::open(&config).unwrap();
        append_prompt(&mut storage, "old", 100, "first prompt");
        append_prompt_record(&mut storage, "old", 3, 150, "later prompt");
        append_prompt(&mut storage, "new", 200, "new prompt");
        assert_eq!(
            storage.untitled_prompts(1).unwrap(),
            [("new".to_owned(), "new prompt".to_owned())]
        );
        storage.claim_title("new", "new prompt", true).unwrap();
        assert_eq!(
            storage.untitled_prompts(2).unwrap(),
            [("old".to_owned(), "first prompt".to_owned())]
        );
        storage
            .connection
            .execute("DROP TABLE session_titles", [])
            .unwrap();
        assert!(
            SessionStorage::open_read_only(&config)
                .unwrap()
                .unwrap()
                .titles()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            SessionStorage::open(&config)
                .unwrap()
                .titles()
                .unwrap()
                .len(),
            0
        );
    }

    fn append_prompt(storage: &mut SessionStorage, session_id: &str, at: u64, text: &str) {
        let records = [
            Arc::new(
                TranscriptRecord::from_local(
                    1,
                    at.saturating_sub(1),
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
            ),
            Arc::new(
                TranscriptRecord::from_local(
                    2,
                    at,
                    LocalEvent::UserSubmitted(UserSubmitted {
                        id: TurnId::new(1),
                        text: text.to_owned(),
                    }),
                )
                .unwrap(),
            ),
        ];
        storage.append_records(session_id, &records).unwrap();
    }

    fn append_prompt_record(
        storage: &mut SessionStorage,
        session_id: &str,
        sequence: u64,
        at: u64,
        text: &str,
    ) {
        let record = Arc::new(
            TranscriptRecord::from_local(
                sequence,
                at,
                LocalEvent::UserSubmitted(UserSubmitted {
                    id: TurnId::new(sequence),
                    text: text.to_owned(),
                }),
            )
            .unwrap(),
        );
        storage.append_records(session_id, &[record]).unwrap();
    }
}
