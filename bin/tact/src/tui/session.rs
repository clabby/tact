//! V2 resumable session storage and indexed session discovery.

use crate::{
    app::config::{ReasoningEffort, ReasoningMode},
    core::SessionMemoryPolicy,
    tui::{
        storage::{ResumeStateRevision, SessionStorage, StorageError},
        transcript::{SessionStarted, TranscriptRecord},
    },
};
use nanocodex::{Model, agent::session::SessionSnapshot};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use thiserror::Error;

const RESUME_STATE_FORMAT_VERSION: u32 = 2;
pub(crate) const MAX_RECENT_PROMPTS: usize = 100;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct SessionSummary {
    pub(crate) session_id: String,
    pub(crate) started_at_unix_ms: u64,
    pub(crate) model: String,
    pub(crate) effort: ReasoningEffort,
    pub(crate) reasoning_mode: ReasoningMode,
    pub(crate) workspace: PathBuf,
    pub(crate) preview: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub(crate) struct RecentPrompt {
    pub(crate) text: String,
    pub(crate) recorded_at_unix_ms: u64,
    pub(crate) session_id: String,
    pub(crate) workspace: PathBuf,
}

#[derive(Deserialize, Serialize)]
struct StoredResumeState {
    format_version: u32,
    snapshot: SessionSnapshot,
    instructions: String,
    skills_catalog_present: bool,
    #[serde(default)]
    memory_policy: SessionMemoryPolicy,
}

pub(crate) struct ResumeState {
    snapshot: SessionSnapshot,
    instructions: String,
    skills_catalog_present: bool,
    memory_policy: SessionMemoryPolicy,
}

impl ResumeState {
    fn new(
        snapshot: SessionSnapshot,
        instructions: String,
        skills_catalog_present: bool,
        memory_policy: SessionMemoryPolicy,
    ) -> Self {
        Self {
            snapshot,
            instructions,
            skills_catalog_present,
            memory_policy,
        }
    }

    pub(crate) fn into_parts(self) -> (SessionSnapshot, String, Option<bool>, SessionMemoryPolicy) {
        (
            self.snapshot,
            self.instructions,
            Some(self.skills_catalog_present),
            self.memory_policy,
        )
    }
}

#[derive(Debug, Error)]
pub(crate) enum SessionError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("no resumable state exists for session {session_id}")]
    MissingCheckpoint { session_id: String },
    #[error(
        "session {session_id} uses resume-state format {found}; expected {RESUME_STATE_FORMAT_VERSION}"
    )]
    IncompatibleCheckpoint { session_id: String, found: u32 },
    #[error("session lineage contains a cycle at {session_id}")]
    LineageCycle { session_id: String },
    #[error("session lineage references missing ancestor {session_id}")]
    MissingAncestor { session_id: String },
    #[error("stored transcript for {session_id} has no matching session start")]
    InvalidLineageStart { session_id: String },
    #[error("session {session_id} does not contain lineage boundary {sequence}")]
    InvalidLineageBoundary { session_id: String, sequence: u64 },
    #[error("the session storage task stopped unexpectedly: {0}")]
    StorageTask(#[source] tokio::task::JoinError),
}

#[cfg(test)]
pub(crate) fn save_checkpoint(
    config_path: &Path,
    session_id: &str,
    snapshot: &SessionSnapshot,
    instructions: &str,
    skills_catalog_present: bool,
    memory_policy: SessionMemoryPolicy,
) -> Result<(), SessionError> {
    let encoded = encode_checkpoint(
        snapshot,
        instructions,
        skills_catalog_present,
        memory_policy,
    )?;
    SessionStorage::open(config_path)?
        .save_resume_state(session_id, &encoded)
        .map_err(Into::into)
}

pub(crate) fn encode_checkpoint(
    snapshot: &SessionSnapshot,
    instructions: &str,
    skills_catalog_present: bool,
    memory_policy: SessionMemoryPolicy,
) -> Result<Vec<u8>, SessionError> {
    let state = StoredResumeState {
        format_version: RESUME_STATE_FORMAT_VERSION,
        snapshot: snapshot.clone(),
        instructions: instructions.to_owned(),
        skills_catalog_present,
        memory_policy,
    };
    serde_json::to_vec(&state)
        .map_err(StorageError::from)
        .map_err(Into::into)
}

#[cfg(test)]
pub(crate) fn load_checkpoint(
    config_path: &Path,
    session_id: &str,
) -> Result<ResumeState, SessionError> {
    let encoded = SessionStorage::open_read_only(config_path)?
        .ok_or_else(|| SessionError::MissingCheckpoint {
            session_id: session_id.to_owned(),
        })?
        .load_resume_state(session_id)?
        .ok_or_else(|| SessionError::MissingCheckpoint {
            session_id: session_id.to_owned(),
        })?;
    let stored =
        serde_json::from_slice::<StoredResumeState>(&encoded).map_err(StorageError::from)?;
    if stored.format_version != RESUME_STATE_FORMAT_VERSION {
        return Err(SessionError::IncompatibleCheckpoint {
            session_id: session_id.to_owned(),
            found: stored.format_version,
        });
    }
    Ok(ResumeState::new(
        stored.snapshot,
        stored.instructions,
        stored.skills_catalog_present,
        stored.memory_policy,
    ))
}

pub(crate) fn load_checkpoint_with_policy(
    config_path: &Path,
    session_id: &str,
    requested_policy: SessionMemoryPolicy,
) -> Result<ResumeState, SessionError> {
    let storage = SessionStorage::open(config_path)?;
    let current = storage
        .load_resume_state_revision(session_id)?
        .ok_or_else(|| SessionError::MissingCheckpoint {
            session_id: session_id.to_owned(),
        })?;
    restrict_checkpoint_policy(&storage, session_id, requested_policy, current)
}

fn restrict_checkpoint_policy(
    storage: &SessionStorage,
    session_id: &str,
    requested_policy: SessionMemoryPolicy,
    mut current: ResumeStateRevision,
) -> Result<ResumeState, SessionError> {
    loop {
        let mut stored = serde_json::from_slice::<StoredResumeState>(current.encoded())
            .map_err(StorageError::from)?;
        if stored.format_version != RESUME_STATE_FORMAT_VERSION {
            return Err(SessionError::IncompatibleCheckpoint {
                session_id: session_id.to_owned(),
                found: stored.format_version,
            });
        }
        let effective_policy = requested_policy.restrict(stored.memory_policy);
        if effective_policy == stored.memory_policy {
            return Ok(ResumeState::new(
                stored.snapshot,
                stored.instructions,
                stored.skills_catalog_present,
                stored.memory_policy,
            ));
        }

        stored.memory_policy = effective_policy;
        let encoded = serde_json::to_vec(&stored).map_err(StorageError::from)?;
        if storage.replace_resume_state_if_current(session_id, &current, &encoded)? {
            return Ok(ResumeState::new(
                stored.snapshot,
                stored.instructions,
                stored.skills_catalog_present,
                stored.memory_policy,
            ));
        }
        current = storage
            .load_resume_state_revision(session_id)?
            .ok_or_else(|| SessionError::MissingCheckpoint {
                session_id: session_id.to_owned(),
            })?;
    }
}

#[allow(dead_code, reason = "used by session benchmarks")]
pub(crate) fn list(
    config_path: &Path,
    workspace: &Path,
    resumable_only: bool,
) -> Result<Vec<SessionSummary>, SessionError> {
    let Some(storage) = SessionStorage::open_read_only(config_path)? else {
        return Ok(Vec::new());
    };
    storage
        .list_sessions(workspace, resumable_only)?
        .into_iter()
        .map(|session| {
            Ok(SessionSummary {
                session_id: session.session_id,
                started_at_unix_ms: session.started_at_unix_ms,
                model: session.model,
                effort: session.effort,
                reasoning_mode: session.reasoning_mode,
                workspace: session.workspace,
                preview: session.preview,
            })
        })
        .collect()
}

pub(crate) async fn list_async(
    config_path: PathBuf,
    workspace: PathBuf,
    resumable_only: bool,
) -> Result<Vec<SessionSummary>, SessionError> {
    tokio::task::spawn_blocking(move || list(&config_path, &workspace, resumable_only))
        .await
        .map_err(SessionError::StorageTask)?
}

pub(crate) async fn load_recent_prompts_async(
    config_path: PathBuf,
) -> Result<Vec<RecentPrompt>, SessionError> {
    tokio::task::spawn_blocking(move || {
        let Some(storage) = SessionStorage::open_read_only(&config_path)? else {
            return Ok(Vec::new());
        };
        let prompts = storage.recent_prompts(MAX_RECENT_PROMPTS)?;
        Ok(prompts
            .into_iter()
            .map(|prompt| RecentPrompt {
                text: prompt.text,
                recorded_at_unix_ms: prompt.recorded_at_unix_ms,
                session_id: prompt.session_id,
                workspace: prompt.workspace,
            })
            .collect())
    })
    .await
    .map_err(SessionError::StorageTask)?
}

#[allow(dead_code, reason = "used by session benchmarks")]
pub(crate) fn load_transcript(
    config_path: &Path,
    session_id: &str,
) -> Result<Vec<Arc<TranscriptRecord>>, SessionError> {
    let Some(storage) = SessionStorage::open_read_only(config_path)? else {
        return Ok(Vec::new());
    };
    let mut records = Vec::new();
    load_lineage(
        &storage,
        session_id,
        None,
        &mut HashSet::new(),
        &mut records,
    )?;
    Ok(records)
}

fn load_lineage(
    storage: &SessionStorage,
    session_id: &str,
    through_sequence: Option<u64>,
    loading: &mut HashSet<String>,
    records: &mut Vec<Arc<TranscriptRecord>>,
) -> Result<(), SessionError> {
    if through_sequence == Some(0) {
        return Ok(());
    }
    if !loading.insert(session_id.to_owned()) {
        return Err(SessionError::LineageCycle {
            session_id: session_id.to_owned(),
        });
    }
    let (local, boundary_found, session_found) = match through_sequence {
        Some(sequence) => {
            let prefix = storage.load_records_through(session_id, sequence)?;
            (prefix.records, prefix.boundary_found, prefix.session_found)
        }
        None => (storage.load_records(session_id)?, true, true),
    };
    if local.is_empty() {
        loading.remove(session_id);
        if through_sequence.is_some() && !session_found {
            return Err(SessionError::MissingAncestor {
                session_id: session_id.to_owned(),
            });
        }
        if let Some(sequence) = through_sequence
            && !boundary_found
        {
            return Err(SessionError::InvalidLineageBoundary {
                session_id: session_id.to_owned(),
                sequence,
            });
        }
        return Ok(());
    }
    if let Some(sequence) = through_sequence
        && !boundary_found
    {
        loading.remove(session_id);
        return Err(SessionError::InvalidLineageBoundary {
            session_id: session_id.to_owned(),
            sequence,
        });
    }
    let started = local
        .iter()
        .find(|record| record.source() == "tact" && record.kind() == "session.started")
        .and_then(|record| record.decode_payload::<SessionStarted>().ok());
    if through_sequence.is_some()
        && !started
            .as_ref()
            .is_some_and(|started| started.session_id == session_id)
    {
        return Err(SessionError::InvalidLineageStart {
            session_id: session_id.to_owned(),
        });
    }
    if let Some(started) = started
        && let (Some(parent), Some(parent_sequence)) =
            (started.parent_session_id, started.parent_sequence)
    {
        load_lineage(storage, &parent, Some(parent_sequence), loading, records)?;
    }
    records.extend(local);
    loading.remove(session_id);
    Ok(())
}

pub(crate) async fn load_transcript_async(
    config_path: PathBuf,
    session_id: String,
) -> Result<Vec<Arc<TranscriptRecord>>, SessionError> {
    tokio::task::spawn_blocking(move || load_transcript(&config_path, &session_id))
        .await
        .map_err(SessionError::StorageTask)?
}

pub(crate) fn reasoning_mode(records: &[Arc<TranscriptRecord>]) -> ReasoningMode {
    records
        .iter()
        .rev()
        .find(|record| record.source() == "tact" && record.kind() == "session.started")
        .and_then(|record| record.decode_payload::<SessionStarted>().ok())
        .map_or(ReasoningMode::Standard, |started| started.reasoning_mode)
}

pub(crate) fn model(records: &[Arc<TranscriptRecord>]) -> Model {
    records
        .iter()
        .rev()
        .find(|record| record.source() == "tact" && record.kind() == "session.started")
        .and_then(|record| record.decode_payload::<SessionStarted>().ok())
        .and_then(|started| started.model.parse().ok())
        .unwrap_or(Model::Sol)
}

pub(crate) fn next_sequence(records: &[Arc<TranscriptRecord>]) -> u64 {
    let current_session_id = records.iter().rev().find_map(|record| {
        (record.source() == "tact" && record.kind() == "session.started")
            .then(|| record.decode_payload::<SessionStarted>().ok())
            .flatten()
            .map(|started| started.session_id)
    });
    let Some(current_session_id) = current_session_id else {
        return 1;
    };
    let mut segment = None::<String>;
    let mut maximum = 0;
    for record in records {
        if record.source() == "tact" && record.kind() == "session.started" {
            segment = record
                .decode_payload::<SessionStarted>()
                .ok()
                .map(|started| started.session_id);
        }
        if segment.as_deref() == Some(&current_session_id) {
            maximum = maximum.max(record.sequence());
        }
    }
    maximum.saturating_add(1).max(1)
}

pub(crate) fn format_age(started_at_unix_ms: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let started = Duration::from_millis(started_at_unix_ms);
    let elapsed = now.saturating_sub(started);
    let seconds = elapsed.as_secs();
    match seconds {
        0..=59 => "now".to_owned(),
        60..=3_599 => format!("{}m", seconds / 60),
        3_600..=86_399 => format!("{}h", seconds / 3_600),
        _ => format!("{}d", seconds / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        encode_checkpoint, load_checkpoint, load_checkpoint_with_policy, load_transcript, model,
        restrict_checkpoint_policy, save_checkpoint,
    };
    use crate::{
        app::config::{ReasoningEffort, ReasoningMode},
        core::SessionMemoryPolicy,
        tui::{
            storage::{SessionStorage, database_path},
            transcript::{LocalEvent, SessionStarted, TranscriptJournal, TranscriptRecord, TurnId},
        },
    };
    use nanocodex::{Model, agent::session::SessionSnapshot};
    use rusqlite::Connection;
    use serde_json::{Value, json};
    use std::sync::Arc;
    use tempfile::tempdir;

    fn snapshot(lineage: &str) -> SessionSnapshot {
        serde_json::from_value(json!({
            "version": 1,
            "model": nanocodex::oai::MODEL,
            "lineage_id": lineage,
            "prompt_cache_key": format!("cache-{lineage}"),
            "workspace": "/work",
            "request_prefix": [
                {"type": "additional_tools", "role": "developer", "tools": []},
                {"type": "message", "role": "developer", "content": []}
            ],
            "canonical_context": {
                "type": "message", "role": "user",
                "content": [{"type": "input_text", "text": "canonical"}]
            },
            "history": [
                {"type": "message", "role": "user",
                 "content": [{"type": "input_text", "text": "hello"}]}
            ]
        }))
        .unwrap()
    }

    fn started(
        sequence: u64,
        session_id: &str,
        parent_session_id: Option<&str>,
        parent_sequence: Option<u64>,
    ) -> Arc<TranscriptRecord> {
        Arc::new(
            TranscriptRecord::from_local(
                sequence,
                sequence,
                LocalEvent::SessionStarted(SessionStarted {
                    session_id: session_id.to_owned(),
                    parent_session_id: parent_session_id.map(str::to_owned),
                    parent_sequence,
                    model: Model::Luna.to_string(),
                    effort: ReasoningEffort::Medium,
                    reasoning_mode: ReasoningMode::Standard,
                    fast_mode: false,
                    memory_policy: Default::default(),
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
                sequence,
                LocalEvent::UserSubmitted {
                    id: TurnId::new(sequence),
                    text: text.to_owned(),
                },
            )
            .unwrap(),
        )
    }

    #[test]
    fn loading_a_missing_session_does_not_create_storage() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");

        assert!(load_transcript(&config, "missing").unwrap().is_empty());
        assert!(!database_path(&config).exists());
    }

    #[test]
    fn restored_model_comes_from_the_session_start_record() {
        let record = TranscriptRecord::from_local(
            1,
            1,
            LocalEvent::SessionStarted(SessionStarted {
                session_id: "session".to_owned(),
                parent_session_id: None,
                parent_sequence: None,
                model: Model::Luna.to_string(),
                effort: ReasoningEffort::Medium,
                reasoning_mode: ReasoningMode::Standard,
                fast_mode: false,
                memory_policy: Default::default(),
                workspace: "/work".into(),
                application_version: "test".to_owned(),
            }),
        )
        .unwrap();

        assert_eq!(model(&[Arc::new(record)]), Model::Luna);
        assert_eq!(model(&[]), Model::Sol);
    }

    #[test]
    fn loading_a_session_does_not_require_a_writer_lock() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let records = [
            Arc::new(
                TranscriptRecord::from_local(
                    1,
                    1,
                    LocalEvent::SessionStarted(SessionStarted {
                        session_id: "session".to_owned(),
                        parent_session_id: None,
                        parent_sequence: None,
                        model: "model".to_owned(),
                        effort: ReasoningEffort::Medium,
                        reasoning_mode: ReasoningMode::Standard,
                        fast_mode: false,
                        memory_policy: Default::default(),
                        workspace: "/work".into(),
                        application_version: "test".to_owned(),
                    }),
                )
                .unwrap(),
            ),
            Arc::new(
                TranscriptRecord::from_local(
                    2,
                    2,
                    LocalEvent::UserSubmitted {
                        id: TurnId::new(1),
                        text: "inspect storage".to_owned(),
                    },
                )
                .unwrap(),
            ),
        ];
        SessionStorage::open(&config)
            .unwrap()
            .append_records("session", &records)
            .unwrap();
        let writer = Connection::open(database_path(&config)).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();

        let loaded = load_transcript(&config, "session").unwrap();

        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].kind(), "session.started");
        assert_eq!(loaded[1].kind(), "user.submitted");
    }

    #[test]
    fn fork_resume_loads_its_parent_only_through_the_fork_boundary() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let mut storage = SessionStorage::open(&config).unwrap();
        storage
            .append_records(
                "parent",
                &[
                    started(1, "parent", None, None),
                    prompt(2, "before the fork"),
                    prompt(3, "later in the parent"),
                ],
            )
            .unwrap();
        storage
            .append_records(
                "fork",
                &[
                    started(1, "fork", Some("parent"), Some(2)),
                    prompt(2, "inside the fork"),
                    prompt(3, "later in the fork"),
                ],
            )
            .unwrap();
        storage
            .append_records(
                "grandchild",
                &[
                    started(1, "grandchild", Some("fork"), Some(2)),
                    prompt(2, "inside the grandchild"),
                ],
            )
            .unwrap();

        let loaded = load_transcript(&config, "grandchild").unwrap();
        let prompts = loaded
            .iter()
            .filter(|record| record.kind() == "user.submitted")
            .map(|record| record.payload_json().to_owned())
            .collect::<Vec<_>>();

        assert_eq!(
            prompts,
            [
                r#"{"id":2,"text":"before the fork"}"#,
                r#"{"id":2,"text":"inside the fork"}"#,
                r#"{"id":2,"text":"inside the grandchild"}"#,
            ]
        );
        assert_eq!(
            loaded
                .iter()
                .filter(|record| record.kind() == "session.started")
                .count(),
            3
        );
    }

    #[test]
    fn resumed_session_sequences_continue_after_every_existing_segment() {
        let records = [
            started(1, "parent", None, None),
            prompt(8, "parent prompt"),
            started(1, "fork", None, None),
            prompt(2, "first fork segment"),
            started(3, "fork", None, None),
            prompt(4, "second fork segment"),
        ];

        assert_eq!(super::next_sequence(&records), 5);
    }

    #[test]
    fn fork_resume_rejects_a_missing_nonempty_ancestor() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let started = started(1, "fork", Some("missing"), Some(1));
        SessionStorage::open(&config)
            .unwrap()
            .append_records("fork", &[started])
            .unwrap();

        let error = load_transcript(&config, "fork").unwrap_err();

        assert!(matches!(
            error,
            super::SessionError::MissingAncestor { session_id } if session_id == "missing"
        ));
    }

    #[test]
    fn fork_resume_rejects_a_lineage_cycle() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let mut storage = SessionStorage::open(&config).unwrap();
        storage
            .append_records("one", &[started(1, "one", Some("two"), Some(1))])
            .unwrap();
        storage
            .append_records("two", &[started(1, "two", Some("one"), Some(1))])
            .unwrap();

        let error = load_transcript(&config, "one").unwrap_err();

        assert!(matches!(
            error,
            super::SessionError::LineageCycle { session_id } if session_id == "one"
        ));
    }

    #[test]
    fn fork_resume_stops_decoding_at_the_parent_boundary() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let mut storage = SessionStorage::open(&config).unwrap();
        storage
            .append_records(
                "parent",
                &[started(1, "parent", None, None), prompt(2, "inherited")],
            )
            .unwrap();
        storage.append_raw_record("parent", b"not-json").unwrap();
        storage
            .append_records("fork", &[started(1, "fork", Some("parent"), Some(2))])
            .unwrap();

        let loaded = load_transcript(&config, "fork").unwrap();

        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[1].payload_json(), r#"{"id":2,"text":"inherited"}"#);
    }

    #[test]
    fn fork_resume_rejects_a_boundary_that_is_not_persisted() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let mut storage = SessionStorage::open(&config).unwrap();
        storage
            .append_records(
                "parent",
                &[started(1, "parent", None, None), prompt(3, "after gap")],
            )
            .unwrap();
        storage
            .append_records("fork", &[started(1, "fork", Some("parent"), Some(2))])
            .unwrap();

        assert!(load_transcript(&config, "fork").is_err());
    }

    #[test]
    fn resume_state_round_trips_the_opaque_nanocodex_snapshot() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let expected = snapshot("lineage");
        save_checkpoint(
            &config,
            "session",
            &expected,
            "exact instructions",
            true,
            Default::default(),
        )
        .unwrap();

        let restored = load_checkpoint(&config, "session").unwrap();
        let (actual, instructions, catalog, policy) = restored.into_parts();

        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
        assert_eq!(instructions, "exact instructions");
        assert_eq!(catalog, Some(true));
        assert_eq!(policy, SessionMemoryPolicy::Configured);
        assert!(directory.path().join("sessions/v2.sqlite3").is_file());
        assert!(!directory.path().join("checkpoints").exists());
    }

    #[test]
    fn resume_state_defaults_legacy_policy_and_preserves_disabled_policy() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let expected = snapshot("lineage");
        let encoded = encode_checkpoint(
            &expected,
            "instructions",
            true,
            SessionMemoryPolicy::Configured,
        )
        .unwrap();
        let mut legacy = serde_json::from_slice::<Value>(&encoded).unwrap();
        legacy.as_object_mut().unwrap().remove("memory_policy");
        SessionStorage::open(&config)
            .unwrap()
            .save_resume_state("legacy", &serde_json::to_vec(&legacy).unwrap())
            .unwrap();

        let (_, _, _, legacy_policy) = load_checkpoint(&config, "legacy").unwrap().into_parts();
        assert_eq!(legacy_policy, SessionMemoryPolicy::Configured);

        save_checkpoint(
            &config,
            "disabled",
            &expected,
            "instructions",
            true,
            SessionMemoryPolicy::Disabled,
        )
        .unwrap();
        let (_, _, _, disabled_policy) = load_checkpoint(&config, "disabled").unwrap().into_parts();
        assert_eq!(disabled_policy, SessionMemoryPolicy::Disabled);
    }

    #[test]
    fn restrictive_resume_policy_persists_without_a_new_snapshot() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let expected = snapshot("configured-lineage");
        save_checkpoint(
            &config,
            "session",
            &expected,
            "exact instructions",
            true,
            SessionMemoryPolicy::Configured,
        )
        .unwrap();

        let restricted =
            load_checkpoint_with_policy(&config, "session", SessionMemoryPolicy::Disabled).unwrap();
        let (restricted_snapshot, restricted_instructions, restricted_catalog, restricted_policy) =
            restricted.into_parts();
        assert_eq!(
            serde_json::to_value(restricted_snapshot).unwrap(),
            serde_json::to_value(&expected).unwrap()
        );
        assert_eq!(restricted_instructions, "exact instructions");
        assert_eq!(restricted_catalog, Some(true));
        assert_eq!(restricted_policy, SessionMemoryPolicy::Disabled);

        let later =
            load_checkpoint_with_policy(&config, "session", SessionMemoryPolicy::Configured)
                .unwrap();
        let (later_snapshot, later_instructions, later_catalog, later_policy) = later.into_parts();
        assert_eq!(
            serde_json::to_value(later_snapshot).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
        assert_eq!(later_instructions, "exact instructions");
        assert_eq!(later_catalog, Some(true));
        assert_eq!(later_policy, SessionMemoryPolicy::Disabled);
    }

    #[test]
    fn stale_restriction_update_preserves_a_newer_checkpoint() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        save_checkpoint(
            &config,
            "session",
            &snapshot("old-lineage"),
            "old instructions",
            false,
            SessionMemoryPolicy::Configured,
        )
        .unwrap();

        let storage = SessionStorage::open(&config).unwrap();
        let stale = storage
            .load_resume_state_revision("session")
            .unwrap()
            .unwrap();

        let newer = snapshot("newer-lineage");
        save_checkpoint(
            &config,
            "session",
            &newer,
            "newer instructions",
            true,
            SessionMemoryPolicy::Configured,
        )
        .unwrap();
        let restricted =
            restrict_checkpoint_policy(&storage, "session", SessionMemoryPolicy::Disabled, stale)
                .unwrap();
        let (actual, instructions, catalog, policy) = restricted.into_parts();
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            serde_json::to_value(newer).unwrap()
        );
        assert_eq!(instructions, "newer instructions");
        assert_eq!(catalog, Some(true));
        assert_eq!(policy, SessionMemoryPolicy::Disabled);
    }

    #[test]
    fn newer_successful_snapshot_atomically_replaces_the_previous_state() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        save_checkpoint(
            &config,
            "session",
            &snapshot("first"),
            "first",
            false,
            Default::default(),
        )
        .unwrap();
        save_checkpoint(
            &config,
            "session",
            &snapshot("second"),
            "second",
            true,
            Default::default(),
        )
        .unwrap();

        let restored = load_checkpoint(&config, "session").unwrap();
        let (snapshot, instructions, catalog, _) = restored.into_parts();
        let snapshot = serde_json::to_value(snapshot).unwrap();

        assert_eq!(snapshot["lineage_id"], Value::String("second".to_owned()));
        assert_eq!(instructions, "second");
        assert_eq!(catalog, Some(true));
    }

    #[tokio::test]
    async fn failed_turn_tail_does_not_replace_the_last_successful_snapshot() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let expected = snapshot("successful");
        save_checkpoint(
            &config,
            "session",
            &expected,
            "instructions",
            true,
            Default::default(),
        )
        .unwrap();

        let (mut journal, writer) = TranscriptJournal::open(&config, "session").unwrap();
        journal.defer_start(SessionStarted {
            session_id: "session".to_owned(),
            parent_session_id: None,
            parent_sequence: None,
            model: nanocodex::oai::MODEL.to_owned(),
            effort: ReasoningEffort::Medium,
            reasoning_mode: ReasoningMode::Standard,
            fast_mode: false,
            memory_policy: Default::default(),
            workspace: "/work".into(),
            application_version: "test".to_owned(),
        });
        journal
            .append_local(LocalEvent::WorkerTurnFinished {
                id: TurnId::new(2),
                error: Some("API failed".to_owned()),
            })
            .unwrap();
        drop(journal);
        writer.into_task().await.unwrap().unwrap();

        let restored = load_checkpoint(&config, "session").unwrap();
        let (actual, _, _, _) = restored.into_parts();
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
        let records = super::load_transcript(&config, "session").unwrap();
        assert_eq!(records.last().unwrap().kind(), "worker.turn_finished");
    }

    #[tokio::test]
    async fn successful_turn_publishes_its_tail_and_snapshot_together() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let expected = snapshot("successful");
        let resume_state =
            encode_checkpoint(&expected, "instructions", true, Default::default()).unwrap();
        let (mut journal, writer) = TranscriptJournal::open(&config, "session").unwrap();
        journal.defer_start(SessionStarted {
            session_id: "session".to_owned(),
            parent_session_id: None,
            parent_sequence: None,
            model: nanocodex::oai::MODEL.to_owned(),
            effort: ReasoningEffort::Medium,
            reasoning_mode: ReasoningMode::Standard,
            fast_mode: false,
            memory_policy: Default::default(),
            workspace: "/work".into(),
            application_version: "test".to_owned(),
        });
        journal
            .append_local_with_resume_state(
                LocalEvent::WorkerTurnFinished {
                    id: TurnId::new(1),
                    error: None,
                },
                resume_state,
            )
            .unwrap();
        drop(journal);
        writer.into_task().await.unwrap().unwrap();

        let records = super::load_transcript(&config, "session").unwrap();
        assert_eq!(records.last().unwrap().kind(), "worker.turn_finished");
        let (actual, instructions, catalog, _) =
            load_checkpoint(&config, "session").unwrap().into_parts();
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
        assert_eq!(instructions, "instructions");
        assert_eq!(catalog, Some(true));
    }
}
