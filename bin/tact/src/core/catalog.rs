//! Session records, lineage, and resume checkpoints in Nanocodex's durable session catalog.
//!
//! Each resumable tact session is a catalog session whose record carries its model, workspace,
//! title, and fork lineage. Every persisted resume point is one completed catalog turn: its
//! checkpoint is the portable [`SessionCheckpoint`] and its input carries the prompt that led to
//! it plus the system prompt the session resumes with. The catalog is a JSONL store beside the
//! transcript database. Catalog work runs on its own short-lived runtime, so callers on any
//! thread, inside or outside Tokio, can persist and load synchronously.

use super::{
    session::{AgentSnapshot, ResumeState},
    storage::{SessionStorage, StorageError, StoredSession, database_path},
};
use nanocode::{
    JsonlStore, Origin, SessionCheckpoint,
    durability::{self, SessionRecord, SessionStore, TurnStatus},
    session_store,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs,
    future::Future,
    path::{Path, PathBuf},
    thread,
};
use thiserror::Error;

/// Marks a completed one-time import of the transcript database's resume states.
const LEGACY_IMPORT_MARKER: &str = "tact-legacy-import-v1";

#[derive(Debug, Error)]
pub(crate) enum CatalogError {
    #[error("failed to open the session catalog at {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: durability::Error,
    },
    #[error("session catalog operation failed: {0}")]
    Store(#[from] durability::Error),
    #[error("session catalog runtime failed: {0}")]
    Runtime(#[source] std::io::Error),
    #[error("session catalog worker panicked")]
    WorkerPanicked,
    #[error("session {session_id} has an unusable catalog checkpoint: {reason}")]
    InvalidCheckpoint { session_id: String, reason: String },
    #[error("session {session_id} has no catalog record and no recorded start")]
    MissingRecord { session_id: String },
    #[error(transparent)]
    Legacy(#[from] StorageError),
    #[error("failed to record the legacy import in {path}: {source}")]
    Marker {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// The tact-owned part of a catalog turn's input.
#[derive(Deserialize, Serialize)]
struct TactResume {
    instructions: String,
    skills_catalog_present: bool,
}

#[derive(Deserialize, Serialize)]
struct TurnInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prompt: Option<String>,
    tact: TactResume,
}

/// How a session enters the catalog the first time it persists a resume point.
pub(crate) struct CatalogEntry<'a> {
    pub(crate) session_id: &'a str,
    pub(crate) parent_session_id: Option<&'a str>,
    pub(crate) origin: Origin,
    pub(crate) workspace: &'a Path,
    pub(crate) title: Option<&'a str>,
    pub(crate) created_at_ms: Option<u64>,
}

/// The catalog directory beside a configuration's transcript database.
pub(crate) fn directory(config_path: &Path) -> PathBuf {
    database_path(config_path).with_file_name("nanocode")
}

fn open(config_path: &Path) -> Result<SessionStore, CatalogError> {
    let path = directory(config_path);
    JsonlStore::open_home(&path)
        .map_err(durability::Error::from)
        .and_then(session_store)
        .map_err(|source| CatalogError::Open { path, source })
}

/// Runs catalog work on a dedicated current-thread runtime, so durable session drivers never
/// outlive the call and synchronous callers inside Tokio never nest runtimes.
fn run<T, F>(
    work: impl FnOnce(SessionStore) -> F + Send,
    config_path: &Path,
) -> Result<T, CatalogError>
where
    T: Send,
    F: Future<Output = Result<T, CatalogError>>,
{
    thread::scope(|scope| {
        scope
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(CatalogError::Runtime)?
                    .block_on(async { work(open(config_path)?).await })
            })
            .join()
            .map_err(|_| CatalogError::WorkerPanicked)?
    })
}

/// Records a resume point as a completed catalog turn named `operation_id`.
///
/// The first resume point creates the session's record. A fork derives its lineage from its
/// parent's record when the parent is cataloged.
pub(crate) fn commit(
    config_path: &Path,
    entry: Option<&CatalogEntry<'_>>,
    session_id: &str,
    operation_id: &str,
    prompt: Option<&str>,
    state: &ResumeState,
) -> Result<(), CatalogError> {
    let checkpoint = state.snapshot().clone().into_checkpoint();
    let input = TurnInput {
        prompt: prompt.map(str::to_owned),
        tact: TactResume {
            instructions: state.instructions().to_owned(),
            skills_catalog_present: state.skills_catalog_present(),
        },
    };
    run(
        |store| async move {
            let record = match store.summary(session_id).await? {
                Some(summary) => {
                    let mut record = summary.record;
                    record.model = checkpoint.model();
                    record
                }
                None => {
                    let entry = entry.ok_or_else(|| CatalogError::MissingRecord {
                        session_id: session_id.to_owned(),
                    })?;
                    new_record(&store, entry, &checkpoint).await?
                }
            };
            let session = store.session(record).await?;
            if !matches!(
                session.admit(operation_id, &input).await?,
                durability::Admission::Accepted
            ) {
                return Ok(());
            }
            session.begin_attempt(operation_id).await?;
            session
                .complete(operation_id, &checkpoint, &serde_json::Value::Null)
                .await?;
            Ok(())
        },
        config_path,
    )
}

async fn new_record(
    store: &SessionStore,
    entry: &CatalogEntry<'_>,
    checkpoint: &SessionCheckpoint,
) -> Result<SessionRecord, CatalogError> {
    let parent = match entry.parent_session_id {
        Some(parent) => store.summary(parent).await?,
        None => None,
    };
    let mut record = match parent {
        Some(parent) => {
            let mut record = parent.record.derive(entry.session_id, entry.origin);
            record.model = checkpoint.model();
            record
        }
        None => SessionRecord::root(
            entry.session_id,
            checkpoint.model(),
            Some(entry.workspace.to_path_buf()),
        ),
    };
    record.workspace = Some(entry.workspace.to_path_buf());
    record.title = entry.title.map(str::to_owned);
    if let Some(created_at_ms) = entry.created_at_ms {
        record.created_at_ms = created_at_ms;
    }
    Ok(record)
}

/// The latest resume point of a cataloged session.
pub(crate) fn load(
    config_path: &Path,
    session_id: &str,
) -> Result<Option<ResumeState>, CatalogError> {
    run(
        |store| async move {
            if store.summary(session_id).await?.is_none() {
                return Ok(None);
            }
            let stored = store.load(session_id).await?;
            let invalid = |reason: String| CatalogError::InvalidCheckpoint {
                session_id: session_id.to_owned(),
                reason,
            };
            let Some(checkpoint) = stored.session_checkpoint()? else {
                return Ok(None);
            };
            let resume = stored
                .turns
                .iter()
                .rev()
                .filter(|turn| turn.status == TurnStatus::Completed)
                .find_map(|turn| serde_json::from_value::<TurnInput>(turn.input.clone()).ok())
                .ok_or_else(|| invalid("no completed tact turn carries resume metadata".into()))?;
            Ok(Some(ResumeState::new(
                AgentSnapshot::from(checkpoint),
                resume.tact.instructions,
                resume.tact.skills_catalog_present,
            )))
        },
        config_path,
    )
}

/// The cataloged session identities.
pub(crate) fn session_ids(config_path: &Path) -> Result<HashSet<String>, CatalogError> {
    run(
        |store| async move {
            Ok(store
                .list()
                .await?
                .into_iter()
                .map(|summary| summary.record.session_id)
                .collect())
        },
        config_path,
    )
}

/// Imports one legacy resume state that the transcript database still holds.
pub(crate) fn import_legacy_session(
    config_path: &Path,
    session: &StoredSession,
    state: &ResumeState,
) -> Result<(), CatalogError> {
    let entry = CatalogEntry {
        session_id: &session.session_id,
        parent_session_id: session.parent_session_id.as_deref(),
        origin: Origin::Fork,
        workspace: &session.workspace,
        title: Some(session.preview.as_str()).filter(|preview| !preview.trim().is_empty()),
        created_at_ms: Some(session.started_at_unix_ms),
    };
    commit(
        config_path,
        Some(&entry),
        &session.session_id,
        "tact-legacy-import",
        None,
        state,
    )
}

/// Imports every legacy resume state once, parents before forks, leaving the database unchanged.
///
/// Returns how many sessions were imported. `decode` turns a stored resume state into the
/// current form; sessions it rejects are skipped and stay readable from the database.
pub(crate) fn import_legacy(
    config_path: &Path,
    decode: impl Fn(&str, &[u8]) -> Option<ResumeState>,
) -> Result<usize, CatalogError> {
    let marker = directory(config_path).join(LEGACY_IMPORT_MARKER);
    if marker.exists() {
        return Ok(0);
    }
    let Some(storage) = SessionStorage::open_read_only(config_path)? else {
        return Ok(0);
    };
    let cataloged = session_ids(config_path)?;
    let mut sessions = storage
        .sessions_with_resume_state()?
        .into_iter()
        .filter(|session| !cataloged.contains(&session.session_id))
        .collect::<Vec<_>>();
    sessions.sort_by_key(|session| session.started_at_unix_ms);
    let mut imported = 0;
    for session in sessions {
        let Some(encoded) = storage.load_resume_state(&session.session_id)? else {
            continue;
        };
        let Some(state) = decode(&session.session_id, &encoded) else {
            continue;
        };
        import_legacy_session(config_path, &session, &state)?;
        imported += 1;
    }
    fs::write(&marker, b"").map_err(|source| CatalogError::Marker {
        path: marker,
        source,
    })?;
    Ok(imported)
}
