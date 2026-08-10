//! SQLite-backed V2 session persistence.

use crate::{
    app::config::{ReasoningEffort, ReasoningMode},
    tui::transcript::{SCHEMA_VERSION, SessionStarted, TranscriptRecord},
};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, params};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use thiserror::Error;

const FORMAT_VERSION: i32 = 2;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const RECORD_COMPRESSION_LEVEL: i32 = 1;
const STATE_COMPRESSION_LEVEL: i32 = 3;

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
    #[error("failed to compress session data: {0}")]
    Compress(#[source] std::io::Error),
    #[error("failed to decode session data: {0}")]
    Decode(#[source] std::io::Error),
    #[error("stored transcript record uses schema version {found}; expected {SCHEMA_VERSION}")]
    UnsupportedRecordVersion { found: u32 },
}

#[derive(Debug)]
pub(crate) struct StoredSession {
    pub(crate) session_id: String,
    pub(crate) started_at_unix_ms: u64,
    pub(crate) model: String,
    pub(crate) effort: ReasoningEffort,
    pub(crate) reasoning_mode: ReasoningMode,
    pub(crate) workspace: PathBuf,
    pub(crate) preview: String,
}

#[derive(Debug)]
pub(crate) struct StoredPrompt {
    pub(crate) text: String,
    pub(crate) recorded_at_unix_ms: u64,
    pub(crate) session_id: String,
    pub(crate) workspace: PathBuf,
}

pub(crate) struct SessionStorage {
    path: PathBuf,
    connection: Connection,
    active_turns: HashMap<String, u64>,
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
        set_private_file_permissions(&path)?;
        Ok(Self {
            path,
            connection,
            active_turns: HashMap::new(),
        })
    }

    pub(crate) fn append_records(
        &mut self,
        session_id: &str,
        records: &[Arc<TranscriptRecord>],
    ) -> Result<(), StorageError> {
        self.append_records_and_resume_state(session_id, records, None)
    }

    pub(crate) fn append_records_and_resume_state(
        &mut self,
        session_id: &str,
        records: &[Arc<TranscriptRecord>],
        resume_state: Option<&[u8]>,
    ) -> Result<(), StorageError> {
        if records.is_empty() && resume_state.is_none() {
            return Ok(());
        }
        let compressed_state = resume_state
            .map(|state| zstd::encode_all(state, STATE_COMPRESSION_LEVEL))
            .transpose()
            .map_err(StorageError::Compress)?;
        let path = self.path.clone();
        let transaction = self
            .connection
            .transaction()
            .map_err(|source| query(&path, source))?;
        let mut active_turn = self.active_turns.get(session_id).copied();
        for record in records {
            if record.source() == "tact" && record.kind() == "worker.turn_accepted" {
                #[derive(serde::Deserialize)]
                struct AcceptedTurn {
                    id: u64,
                }
                active_turn = Some(record.decode_payload::<AcceptedTurn>()?.id);
            }
            append_record(&transaction, &path, session_id, active_turn, record)?;
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
        let mut statement = self
            .connection
            .prepare("SELECT record_zstd FROM events WHERE session_id = ?1 ORDER BY event_id")
            .map_err(|source| query(&self.path, source))?;
        let rows = statement
            .query_map([session_id], |row| row.get::<_, Vec<u8>>(0))
            .map_err(|source| query(&self.path, source))?;
        let compressed = rows
            .map(|row| row.map_err(|source| query(&self.path, source)))
            .collect::<Result<Vec<_>, _>>()?;
        compressed
            .iter()
            .map(|record| decode_record(record).map(Arc::new))
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn save_resume_state(
        &mut self,
        session_id: &str,
        encoded: &[u8],
    ) -> Result<(), StorageError> {
        let compressed =
            zstd::encode_all(encoded, STATE_COMPRESSION_LEVEL).map_err(StorageError::Compress)?;
        write_resume_state(&self.connection, &self.path, session_id, &compressed)
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

    pub(crate) fn list_sessions(
        &self,
        workspace: &Path,
    ) -> Result<Vec<StoredSession>, StorageError> {
        let workspace = workspace.to_string_lossy();
        let mut statement = self
            .connection
            .prepare(
                "SELECT s.session_id, s.started_at_ms, s.model, s.effort, s.reasoning_mode,\n\
                        s.workspace, s.preview\n\
                 FROM sessions s INNER JOIN resume_states r ON r.session_id = s.session_id\n\
                 WHERE s.workspace = ?1\n\
                 ORDER BY s.updated_at_ms DESC, s.session_id",
            )
            .map_err(|source| query(&self.path, source))?;
        let rows = statement
            .query_map([workspace.as_ref()], decode_session)
            .map_err(|source| query(&self.path, source))?;
        rows.map(|row| row.map_err(|source| query(&self.path, source)))
            .collect()
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

fn decode_record(compressed: &[u8]) -> Result<TranscriptRecord, StorageError> {
    let encoded = zstd::decode_all(compressed).map_err(StorageError::Decode)?;
    let record = serde_json::from_slice::<TranscriptRecord>(&encoded)?;
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
    let version = connection
        .query_row("PRAGMA user_version", [], |row| row.get::<_, i32>(0))
        .map_err(|source| query(path, source))?;
    if version != 0 && version != FORMAT_VERSION {
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
                 record_zstd BLOB NOT NULL, prompt_text TEXT, prompt_recorded_at_ms INTEGER,\n\
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

fn append_record(
    transaction: &Transaction<'_>,
    path: &Path,
    session_id: &str,
    active_turn: Option<u64>,
    record: &TranscriptRecord,
) -> Result<(), StorageError> {
    if record.source() == "tact" && record.kind() == "session.started" {
        let started = record.decode_payload::<SessionStarted>()?;
        upsert_session(transaction, path, record, &started)?;
    }
    let prompt_text = prompt_text(record)?;
    let assistant_stream = assistant_stream(active_turn, record)?;
    if record.kind() == "assistant.message"
        && let Some(stream) = &assistant_stream
    {
        transaction
            .execute(
                "DELETE FROM events WHERE session_id = ?1 AND assistant_stream = ?2",
                params![session_id, stream],
            )
            .map_err(|source| query(path, source))?;
    }
    let encoded = serde_json::to_vec(record)?;
    let compressed = zstd::encode_all(encoded.as_slice(), RECORD_COMPRESSION_LEVEL)
        .map_err(StorageError::Compress)?;
    transaction
        .execute(
            "INSERT INTO events(\n\
                 session_id, record_zstd, prompt_text, prompt_recorded_at_ms, assistant_stream\n\
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                session_id,
                compressed,
                prompt_text,
                prompt_text
                    .as_ref()
                    .map(|_| to_sql_u64(record.recorded_at_unix_ms())),
                (record.kind() == "assistant.delta")
                    .then_some(assistant_stream)
                    .flatten()
            ],
        )
        .map_err(|source| query(path, source))?;
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
    update_settings(transaction, path, session_id, record)
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
                started.fast_mode,
                started.application_version,
                to_sql_u64(record.recorded_at_unix_ms())
            ],
        )
        .map_err(|source| query(path, source))?;
    Ok(())
}

fn prompt_text(record: &TranscriptRecord) -> Result<Option<String>, serde_json::Error> {
    #[derive(serde::Deserialize)]
    struct Prompt {
        text: String,
    }
    if record.source() != "tact" || !matches!(record.kind(), "user.submitted" | "user.steered") {
        return Ok(None);
    }
    Ok(Some(record.decode_payload::<Prompt>()?.text))
}

fn assistant_stream(
    active_turn: Option<u64>,
    record: &TranscriptRecord,
) -> Result<Option<String>, serde_json::Error> {
    #[derive(serde::Deserialize, serde::Serialize)]
    struct AssistantRecord {
        model_call_index: u32,
        phase: Option<serde_json::Value>,
    }
    if !matches!(record.kind(), "assistant.delta" | "assistant.message") {
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
    #[derive(serde::Deserialize)]
    struct EffortChanged {
        to: ReasoningEffort,
    }
    #[derive(serde::Deserialize)]
    struct FastModeChanged {
        to: bool,
    }
    match (record.source(), record.kind()) {
        ("tact", "effort.changed") => {
            let effort = serde_json::to_string(&record.decode_payload::<EffortChanged>()?.to)?;
            transaction
                .execute(
                    "UPDATE sessions SET effort=?2 WHERE session_id=?1",
                    params![session_id, effort],
                )
                .map_err(|source| query(path, source))?;
        }
        ("tact", "fast_mode.changed") => {
            let enabled = record.decode_payload::<FastModeChanged>()?.to;
            transaction
                .execute(
                    "UPDATE sessions SET fast_mode=?2 WHERE session_id=?1",
                    params![session_id, enabled],
                )
                .map_err(|source| query(path, source))?;
        }
        _ => {}
    }
    Ok(())
}

fn decode_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredSession> {
    let effort = row.get::<_, String>(3)?;
    let reasoning_mode = row.get::<_, String>(4)?;
    Ok(StoredSession {
        session_id: row.get(0)?,
        started_at_unix_ms: from_sql_u64(row.get(1)?),
        model: row.get(2)?,
        effort: decode_json_column(3, &effort)?,
        reasoning_mode: decode_json_column(4, &reasoning_mode)?,
        workspace: PathBuf::from(row.get::<_, String>(5)?),
        preview: row.get(6)?,
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
        app::config::{ReasoningEffort, ReasoningMode},
        tui::transcript::{LocalEvent, SessionStarted, TranscriptRecord, TurnId},
    };
    use rusqlite::Connection;
    use std::{fs, sync::Arc};
    use tempfile::tempdir;

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

    fn append_prompt(storage: &mut SessionStorage, session_id: &str, at: u64, text: &str) {
        let records = [
            Arc::new(
                TranscriptRecord::from_local(
                    1,
                    at.saturating_sub(1),
                    LocalEvent::SessionStarted(SessionStarted {
                        session_id: session_id.to_owned(),
                        parent_session_id: None,
                        model: "model".to_owned(),
                        effort: ReasoningEffort::Medium,
                        reasoning_mode: ReasoningMode::Standard,
                        fast_mode: false,
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
                    LocalEvent::UserSubmitted {
                        id: TurnId::new(1),
                        text: text.to_owned(),
                    },
                )
                .unwrap(),
            ),
        ];
        storage.append_records(session_id, &records).unwrap();
    }
}
