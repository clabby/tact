//! V2 resumable session storage and indexed session discovery.

use crate::{
    app::config::{ReasoningEffort, ReasoningMode},
    tui::{
        storage::{SessionStorage, StorageError},
        transcript::{SessionStarted, TranscriptRecord},
    },
};
use nanocodex::agent::session::SessionSnapshot;
use serde::{Deserialize, Serialize};
use std::{
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
}

pub(crate) struct ResumeState {
    snapshot: SessionSnapshot,
    instructions: String,
    skills_catalog_present: bool,
}

impl ResumeState {
    fn new(snapshot: SessionSnapshot, instructions: String, skills_catalog_present: bool) -> Self {
        Self {
            snapshot,
            instructions,
            skills_catalog_present,
        }
    }

    pub(crate) fn into_parts(self) -> (SessionSnapshot, String, Option<bool>) {
        (
            self.snapshot,
            self.instructions,
            Some(self.skills_catalog_present),
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
) -> Result<(), SessionError> {
    let encoded = encode_checkpoint(snapshot, instructions, skills_catalog_present)?;
    SessionStorage::open(config_path)?
        .save_resume_state(session_id, &encoded)
        .map_err(Into::into)
}

pub(crate) fn encode_checkpoint(
    snapshot: &SessionSnapshot,
    instructions: &str,
    skills_catalog_present: bool,
) -> Result<Vec<u8>, SessionError> {
    let state = StoredResumeState {
        format_version: RESUME_STATE_FORMAT_VERSION,
        snapshot: snapshot.clone(),
        instructions: instructions.to_owned(),
        skills_catalog_present,
    };
    serde_json::to_vec(&state)
        .map_err(StorageError::from)
        .map_err(Into::into)
}

pub(crate) fn load_checkpoint(
    config_path: &Path,
    session_id: &str,
) -> Result<ResumeState, SessionError> {
    let encoded = SessionStorage::open(config_path)?
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
    ))
}

#[allow(dead_code, reason = "used by session benchmarks")]
pub(crate) fn list(
    config_path: &Path,
    workspace: &Path,
) -> Result<Vec<SessionSummary>, SessionError> {
    SessionStorage::open(config_path)?
        .list_sessions(workspace)?
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
) -> Result<Vec<SessionSummary>, SessionError> {
    tokio::task::spawn_blocking(move || list(&config_path, &workspace))
        .await
        .map_err(SessionError::StorageTask)?
}

pub(crate) async fn load_recent_prompts_async(
    config_path: PathBuf,
) -> Result<Vec<RecentPrompt>, SessionError> {
    tokio::task::spawn_blocking(move || {
        let prompts = SessionStorage::open(&config_path)?.recent_prompts(MAX_RECENT_PROMPTS)?;
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
    SessionStorage::open(config_path)?
        .load_records(session_id)
        .map_err(Into::into)
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
        .find(|record| record.source() == "tact" && record.kind() == "session.started")
        .and_then(|record| record.decode_payload::<SessionStarted>().ok())
        .map_or(ReasoningMode::Standard, |started| started.reasoning_mode)
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
    use super::{encode_checkpoint, load_checkpoint, save_checkpoint};
    use crate::{
        app::config::{ReasoningEffort, ReasoningMode},
        tui::transcript::{LocalEvent, SessionStarted, TranscriptJournal, TurnId},
    };
    use nanocodex::agent::session::SessionSnapshot;
    use serde_json::{Value, json};
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

    #[test]
    fn resume_state_round_trips_the_opaque_nanocodex_snapshot() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let expected = snapshot("lineage");
        save_checkpoint(&config, "session", &expected, "exact instructions", true).unwrap();

        let restored = load_checkpoint(&config, "session").unwrap();
        let (actual, instructions, catalog) = restored.into_parts();

        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
        assert_eq!(instructions, "exact instructions");
        assert_eq!(catalog, Some(true));
        assert!(directory.path().join("sessions/v2.sqlite3").is_file());
        assert!(!directory.path().join("checkpoints").exists());
    }

    #[test]
    fn newer_successful_snapshot_atomically_replaces_the_previous_state() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        save_checkpoint(&config, "session", &snapshot("first"), "first", false).unwrap();
        save_checkpoint(&config, "session", &snapshot("second"), "second", true).unwrap();

        let restored = load_checkpoint(&config, "session").unwrap();
        let (snapshot, instructions, catalog) = restored.into_parts();
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
        save_checkpoint(&config, "session", &expected, "instructions", true).unwrap();

        let (mut journal, writer) = TranscriptJournal::open(&config, "session").unwrap();
        journal.defer_start(SessionStarted {
            session_id: "session".to_owned(),
            parent_session_id: None,
            model: nanocodex::oai::MODEL.to_owned(),
            effort: ReasoningEffort::Medium,
            reasoning_mode: ReasoningMode::Standard,
            fast_mode: false,
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
        let (actual, _, _) = restored.into_parts();
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
        let resume_state = encode_checkpoint(&expected, "instructions", true).unwrap();
        let (mut journal, writer) = TranscriptJournal::open(&config, "session").unwrap();
        journal.defer_start(SessionStarted {
            session_id: "session".to_owned(),
            parent_session_id: None,
            model: nanocodex::oai::MODEL.to_owned(),
            effort: ReasoningEffort::Medium,
            reasoning_mode: ReasoningMode::Standard,
            fast_mode: false,
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
        let (actual, instructions, catalog) =
            load_checkpoint(&config, "session").unwrap().into_parts();
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
        assert_eq!(instructions, "instructions");
        assert_eq!(catalog, Some(true));
    }
}
