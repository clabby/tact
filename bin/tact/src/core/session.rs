//! V2 resumable session storage and indexed session discovery.

use super::context::ContextBudget;
use crate::{
    app::{
        config::{ReasoningEffort, ReasoningMode},
        model,
    },
    core::{
        storage::{SessionStorage, StorageError, StoredSession, database_path},
        transcript::{LocalKind, TerminalStopReason, TranscriptRecord},
    },
    search::rank,
};
use nanocodex::{
    HarnessModel, HarnessModel as Model, Model as CodexModel, NanocodexError, Thinking,
    agent::{ChildSnapshot, session::SessionSnapshot},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs::{self, File, TryLockError},
    path::{Path, PathBuf},
    sync::Arc,
};
use thiserror::Error;

const RESUME_STATE_FORMAT_VERSION: u32 = 2;
pub(crate) const MAX_RECENT_PROMPTS: usize = 100;
/// The most persisted sessions one history page holds.
const HISTORY_PAGE_SIZE: usize = 50;

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

impl From<StoredSession> for SessionSummary {
    fn from(session: StoredSession) -> Self {
        Self {
            session_id: session.session_id,
            started_at_unix_ms: session.started_at_unix_ms,
            model: session.model,
            effort: session.effort,
            reasoning_mode: session.reasoning_mode,
            workspace: session.workspace,
            preview: session.preview,
        }
    }
}

impl SessionSummary {
    /// Whether a session-picker search matches. `query` must already be lowercase.
    pub(crate) fn matches(&self, query: &str) -> bool {
        query.is_empty()
            || self.session_id.to_ascii_lowercase().contains(query)
            || self.preview.to_ascii_lowercase().contains(query)
            || self.model.to_ascii_lowercase().contains(query)
            || self
                .workspace
                .to_string_lossy()
                .to_ascii_lowercase()
                .contains(query)
    }
}

/// One page of persisted sessions matching a search, in listing order (newest first).
#[derive(Debug, Serialize)]
pub(crate) struct HistoryPage {
    pub(crate) sessions: Vec<SessionSummary>,
    /// Passed back as `cursor` to read the next page; `None` on the last page.
    pub(crate) next_cursor: Option<String>,
}

impl HistoryPage {
    /// Pages through `sessions` filtered by `query`. The cursor is opaque to clients; an
    /// unparsable one is a client error.
    pub(crate) fn new(
        sessions: Vec<SessionSummary>,
        query: &str,
        cursor: Option<&str>,
    ) -> Result<Self, String> {
        let skip = cursor
            .map(|cursor| {
                cursor
                    .parse::<usize>()
                    .map_err(|_| format!("invalid history cursor {cursor:?}"))
            })
            .transpose()?
            .unwrap_or(0);
        let query = query.to_ascii_lowercase();
        let mut matching = sessions
            .into_iter()
            .filter(|session| session.matches(&query))
            .skip(skip);
        let sessions = matching
            .by_ref()
            .take(HISTORY_PAGE_SIZE)
            .collect::<Vec<_>>();
        let next_cursor = matching.next().map(|_| (skip + sessions.len()).to_string());
        Ok(Self {
            sessions,
            next_cursor,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub(crate) struct RecentPrompt {
    pub(crate) text: String,
    pub(crate) recorded_at_unix_ms: u64,
    pub(crate) session_id: String,
    pub(crate) workspace: PathBuf,
}

/// Which prompts the recent-prompt picker offers.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RecentPromptScope {
    #[default]
    Global,
    CurrentSession,
}

impl RecentPromptScope {
    pub(crate) const fn toggled(self) -> Self {
        match self {
            Self::Global => Self::CurrentSession,
            Self::CurrentSession => Self::Global,
        }
    }
}

/// Indices of the `prompts` within `scope` that match `query`, best first. Ties keep the order of
/// `prompts`, which the loader sorts newest first.
pub(crate) fn rank_recent_prompts(
    prompts: &[RecentPrompt],
    current_session_id: &str,
    scope: RecentPromptScope,
    query: &str,
) -> Vec<usize> {
    let mut ranked = rank(prompts, query, |prompt| prompt.text.as_str());
    if scope == RecentPromptScope::CurrentSession {
        ranked.retain(|&index| prompts[index].session_id == current_session_id);
    }
    ranked
}

/// Recent prompts matching a picker query, best first.
#[derive(Debug, Serialize)]
pub(crate) struct RecentPrompts {
    pub(crate) prompts: Vec<RecentPrompt>,
}

impl RecentPrompts {
    pub(crate) fn new(
        prompts: Vec<RecentPrompt>,
        current_session_id: &str,
        scope: RecentPromptScope,
        query: &str,
    ) -> Self {
        let ranked = rank_recent_prompts(&prompts, current_session_id, scope, query);
        let mut slots = prompts.into_iter().map(Some).collect::<Vec<_>>();
        let prompts = ranked
            .into_iter()
            .filter_map(|index| slots[index].take())
            .collect();
        Self { prompts }
    }
}

/// Provider-owned checkpoints retain their native payload without translating conversation items.
#[derive(Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub(crate) enum AgentSnapshot {
    Claude(ClaudeSnapshot),
    Codex(Box<SessionSnapshot>),
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ClaudeSnapshot {
    model: Model,
    session_id: String,
    thinking: Thinking,
    payload: String,
    has_conversation: bool,
}

impl AgentSnapshot {
    /// Reads count-only metadata from the pinned Claude checkpoint schema.
    pub(crate) fn context_budget(&self) -> Option<ContextBudget> {
        #[derive(Deserialize)]
        struct Native {
            version: u32,
            context_window_tokens: u64,
            snapshot: Snapshot,
        }
        #[derive(Deserialize)]
        struct Snapshot {
            provider: String,
            version: u32,
            conversation: Conversation,
        }
        #[derive(Deserialize)]
        struct Conversation {
            active_context_tokens: u64,
        }
        let Self::Claude(snapshot) = self else {
            return None;
        };
        let native: Native = serde_json::from_str(&snapshot.payload).ok()?;
        (native.version == 1
            && native.snapshot.version == 1
            && native.snapshot.provider == "claude"
            && native.context_window_tokens > 0)
            .then_some(ContextBudget {
                active_tokens: native.snapshot.conversation.active_context_tokens,
                window_tokens: native.context_window_tokens,
            })
    }

    pub(crate) fn validate_identity(
        &self,
        model: Model,
        session_id: Option<&str>,
    ) -> Result<(), NanocodexError> {
        match self {
            Self::Claude(snapshot) if snapshot.model != model => {
                Err(NanocodexError::InvalidSessionSnapshot(
                    "checkpoint model does not match the selected model".into(),
                ))
            }
            Self::Claude(snapshot)
                if session_id.is_some_and(|expected| snapshot.session_id != expected) =>
            {
                Err(NanocodexError::InvalidSessionSnapshot(
                    "checkpoint session does not match the selected session".into(),
                ))
            }
            Self::Codex(_) if matches!(model, Model::Claude(_)) => {
                Err(NanocodexError::InvalidSessionSnapshot(
                    "cannot restore a Codex checkpoint with Claude".into(),
                ))
            }
            _ => Ok(()),
        }
    }

    pub(crate) fn from_claude(snapshot: ChildSnapshot) -> Result<Self, NanocodexError> {
        let ChildSnapshot::Native {
            model: HarnessModel::Claude(model),
            session_id,
            thinking,
            payload,
            has_conversation,
        } = snapshot
        else {
            return Err(NanocodexError::InvalidSessionSnapshot(
                "expected a Claude native checkpoint".into(),
            ));
        };
        let model = tact_subagents::parse_model(model.as_str())
            .map_err(|error| NanocodexError::InvalidSessionSnapshot(error.to_string()))?;
        Ok(Self::Claude(ClaudeSnapshot {
            model,
            session_id,
            thinking,
            payload,
            has_conversation,
        }))
    }

    pub(crate) fn into_codex(self) -> Result<SessionSnapshot, NanocodexError> {
        match self {
            Self::Codex(snapshot) => Ok(*snapshot),
            Self::Claude(_) => Err(NanocodexError::InvalidSessionSnapshot(
                "cannot restore a Claude checkpoint with Codex".into(),
            )),
        }
    }

    pub(crate) fn into_claude(self) -> Result<ChildSnapshot, NanocodexError> {
        let Self::Claude(snapshot) = self else {
            return Err(NanocodexError::InvalidSessionSnapshot(
                "cannot restore a Codex checkpoint with Claude".into(),
            ));
        };
        let Model::Claude(model) = snapshot.model else {
            return Err(NanocodexError::InvalidSessionSnapshot(
                "native checkpoint model is not Claude".into(),
            ));
        };
        Ok(ChildSnapshot::Native {
            model: HarnessModel::Claude(model),
            session_id: snapshot.session_id,
            thinking: snapshot.thinking,
            payload: snapshot.payload,
            has_conversation: snapshot.has_conversation,
        })
    }
}

#[derive(Deserialize, Serialize)]
struct StoredResumeState {
    format_version: u32,
    snapshot: AgentSnapshot,
    instructions: String,
    skills_catalog_present: bool,
}

pub(crate) struct ResumeState {
    snapshot: AgentSnapshot,
    instructions: String,
    skills_catalog_present: bool,
}

impl ResumeState {
    fn new(snapshot: AgentSnapshot, instructions: String, skills_catalog_present: bool) -> Self {
        Self {
            snapshot,
            instructions,
            skills_catalog_present,
        }
    }

    pub(crate) fn into_parts(self) -> (AgentSnapshot, String, Option<bool>) {
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
    #[error("session {session_id} cannot be resumed: {reason}")]
    TerminalStop {
        session_id: String,
        reason: TerminalStopReason,
    },
    #[error(
        "session {session_id} uses resume-state format {found}; expected {RESUME_STATE_FORMAT_VERSION}"
    )]
    IncompatibleCheckpoint { session_id: String, found: u32 },
    #[error("session transcript has no workspace metadata")]
    MissingWorkspace,
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
    #[error("stored session uses unsupported model {model:?}")]
    UnsupportedModel { model: String },
    #[error("session {session_id} is open in another Tact")]
    Locked { session_id: String },
    #[error("failed to lock session file {path}: {source}")]
    Lock {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Exclusive ownership of a live session across Tact processes, released on drop.
///
/// A session's journal has one writer, so a session is live in at most one pane of one process.
/// The lock is an advisory file lock that the operating system releases if the process exits.
#[derive(Debug)]
pub(crate) struct SessionLock {
    _file: File,
}

impl SessionLock {
    pub(crate) fn acquire(config_path: &Path, session_id: &str) -> Result<Self, SessionError> {
        let directory = database_path(config_path).with_file_name("locks");
        let path = directory.join(format!("{session_id}.lock"));
        let lock_error = |source| SessionError::Lock {
            path: path.clone(),
            source,
        };
        fs::create_dir_all(&directory).map_err(lock_error)?;
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(lock_error)?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(TryLockError::WouldBlock) => Err(SessionError::Locked {
                session_id: session_id.to_owned(),
            }),
            Err(TryLockError::Error(source)) => Err(lock_error(source)),
        }
    }
}

#[cfg(test)]
pub(crate) fn save_checkpoint(
    config_path: &Path,
    session_id: &str,
    snapshot: &SessionSnapshot,
    instructions: &str,
    skills_catalog_present: bool,
) -> Result<(), SessionError> {
    let encoded = encode_checkpoint(
        &AgentSnapshot::Codex(Box::new(snapshot.clone())),
        instructions,
        skills_catalog_present,
    )?;
    SessionStorage::open(config_path)?
        .save_resume_state(session_id, &encoded)
        .map_err(Into::into)
}

pub(crate) fn encode_checkpoint(
    snapshot: &AgentSnapshot,
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
    let storage = SessionStorage::open_read_only(config_path)?.ok_or_else(|| {
        SessionError::MissingCheckpoint {
            session_id: session_id.to_owned(),
        }
    })?;
    if let Some(reason) = storage.terminal_stop(session_id)? {
        return Err(SessionError::TerminalStop {
            session_id: session_id.to_owned(),
            reason,
        });
    }
    let encoded =
        storage
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
    resumable_only: bool,
) -> Result<Vec<SessionSummary>, SessionError> {
    let Some(storage) = SessionStorage::open_read_only(config_path)? else {
        return Ok(Vec::new());
    };
    Ok(storage
        .list_sessions(workspace, resumable_only)?
        .into_iter()
        .map(SessionSummary::from)
        .collect())
}

pub(crate) async fn list_async(
    config_path: PathBuf,
    workspace: PathBuf,
    resumable_only: bool,
) -> Result<Vec<SessionSummary>, SessionError> {
    let workspaces = tact_vcs::family_paths(&workspace).await;
    tokio::task::spawn_blocking(move || {
        let Some(storage) = SessionStorage::open_read_only(&config_path)? else {
            return Ok(Vec::new());
        };
        let mut sessions = Vec::new();
        for workspace in workspaces {
            sessions.extend(storage.list_sessions(&workspace, resumable_only)?);
        }
        sessions.sort_by(|left, right| {
            right
                .updated_at_unix_ms
                .cmp(&left.updated_at_unix_ms)
                .then_with(|| left.session_id.cmp(&right.session_id))
        });
        Ok(sessions.into_iter().map(SessionSummary::from).collect())
    })
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
    let started = local.iter().find_map(|record| record.session_started());
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
        .find_map(|record| record.session_started())
        .map_or(ReasoningMode::Standard, |started| started.reasoning_mode)
}

pub(crate) fn workspace(records: &[Arc<TranscriptRecord>]) -> Result<PathBuf, SessionError> {
    records
        .iter()
        .rev()
        .find_map(|record| record.session_started())
        .map(|started| started.workspace)
        .ok_or(SessionError::MissingWorkspace)
}

pub(crate) fn model(records: &[Arc<TranscriptRecord>]) -> Result<Model, SessionError> {
    let stored = records
        .iter()
        .rev()
        .find_map(|record| record.session_started())
        .map_or_else(
            || Model::Codex(CodexModel::Sol).to_string(),
            |started| started.model,
        );
    model::parse(&stored).map_err(|_| SessionError::UnsupportedModel { model: stored })
}

pub(crate) fn next_sequence(records: &[Arc<TranscriptRecord>]) -> u64 {
    let current_session_id = records
        .iter()
        .rev()
        .find_map(|record| record.session_started().map(|started| started.session_id));
    let Some(current_session_id) = current_session_id else {
        return 1;
    };
    let mut segment = None::<String>;
    let mut maximum = 0;
    for record in records {
        if record.local_kind() == Some(LocalKind::SessionStarted) {
            segment = record.session_started().map(|started| started.session_id);
        }
        if segment.as_deref() == Some(&current_session_id) {
            maximum = maximum.max(record.sequence());
        }
    }
    maximum.saturating_add(1).max(1)
}

#[cfg(test)]
mod tests {
    use super::{
        HistoryPage, RecentPrompt, RecentPromptScope, RecentPrompts, SessionError, SessionSummary,
        TerminalStopReason, encode_checkpoint, load_checkpoint, load_transcript, model,
        save_checkpoint,
    };
    use crate::{
        app::config::{ReasoningEffort, ReasoningMode, Speed},
        core::{
            storage::{BUSY_TIMEOUT, SessionStorage, database_path},
            transcript::{
                LocalEvent, LocalKind, SessionStarted, TranscriptJournal, TranscriptRecord, TurnId,
            },
        },
    };
    use nanocodex::{
        ClaudeModel, HarnessModel as Model, Model as CodexModel, agent::session::SessionSnapshot,
    };
    use rusqlite::Connection;
    use serde_json::{Value, json};
    use std::{path::PathBuf, sync::Arc};
    use tempfile::tempdir;

    fn summary(index: usize, preview: &str) -> SessionSummary {
        SessionSummary {
            session_id: format!("session-{index}"),
            started_at_unix_ms: 0,
            model: "gpt-6.1-sol".to_owned(),
            effort: ReasoningEffort::Medium,
            reasoning_mode: ReasoningMode::Standard,
            workspace: PathBuf::from("/work"),
            preview: preview.to_owned(),
        }
    }

    #[test]
    fn history_pages_filter_like_the_resume_picker() {
        let sessions = (0..120)
            .map(|index| summary(index, if index % 2 == 0 { "Fix Parser" } else { "docs" }))
            .collect::<Vec<_>>();

        let first = HistoryPage::new(sessions.clone(), "PARSER", None).unwrap();
        assert_eq!(first.sessions.len(), 50);
        assert_eq!(first.sessions[1].session_id, "session-2");
        let cursor = first.next_cursor.unwrap();
        let second = HistoryPage::new(sessions.clone(), "parser", Some(&cursor)).unwrap();
        assert_eq!(second.sessions.len(), 10);
        assert_eq!(second.sessions[0].session_id, "session-100");
        assert_eq!(second.next_cursor, None);

        let by_id = HistoryPage::new(sessions.clone(), "session-7", None).unwrap();
        assert_eq!(by_id.sessions.len(), 11, "session-7 and session-70..79");
        assert!(HistoryPage::new(sessions, "", Some("not a cursor")).is_err());
    }

    #[test]
    fn recent_prompts_rank_within_the_requested_scope() {
        let prompt = |text: &str, session_id: &str| RecentPrompt {
            text: text.to_owned(),
            recorded_at_unix_ms: 0,
            session_id: session_id.to_owned(),
            workspace: PathBuf::from("/work"),
        };
        let prompts = vec![
            prompt("deploy the review app", "other"),
            prompt("review the diff", "current"),
            prompt("write docs", "current"),
        ];
        let texts = |scope, query| {
            RecentPrompts::new(prompts.clone(), "current", scope, query)
                .prompts
                .into_iter()
                .map(|prompt| prompt.text)
                .collect::<Vec<_>>()
        };

        assert_eq!(
            texts(RecentPromptScope::Global, "review"),
            ["review the diff", "deploy the review app"]
        );
        assert_eq!(
            texts(RecentPromptScope::CurrentSession, ""),
            ["review the diff", "write docs"]
        );
    }

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
                    model: Model::Codex(CodexModel::Luna).to_string(),
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

    #[tokio::test]
    async fn history_lists_the_repository_family_and_excludes_unrelated_sessions() {
        use std::process::Command;
        let directory = tempdir().unwrap();
        let repository = directory.path().join("repository");
        let checkout = directory.path().join("checkout");
        std::fs::create_dir(&repository).unwrap();
        let run = |args: &[&str]| {
            let output = Command::new("git")
                .args([
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "core.hooksPath=/dev/null",
                ])
                .current_dir(&repository)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        run(&["init", "--quiet"]);
        run(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.test",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "fixture",
        ]);
        run(&[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            checkout.to_str().unwrap(),
        ]);
        let repository = repository.canonicalize().unwrap();
        let checkout = checkout.canonicalize().unwrap();
        let config_path = directory.path().join("config.toml");
        let mut storage = SessionStorage::open(&config_path).unwrap();
        for (index, workspace) in [
            repository.clone(),
            checkout.clone(),
            directory.path().join("unrelated"),
        ]
        .into_iter()
        .enumerate()
        {
            let id = format!("session-{index}");
            let mut start = started(1, &id, None, None)
                .decode_payload::<SessionStarted>()
                .unwrap();
            start.workspace = workspace;
            let record = Arc::new(
                TranscriptRecord::from_local(
                    1,
                    index as u64 + 1,
                    LocalEvent::SessionStarted(start),
                )
                .unwrap(),
            );
            storage.append_records(&id, &[record]).unwrap();
        }
        storage
            .append_records("session-0", &[prompt(2, "recent activity")])
            .unwrap();
        drop(storage);
        let sessions = super::list_async(config_path.clone(), checkout, false)
            .await
            .unwrap();
        assert_eq!(
            sessions
                .iter()
                .map(|session| session.session_id.as_str())
                .collect::<Vec<_>>(),
            ["session-0", "session-1"]
        );
        assert_eq!(
            super::list_async(config_path, repository, false)
                .await
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn restored_model_comes_from_the_session_start_record() {
        for selected in [
            Model::Codex(CodexModel::Luna),
            Model::Claude(ClaudeModel::Sonnet55),
            Model::Claude(ClaudeModel::Opus55),
            Model::Claude(ClaudeModel::Fable51),
        ] {
            let record = TranscriptRecord::from_local(
                1,
                1,
                LocalEvent::SessionStarted(SessionStarted {
                    session_id: "session".to_owned(),
                    parent_session_id: None,
                    parent_sequence: None,
                    model: selected.to_string(),
                    effort: ReasoningEffort::Medium,
                    reasoning_mode: ReasoningMode::Standard,
                    speed: Speed::Standard,
                    workspace: "/work".into(),
                    application_version: "test".to_owned(),
                }),
            )
            .unwrap();

            assert_eq!(model(&[Arc::new(record)]).unwrap(), selected);
        }
        assert_eq!(model(&[]).unwrap(), Model::Codex(CodexModel::Sol));
    }

    #[test]
    fn retired_model_identity_is_not_remapped_on_resume() {
        for old_id in ["gpt-6-sol", "gpt-5.6-sol"] {
            let record = TranscriptRecord::from_local(
                1,
                1,
                LocalEvent::SessionStarted(SessionStarted {
                    session_id: "session".to_owned(),
                    parent_session_id: None,
                    parent_sequence: None,
                    model: old_id.to_owned(),
                    effort: ReasoningEffort::Medium,
                    reasoning_mode: ReasoningMode::Standard,
                    speed: Speed::Standard,
                    workspace: "/work".into(),
                    application_version: "test".to_owned(),
                }),
            )
            .unwrap();

            assert!(matches!(
                model(&[Arc::new(record)]),
                Err(SessionError::UnsupportedModel { model }) if model == old_id
            ));
        }
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
        assert_eq!(loaded[0].local_kind(), Some(LocalKind::SessionStarted));
        assert_eq!(loaded[1].local_kind(), Some(LocalKind::UserSubmitted));
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
            .filter(|record| record.local_kind() == Some(LocalKind::UserSubmitted))
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
                .filter(|record| record.local_kind() == Some(LocalKind::SessionStarted))
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
    fn claude_checkpoint_round_trips_native_fields_and_rejects_codex_restore() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let expected = super::AgentSnapshot::from_claude(nanocodex::agent::ChildSnapshot::Native {
            model: nanocodex::HarnessModel::Claude(nanocodex::ClaudeModel::Fable51),
            session_id: "native-session".to_owned(),
            thinking: nanocodex::Thinking::Max,
            payload: "{\"native\":\"opaque state\"}".to_owned(),
            has_conversation: true,
        })
        .unwrap();
        assert!(expected.clone().into_codex().is_err());
        assert!(
            expected
                .validate_identity(Model::Claude(ClaudeModel::Fable51), Some("native-session"))
                .is_ok()
        );
        assert!(
            expected
                .validate_identity(Model::Claude(ClaudeModel::Fable51), None)
                .is_ok()
        );
        assert!(
            expected
                .validate_identity(Model::Claude(ClaudeModel::Opus55), Some("native-session"))
                .is_err()
        );
        assert!(
            expected
                .validate_identity(Model::Claude(ClaudeModel::Fable51), Some("other-session"))
                .is_err()
        );
        assert!(
            expected
                .validate_identity(Model::Codex(CodexModel::Sol), Some("native-session"))
                .is_err()
        );
        let encoded = encode_checkpoint(&expected, "exact Claude instructions", true).unwrap();
        SessionStorage::open(&config)
            .unwrap()
            .save_resume_state("native-session", &encoded)
            .unwrap();
        let (actual, instructions, catalog) = load_checkpoint(&config, "native-session")
            .unwrap()
            .into_parts();
        assert_eq!(
            serde_json::to_value(&actual).unwrap(),
            serde_json::to_value(&expected).unwrap()
        );
        assert_eq!(instructions, "exact Claude instructions");
        assert_eq!(catalog, Some(true));
        let nanocodex::agent::ChildSnapshot::Native {
            model,
            session_id,
            thinking,
            payload,
            has_conversation,
        } = actual.into_claude().unwrap()
        else {
            panic!("expected native checkpoint")
        };
        assert_eq!(
            model,
            nanocodex::HarnessModel::Claude(nanocodex::ClaudeModel::Fable51)
        );
        assert_eq!(session_id, "native-session");
        assert_eq!(thinking, nanocodex::Thinking::Max);
        assert_eq!(payload, "{\"native\":\"opaque state\"}");
        assert!(has_conversation);
    }

    #[test]
    fn legacy_v2_checkpoint_loads_and_rejects_claude_restore() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let expected = snapshot("legacy");
        let encoded = serde_json::to_vec(&serde_json::json!({
            "format_version": 2,
            "snapshot": expected,
            "instructions": "legacy instructions",
            "skills_catalog_present": false,
        }))
        .unwrap();
        SessionStorage::open(&config)
            .unwrap()
            .save_resume_state("legacy", &encoded)
            .unwrap();
        let (actual, instructions, _) = load_checkpoint(&config, "legacy").unwrap().into_parts();
        assert!(actual.clone().into_claude().is_err());
        assert_eq!(
            serde_json::to_value(actual.into_codex().unwrap()).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
        assert_eq!(instructions, "legacy instructions");
    }

    #[test]
    fn native_checkpoint_rejects_a_codex_model() {
        let snapshot: super::AgentSnapshot = serde_json::from_value(serde_json::json!({
            "model": "sol", "session_id": "native-session", "thinking": "low",
            "payload": "opaque", "has_conversation": true,
        }))
        .unwrap();
        assert!(snapshot.into_claude().is_err());
    }

    #[test]
    fn native_context_budget_requires_supported_complete_metadata() {
        let payload = serde_json::json!({
            "version": 1, "context_window_tokens": 1_000_000,
            "snapshot": {"version": 1, "provider": "claude",
                "conversation": {"active_context_tokens": 42}}
        });
        let project = |payload: serde_json::Value| {
            super::AgentSnapshot::from_claude(nanocodex::agent::ChildSnapshot::Native {
                model: nanocodex::HarnessModel::Claude(nanocodex::ClaudeModel::Opus55),
                session_id: "context-fixture".into(),
                thinking: nanocodex::Thinking::Medium,
                payload: payload.to_string(),
                has_conversation: true,
            })
            .unwrap()
            .context_budget()
        };
        assert_eq!(
            project(payload.clone()),
            Some(super::ContextBudget {
                active_tokens: 42,
                window_tokens: 1_000_000,
            })
        );
        for path in ["/version", "/snapshot/version"] {
            let mut invalid = payload.clone();
            *invalid.pointer_mut(path).unwrap() = serde_json::json!(2);
            assert_eq!(project(invalid), None);
        }
        for (path, value) in [
            ("/context_window_tokens", serde_json::json!(0)),
            ("/snapshot/provider", serde_json::json!("other")),
            (
                "/snapshot/conversation/active_context_tokens",
                serde_json::Value::Null,
            ),
        ] {
            let mut invalid = payload.clone();
            *invalid.pointer_mut(path).unwrap() = value;
            assert_eq!(project(invalid), None);
        }
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
            parent_sequence: None,
            model: nanocodex::oai::MODEL.to_owned(),
            effort: ReasoningEffort::Medium,
            reasoning_mode: ReasoningMode::Standard,
            speed: Speed::Standard,
            workspace: "/work".into(),
            application_version: "test".to_owned(),
        });
        journal
            .append_local(LocalEvent::WorkerTurnFinished {
                id: TurnId::new(2),
                error: Some("API failed".to_owned()),
                terminal_stop: None,
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
        assert_eq!(
            records.last().unwrap().local_kind(),
            Some(LocalKind::WorkerTurnFinished)
        );
    }

    #[test]
    fn terminal_provider_stop_blocks_the_last_successful_checkpoint() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let expected = snapshot("successful");
        save_checkpoint(&config, "session", &expected, "instructions", true).unwrap();

        let terminal_stop = serde_json::from_str::<TranscriptRecord>(
            &json!({
                "schema_version": 2,
                "sequence": 2,
                "recorded_at_unix_ms": 2,
                "source": "tact",
                "type": "worker.turn_finished",
                "payload": {
                    "id": 2,
                    "error": "provider stopped conversation",
                    "terminal_stop": "misalignment_policy_violation"
                }
            })
            .to_string(),
        )
        .unwrap();
        let later_failure = TranscriptRecord::from_local(
            3,
            3,
            LocalEvent::WorkerTurnFinished {
                id: TurnId::new(3),
                error: Some("agent stopped".to_owned()),
                terminal_stop: None,
            },
        )
        .unwrap();
        SessionStorage::open(&config)
            .unwrap()
            .append_records(
                "session",
                &[
                    started(1, "session", None, None),
                    Arc::new(terminal_stop),
                    Arc::new(later_failure),
                ],
            )
            .unwrap();

        assert!(
            load_checkpoint(&config, "session").is_err(),
            "a terminal provider stop must block the retained checkpoint even after a later failure"
        );
    }

    #[tokio::test]
    async fn terminal_stop_round_trips_through_the_journal_without_a_checkpoint() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let (mut journal, writer) = TranscriptJournal::open(&config, "session").unwrap();
        let start = started(1, "session", None, None);
        journal.defer_start(start.decode_payload::<SessionStarted>().unwrap());
        let reason = TerminalStopReason::MisalignmentPolicyViolation;
        journal
            .append_local(LocalEvent::WorkerTurnFinished {
                id: TurnId::new(2),
                error: Some("provider stopped conversation".to_owned()),
                terminal_stop: Some(reason),
            })
            .unwrap();
        journal.flush().await.unwrap();

        assert!(matches!(load_checkpoint(&config, "session"),
            Err(SessionError::TerminalStop { reason: actual, .. }) if actual == reason));
        let records = load_transcript(&config, "session").unwrap();
        let payload = records.last().unwrap().decode_payload::<Value>().unwrap();
        assert_eq!(payload["terminal_stop"], "misalignment_policy_violation");
        assert_eq!(
            serde_json::from_value::<TerminalStopReason>(payload["terminal_stop"].clone()).unwrap(),
            reason
        );
        drop(journal);
        writer.into_task().await.unwrap().unwrap();
    }

    #[test]
    fn unknown_terminal_stop_classification_fails_closed() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        save_checkpoint(
            &config,
            "session",
            &snapshot("successful"),
            "instructions",
            true,
        )
        .unwrap();
        let record = TranscriptRecord::from_local(
            2,
            2,
            LocalEvent::WorkerTurnFinished {
                id: TurnId::new(2),
                error: Some("provider stopped conversation".to_owned()),
                terminal_stop: Some(TerminalStopReason::MisalignmentPolicyViolation),
            },
        )
        .unwrap();
        let mut encoded = serde_json::to_value(record).unwrap();
        encoded["payload"]["terminal_stop"] = json!("future_provider_stop");
        let record = serde_json::from_str(&encoded.to_string()).unwrap();
        let mut storage = SessionStorage::open(&config).unwrap();
        let expected = storage.load_resume_state("session").unwrap();
        storage
            .append_records(
                "session",
                &[started(1, "session", None, None), Arc::new(record)],
            )
            .unwrap();
        drop(storage);

        assert!(matches!(
            load_checkpoint(&config, "session"),
            Err(SessionError::Storage(_))
        ));
        assert_eq!(
            SessionStorage::open_read_only(&config)
                .unwrap()
                .unwrap()
                .load_resume_state("session")
                .unwrap(),
            expected
        );
    }

    #[tokio::test]
    async fn successful_turn_publishes_its_tail_and_snapshot_together() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let expected = snapshot("successful");
        let resume_state = encode_checkpoint(
            &super::AgentSnapshot::Codex(Box::new(expected.clone())),
            "instructions",
            true,
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
            speed: Speed::Standard,
            workspace: "/work".into(),
            application_version: "test".to_owned(),
        });
        journal
            .append_local_with_resume_state(
                LocalEvent::WorkerTurnFinished {
                    id: TurnId::new(1),
                    error: None,
                    terminal_stop: None,
                },
                resume_state,
            )
            .unwrap();
        drop(journal);
        writer.into_task().await.unwrap().unwrap();

        let records = super::load_transcript(&config, "session").unwrap();
        assert_eq!(
            records.last().unwrap().local_kind(),
            Some(LocalKind::WorkerTurnFinished)
        );
        let (actual, instructions, catalog) =
            load_checkpoint(&config, "session").unwrap().into_parts();
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
        assert_eq!(instructions, "instructions");
        assert_eq!(catalog, Some(true));
    }

    #[tokio::test]
    async fn manual_compaction_publishes_resume_state_and_failure_retains_it() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let instructions = "exact instructions\r\n  including whitespace\n";
        save_checkpoint(&config, "session", &snapshot("before"), instructions, true).unwrap();
        SessionStorage::open(&config)
            .unwrap()
            .append_records("session", &[started(1, "session", None, None)])
            .unwrap();
        let (mut journal, writer) = TranscriptJournal::open_at(&config, "session", 2).unwrap();
        journal.append_local(LocalEvent::CompactionStarted).unwrap();
        let expected = super::AgentSnapshot::Codex(Box::new(snapshot("compacted")));
        journal
            .append_local_with_resume_state(
                LocalEvent::CompactionFinished {
                    terminal_stop: None,
                    error: None,
                    duration_ns: 1,
                },
                encode_checkpoint(&expected, instructions, true).unwrap(),
            )
            .unwrap();
        journal.flush().await.unwrap();
        let (actual, actual_instructions, catalog) =
            load_checkpoint(&config, "session").unwrap().into_parts();
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            serde_json::to_value(&expected).unwrap()
        );
        assert_eq!(actual_instructions, instructions);
        assert_eq!(catalog, Some(true));
        let records = load_transcript(&config, "session").unwrap();
        assert_eq!(
            records.last().unwrap().local_kind(),
            Some(LocalKind::CompactionFinished)
        );
        assert!(records.last().unwrap().decode_payload::<Value>().unwrap()["error"].is_null());
        journal.append_local(LocalEvent::CompactionStarted).unwrap();
        journal
            .append_local(LocalEvent::CompactionFinished {
                terminal_stop: None,
                error: Some("synthetic failure".to_owned()),
                duration_ns: 1,
            })
            .unwrap();
        journal.flush().await.unwrap();
        let (actual, actual_instructions, _) =
            load_checkpoint(&config, "session").unwrap().into_parts();
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
        assert_eq!(actual_instructions, instructions);
        drop(journal);
        writer.into_task().await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn transient_write_lock_preserves_transcript_for_resume() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let initial_records = [started(1, "session", None, None)];
        SessionStorage::open(&config)
            .unwrap()
            .append_records("session", &initial_records)
            .unwrap();
        let expected = snapshot("before-lock");
        save_checkpoint(&config, "session", &expected, "instructions", true).unwrap();

        let lock = Connection::open(database_path(&config)).unwrap();
        lock.execute_batch("BEGIN IMMEDIATE").unwrap();
        let (mut journal, writer) = TranscriptJournal::open_at(&config, "session", 2).unwrap();
        journal
            .append_local(LocalEvent::UserSubmitted {
                id: TurnId::new(1),
                text: "first tail".to_owned(),
            })
            .unwrap();

        // Outlast the writer's busy timeout so it observes contention and retries.
        tokio::time::sleep(BUSY_TIMEOUT * 2).await;
        journal
            .append_local(LocalEvent::UserSubmitted {
                id: TurnId::new(2),
                text: "second tail".to_owned(),
            })
            .unwrap();

        lock.execute_batch("COMMIT").unwrap();
        drop(journal);
        writer.into_task().await.unwrap().unwrap();

        let (restored, _, _) = load_checkpoint(&config, "session").unwrap().into_parts();
        assert_eq!(
            serde_json::to_value(restored).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
        let records = load_transcript(&config, "session").unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(records[1].local_kind(), Some(LocalKind::UserSubmitted));
        assert_eq!(records[2].local_kind(), Some(LocalKind::UserSubmitted));
    }
}
