//! Resumable Nanocodex checkpoints and transcript-derived session discovery.

use crate::{
    app::config::{ReasoningEffort, ReasoningMode},
    tui::{
        context::{ApiEventProjection, api_event_projection, outbound_context_snapshot},
        transcript::{self, LocalEvent, SessionStarted, TranscriptRecord},
    },
};
use nanocodex::agent::session::SessionSnapshot;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{self, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};
use tempfile::NamedTempFile;
use thiserror::Error;
use tokio::sync::oneshot;
use zstd::stream::{read::Decoder, write::Encoder};

const COMPRESSION_LEVEL: i32 = 3;
const CHECKPOINT_FORMAT_VERSION: u32 = 1;
const SEGMENT_SUMMARY_FORMAT_VERSION: u32 = 1;
const PROJECTION_FORMAT_VERSION: u32 = 1;

#[cfg(unix)]
use std::os::unix::fs::DirBuilderExt;

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

#[derive(Deserialize, Serialize)]
struct StoredSegmentSummary {
    format_version: u32,
    transcript_filename: String,
    transcript_bytes: u64,
    transcript_modified_unix_ns: u128,
    summary: SessionSummary,
}

#[derive(Deserialize, Eq, PartialEq, Serialize)]
struct TranscriptFingerprint {
    filename: String,
    bytes: u64,
    modified_unix_ns: u128,
}

#[derive(Deserialize, Serialize)]
struct StoredTranscriptProjection {
    format_version: u32,
    session_id: String,
    transcript_fingerprint: Vec<TranscriptFingerprint>,
    records: Vec<Arc<TranscriptRecord>>,
}

#[derive(Serialize)]
struct TranscriptProjectionEnvelope<'a> {
    format_version: u32,
    session_id: &'a str,
    transcript_fingerprint: &'a [TranscriptFingerprint],
    records: &'a [Arc<TranscriptRecord>],
}

struct LoadedSessionSegment {
    records: Vec<Arc<TranscriptRecord>>,
    complete: bool,
    observed_records: usize,
}

#[derive(Serialize)]
struct CheckpointEnvelope<'a> {
    format_version: u32,
    instructions: &'a str,
    snapshot: &'a SessionSnapshot,
}

#[derive(Deserialize)]
struct StoredCheckpoint {
    #[serde(default)]
    format_version: Option<u32>,
    #[serde(default)]
    instructions: Option<String>,
    #[serde(default)]
    snapshot: Option<SessionSnapshot>,
}

pub(crate) struct ResumeState {
    snapshot: SessionSnapshot,
    instructions: String,
}

impl ResumeState {
    fn new(snapshot: SessionSnapshot, instructions: String) -> Self {
        Self {
            snapshot,
            instructions,
        }
    }

    pub(crate) fn into_parts(self) -> (SessionSnapshot, String) {
        (self.snapshot, self.instructions)
    }
}

#[derive(Debug, Error)]
pub(crate) enum SessionError {
    #[error("failed to create session directory {path}: {source}")]
    CreateDirectory {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to inspect session directory {path}: {source}")]
    ReadDirectory {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to create checkpoint in {path}: {source}")]
    CreateCheckpoint {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to encode checkpoint {path}: {source}")]
    EncodeCheckpoint {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to write checkpoint {path}: {source}")]
    WriteCheckpoint {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to replace checkpoint {path}: {source}")]
    PersistCheckpoint {
        path: PathBuf,
        #[source]
        source: tempfile::PersistError,
    },
    #[error("failed to remove obsolete checkpoint {path}: {source}")]
    RemoveObsoleteCheckpoint {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("no resumable checkpoint exists for session {session_id}")]
    MissingCheckpoint { session_id: String },
    #[error("obsolete checkpoint {path} was removed; start a new session")]
    ObsoleteCheckpointRemoved { path: PathBuf },
    #[error("failed to read checkpoint {path}: {source}")]
    ReadCheckpoint {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to decode checkpoint {path}: {source}")]
    DecodeCheckpoint {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("checkpoint {path} was created by an incompatible tact version; start a new session")]
    IncompatibleCheckpoint { path: PathBuf },
    #[error("checkpoint {path} is incomplete or corrupt; start a new session")]
    InvalidCheckpoint { path: PathBuf },
    #[error(transparent)]
    Transcript(#[from] transcript::TranscriptError),
    #[error("the transcript loading worker stopped unexpectedly")]
    TranscriptLoaderStopped,
}

pub(crate) fn save_checkpoint(
    config_path: &Path,
    session_id: &str,
    snapshot: &SessionSnapshot,
    instructions: &str,
) -> Result<(), SessionError> {
    let directory = checkpoint_directory(config_path);
    create_private_directory(&directory)?;
    let path = checkpoint_path(config_path, session_id);
    let temporary =
        NamedTempFile::new_in(&directory).map_err(|source| SessionError::CreateCheckpoint {
            path: directory.clone(),
            source,
        })?;
    let file = temporary
        .reopen()
        .map_err(|source| SessionError::WriteCheckpoint {
            path: path.clone(),
            source,
        })?;
    let output = BufWriter::new(file);
    let mut output = Encoder::new(output, COMPRESSION_LEVEL).map_err(|source| {
        SessionError::WriteCheckpoint {
            path: path.clone(),
            source,
        }
    })?;
    let checkpoint = CheckpointEnvelope {
        format_version: CHECKPOINT_FORMAT_VERSION,
        instructions,
        snapshot,
    };
    serde_json::to_writer(&mut output, &checkpoint).map_err(|source| {
        SessionError::EncodeCheckpoint {
            path: path.clone(),
            source,
        }
    })?;
    let mut output = output
        .finish()
        .map_err(|source| SessionError::WriteCheckpoint {
            path: path.clone(),
            source,
        })?;
    output
        .flush()
        .map_err(|source| SessionError::WriteCheckpoint {
            path: path.clone(),
            source,
        })?;
    output
        .get_ref()
        .sync_all()
        .map_err(|source| SessionError::WriteCheckpoint {
            path: path.clone(),
            source,
        })?;
    drop(output);
    temporary
        .persist(&path)
        .map_err(|source| SessionError::PersistCheckpoint { path, source })?;
    remove_obsolete_checkpoint(config_path, session_id)?;
    Ok(())
}

pub(crate) fn load_checkpoint(
    config_path: &Path,
    session_id: &str,
) -> Result<ResumeState, SessionError> {
    let path = checkpoint_path(config_path, session_id);
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            let obsolete_path = obsolete_checkpoint_path(config_path, session_id);
            if remove_obsolete_checkpoint(config_path, session_id)? {
                return Err(SessionError::ObsoleteCheckpointRemoved {
                    path: obsolete_path,
                });
            }
            return Err(SessionError::MissingCheckpoint {
                session_id: session_id.to_owned(),
            });
        }
        Err(source) => {
            return Err(SessionError::ReadCheckpoint {
                path: path.clone(),
                source,
            });
        }
    };
    let decoder = Decoder::new(file).map_err(|source| SessionError::ReadCheckpoint {
        path: path.clone(),
        source,
    })?;
    let checkpoint = serde_json::from_reader::<_, StoredCheckpoint>(BufReader::new(decoder))
        .map_err(|source| SessionError::DecodeCheckpoint {
            path: path.clone(),
            source,
        })?;
    if checkpoint.format_version != Some(CHECKPOINT_FORMAT_VERSION) {
        return Err(SessionError::IncompatibleCheckpoint { path });
    }
    let (Some(snapshot), Some(instructions)) = (checkpoint.snapshot, checkpoint.instructions)
    else {
        return Err(SessionError::InvalidCheckpoint { path });
    };
    Ok(ResumeState::new(snapshot, instructions))
}

#[allow(
    dead_code,
    reason = "used by compatibility tests and catalog benchmarks"
)]
pub(crate) fn list(
    config_path: &Path,
    workspace: &Path,
) -> Result<Vec<SessionSummary>, SessionError> {
    remove_obsolete_checkpoints(config_path)?;
    let segments = transcript_paths(config_path)?
        .into_iter()
        .map(|path| load_catalog_segment(&path, workspace));
    collect_catalog(config_path, workspace, segments)
}

pub(crate) async fn list_async(
    config_path: PathBuf,
    workspace: PathBuf,
) -> Result<Vec<SessionSummary>, SessionError> {
    let (sender, receiver) = oneshot::channel();
    transcript_loader().spawn(move || {
        drop(sender.send(list_parallel_inner(&config_path, &workspace)));
    });
    receiver
        .await
        .map_err(|_| SessionError::TranscriptLoaderStopped)?
}

#[allow(dead_code, reason = "used by session catalog benchmarks")]
pub(crate) fn list_parallel(
    config_path: &Path,
    workspace: &Path,
) -> Result<Vec<SessionSummary>, SessionError> {
    transcript_loader().install(|| list_parallel_inner(config_path, workspace))
}

fn list_parallel_inner(
    config_path: &Path,
    workspace: &Path,
) -> Result<Vec<SessionSummary>, SessionError> {
    remove_obsolete_checkpoints(config_path)?;
    let segments = transcript_paths(config_path)?
        .into_par_iter()
        .map(|path| load_catalog_segment(&path, workspace))
        .collect::<Vec<_>>();
    collect_catalog(config_path, workspace, segments)
}

fn load_catalog_segment(
    path: &Path,
    workspace: &Path,
) -> Result<Option<SessionSummary>, SessionError> {
    if let Some(summary) = read_segment_summary(path) {
        return Ok((summary.workspace == workspace).then_some(summary));
    }
    let segment = transcript::load_matching_segment_filtered(
        path,
        |first| session_started_record(first).is_none_or(|started| started.workspace == workspace),
        |record| {
            record.source() == "tact"
                && matches!(
                    record.kind(),
                    "session.started" | "user.submitted" | "effort.changed"
                )
        },
    )?;
    let Some(segment) = segment else {
        return Ok(None);
    };
    let summary = summarize_segment(&segment.records);
    if segment.complete
        && let Some(summary) = &summary
    {
        write_segment_summary(path, summary);
    }
    Ok(summary.filter(|summary| summary.workspace == workspace))
}

fn collect_catalog(
    config_path: &Path,
    workspace: &Path,
    segments: impl IntoIterator<Item = Result<Option<SessionSummary>, SessionError>>,
) -> Result<Vec<SessionSummary>, SessionError> {
    let mut sessions = HashMap::<String, SessionSummary>::new();
    for segment in segments {
        let Some(summary) = segment? else {
            continue;
        };
        if summary.workspace != workspace {
            continue;
        }
        if !has_checkpoint(config_path, &summary.session_id)? {
            continue;
        }
        sessions
            .entry(summary.session_id.clone())
            .and_modify(|existing| {
                if summary.started_at_unix_ms > existing.started_at_unix_ms {
                    existing.started_at_unix_ms = summary.started_at_unix_ms;
                    existing.effort = summary.effort;
                    existing.reasoning_mode = summary.reasoning_mode;
                }
                if existing.preview == "No user prompt" {
                    existing.preview.clone_from(&summary.preview);
                }
            })
            .or_insert(summary);
    }
    let mut sessions = sessions.into_values().collect::<Vec<_>>();
    sessions.sort_unstable_by_key(|session| std::cmp::Reverse(session.started_at_unix_ms));
    Ok(sessions)
}

#[allow(
    dead_code,
    reason = "used by compatibility tests and restoration benchmarks"
)]
pub(crate) fn load_transcript(
    config_path: &Path,
    session_id: &str,
) -> Result<Vec<Arc<TranscriptRecord>>, SessionError> {
    if let Some(records) = read_transcript_projection(config_path, session_id) {
        return Ok(records);
    }
    let source_fingerprint = transcript_fingerprint(config_path)?;
    let mut records = Vec::new();
    let mut complete = true;
    let mut observed_records = 0;
    for path in transcript_paths(config_path)? {
        let Some(segment) = load_session_segment(&path, session_id)? else {
            continue;
        };
        complete &= segment.complete;
        observed_records += segment.observed_records;
        records.extend(segment.records);
    }
    if complete && observed_records > records.len() {
        records = compact_projection_records(records);
        if projection_cache_worthwhile(observed_records, records.len()) {
            write_transcript_projection(config_path, session_id, &source_fingerprint, &records);
        }
    }
    Ok(records)
}

pub(crate) async fn load_transcript_async(
    config_path: PathBuf,
    session_id: String,
) -> Result<Vec<Arc<TranscriptRecord>>, SessionError> {
    let (sender, receiver) = oneshot::channel();
    transcript_loader().spawn(move || {
        drop(sender.send(load_transcript_parallel_inner(&config_path, &session_id)));
    });
    receiver
        .await
        .map_err(|_| SessionError::TranscriptLoaderStopped)?
}

#[allow(dead_code, reason = "used by restoration benchmarks")]
pub(crate) fn load_transcript_parallel(
    config_path: &Path,
    session_id: &str,
) -> Result<Vec<Arc<TranscriptRecord>>, SessionError> {
    transcript_loader().install(|| load_transcript_parallel_inner(config_path, session_id))
}

fn load_transcript_parallel_inner(
    config_path: &Path,
    session_id: &str,
) -> Result<Vec<Arc<TranscriptRecord>>, SessionError> {
    if let Some(records) = read_transcript_projection(config_path, session_id) {
        return Ok(records);
    }
    let source_fingerprint = transcript_fingerprint(config_path)?;
    let segments = transcript_paths(config_path)?
        .into_par_iter()
        .map(|path| load_session_segment(&path, session_id))
        .collect::<Vec<_>>()
        .into_iter()
        .collect::<Result<Vec<_>, SessionError>>()?;
    let mut records = Vec::new();
    let mut complete = true;
    let mut observed_records = 0;
    for segment in segments.into_iter().flatten() {
        complete &= segment.complete;
        observed_records += segment.observed_records;
        records.extend(segment.records);
    }
    if complete && observed_records > records.len() {
        records = compact_projection_records(records);
        if projection_cache_worthwhile(observed_records, records.len()) {
            write_transcript_projection(config_path, session_id, &source_fingerprint, &records);
        }
    }
    Ok(records)
}

fn load_session_segment(
    path: &Path,
    session_id: &str,
) -> Result<Option<LoadedSessionSegment>, SessionError> {
    if let Some(summary) = read_segment_summary(path)
        && summary.session_id != session_id
    {
        return Ok(None);
    }
    let segment = transcript::load_matching_segment_filtered(
        path,
        |first| {
            session_started_record(first).is_none_or(|started| started.session_id == session_id)
        },
        |record| api_event_projection(record) != ApiEventProjection::Discard,
    )?;
    let Some(segment) = segment else {
        return Ok(None);
    };
    let matches =
        session_started(&segment.records).is_some_and(|started| started.session_id == session_id);
    if segment.complete
        && let Some(summary) = summarize_segment(&segment.records)
    {
        write_segment_summary(path, &summary);
    }
    Ok(matches.then_some(LoadedSessionSegment {
        records: segment.records,
        complete: segment.complete,
        observed_records: segment.observed_records,
    }))
}

fn transcript_loader() -> &'static rayon::ThreadPool {
    static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        let available = std::thread::available_parallelism().map_or(1, usize::from);
        rayon::ThreadPoolBuilder::new()
            .num_threads(available.min(4))
            .thread_name(|index| format!("tact-transcript-loader-{index}"))
            .build()
            .expect("the transcript loading thread pool should initialize")
    })
}

pub(crate) fn format_age(started_at_unix_ms: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let elapsed = now.saturating_sub(u128::from(started_at_unix_ms));
    let minutes = elapsed / 60_000;
    match minutes {
        0 => "just now".to_owned(),
        1..=59 => format!("{minutes}m ago"),
        60..=1_439 => format!("{}h ago", minutes / 60),
        _ => format!("{}d ago", minutes / 1_440),
    }
}

fn transcript_paths(config_path: &Path) -> Result<Vec<PathBuf>, SessionError> {
    transcript::remove_obsolete(config_path)?;
    let directory = transcript::storage_directory(config_path);
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(SessionError::ReadDirectory {
                path: directory,
                source,
            });
        }
    };
    let mut paths = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|extension| extension == "zst"))
        .collect::<Vec<_>>();
    paths.sort_unstable();
    Ok(paths)
}

fn session_started(records: &[Arc<TranscriptRecord>]) -> Option<SessionStarted> {
    let record = records
        .iter()
        .find(|record| record.source() == "tact" && record.kind() == "session.started")?;
    session_started_record(record)
}

fn session_started_record(record: &TranscriptRecord) -> Option<SessionStarted> {
    (record.source() == "tact" && record.kind() == "session.started")
        .then(|| record.decode_payload().ok())?
}

pub(crate) fn reasoning_mode(records: &[Arc<TranscriptRecord>]) -> ReasoningMode {
    session_started(records).map_or(ReasoningMode::Standard, |started| started.reasoning_mode)
}

fn latest_effort(records: &[Arc<TranscriptRecord>], initial: ReasoningEffort) -> ReasoningEffort {
    #[derive(serde::Deserialize)]
    struct EffortChanged {
        to: ReasoningEffort,
    }

    records
        .iter()
        .filter(|record| record.source() == "tact" && record.kind() == "effort.changed")
        .filter_map(|record| record.decode_payload::<EffortChanged>().ok())
        .fold(initial, |_, change| change.to)
}

fn first_user_message(records: &[Arc<TranscriptRecord>]) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct UserSubmitted {
        text: String,
    }

    records
        .iter()
        .find(|record| record.source() == "tact" && record.kind() == "user.submitted")
        .and_then(|record| record.decode_payload::<UserSubmitted>().ok())
        .map(|payload| {
            payload
                .text
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|preview| !preview.is_empty())
}

fn summarize_segment(records: &[Arc<TranscriptRecord>]) -> Option<SessionSummary> {
    let started = session_started(records)?;
    Some(SessionSummary {
        session_id: started.session_id,
        started_at_unix_ms: records
            .first()
            .map_or(0, |record| record.recorded_at_unix_ms()),
        model: started.model,
        effort: latest_effort(records, started.effort),
        reasoning_mode: started.reasoning_mode,
        workspace: started.workspace,
        preview: first_user_message(records).unwrap_or_else(|| "No user prompt".to_owned()),
    })
}

fn read_segment_summary(transcript_path: &Path) -> Option<SessionSummary> {
    let metadata = fs::metadata(transcript_path).ok()?;
    let stored = serde_json::from_slice::<StoredSegmentSummary>(
        &fs::read(segment_summary_path(transcript_path)).ok()?,
    )
    .ok()?;
    (stored.format_version == SEGMENT_SUMMARY_FORMAT_VERSION
        && stored.transcript_filename == transcript_path.file_name()?.to_string_lossy().as_ref()
        && stored.transcript_bytes == metadata.len()
        && stored.transcript_modified_unix_ns == modified_unix_ns(&metadata))
    .then_some(stored.summary)
}

fn write_segment_summary(transcript_path: &Path, summary: &SessionSummary) {
    let Some(directory) = transcript_path.parent() else {
        return;
    };
    let Ok(metadata) = fs::metadata(transcript_path) else {
        return;
    };
    let stored = StoredSegmentSummary {
        format_version: SEGMENT_SUMMARY_FORMAT_VERSION,
        transcript_filename: transcript_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        transcript_bytes: metadata.len(),
        transcript_modified_unix_ns: modified_unix_ns(&metadata),
        summary: summary.clone(),
    };
    let Ok(encoded) = serde_json::to_vec(&stored) else {
        return;
    };
    let Ok(mut temporary) = NamedTempFile::new_in(directory) else {
        return;
    };
    if temporary.write_all(&encoded).is_err() {
        return;
    }
    drop(temporary.persist(segment_summary_path(transcript_path)));
}

fn segment_summary_path(transcript_path: &Path) -> PathBuf {
    transcript_path.with_extension("summary.json")
}

fn compact_projection_records(records: Vec<Arc<TranscriptRecord>>) -> Vec<Arc<TranscriptRecord>> {
    let records = discard_completed_assistant_deltas(records);
    let latest_outbound = records
        .iter()
        .rposition(|record| api_event_projection(record) == ApiEventProjection::LatestOutbound);
    records
        .into_iter()
        .enumerate()
        .filter_map(|(index, record)| match api_event_projection(&record) {
            ApiEventProjection::Discard => None,
            ApiEventProjection::Retain => Some(record),
            ApiEventProjection::LatestOutbound if Some(index) == latest_outbound => {
                let (prompt_cache, previous_response) = outbound_context_snapshot(&record)?;
                TranscriptRecord::from_local(
                    record.sequence(),
                    record.recorded_at_unix_ms(),
                    LocalEvent::ContextObserved {
                        prompt_cache,
                        previous_response,
                    },
                )
                .ok()
                .map(Arc::new)
            }
            ApiEventProjection::LatestOutbound => None,
        })
        .collect()
}

fn projection_cache_worthwhile(observed_records: usize, projected_records: usize) -> bool {
    observed_records > 0 && projected_records.saturating_mul(2) <= observed_records
}

fn discard_completed_assistant_deltas(
    records: Vec<Arc<TranscriptRecord>>,
) -> Vec<Arc<TranscriptRecord>> {
    let mut completed_messages = HashSet::new();
    let mut compacted = Vec::with_capacity(records.len());
    for record in records.into_iter().rev() {
        match record.kind() {
            "assistant.message" => {
                if let Some(key) = assistant_message_key(&record) {
                    completed_messages.insert(key);
                }
                compacted.push(record);
            }
            "assistant.delta" => {
                let completed = assistant_message_key(&record)
                    .is_some_and(|key| completed_messages.contains(&key));
                if !completed {
                    compacted.push(record);
                }
            }
            _ => compacted.push(record),
        }
    }
    compacted.reverse();
    compacted
}

fn assistant_message_key(record: &TranscriptRecord) -> Option<AssistantMessageKey> {
    let payload = record.decode_payload::<AssistantMessageIdentity>().ok()?;
    Some(AssistantMessageKey {
        model_call_index: payload.model_call_index,
        item_id: payload.item_id,
        commentary: payload.phase.as_deref() == Some("commentary"),
    })
}

#[derive(Eq, Hash, PartialEq)]
struct AssistantMessageKey {
    model_call_index: u32,
    item_id: Option<String>,
    commentary: bool,
}

#[derive(Deserialize)]
struct AssistantMessageIdentity {
    model_call_index: u32,
    item_id: Option<String>,
    phase: Option<String>,
}

fn read_transcript_projection(
    config_path: &Path,
    session_id: &str,
) -> Option<Vec<Arc<TranscriptRecord>>> {
    let path = transcript_projection_path(config_path, session_id);
    let decoder = Decoder::new(File::open(path).ok()?).ok()?;
    let stored =
        serde_json::from_reader::<_, StoredTranscriptProjection>(BufReader::new(decoder)).ok()?;
    if stored.format_version != PROJECTION_FORMAT_VERSION {
        return None;
    }
    if stored.session_id != session_id {
        return None;
    }
    let fingerprint = transcript_fingerprint(config_path).ok()?;
    (stored.transcript_fingerprint == fingerprint).then_some(stored.records)
}

fn write_transcript_projection(
    config_path: &Path,
    session_id: &str,
    source_fingerprint: &[TranscriptFingerprint],
    records: &[Arc<TranscriptRecord>],
) {
    let _ = try_write_transcript_projection(config_path, session_id, source_fingerprint, records);
}

fn try_write_transcript_projection(
    config_path: &Path,
    session_id: &str,
    source_fingerprint: &[TranscriptFingerprint],
    records: &[Arc<TranscriptRecord>],
) -> Option<()> {
    let directory = transcript_projection_directory(config_path);
    create_private_directory(&directory).ok()?;
    let path = transcript_projection_path(config_path, session_id);
    let mut temporary = NamedTempFile::new_in(&directory).ok()?;
    {
        let mut output = Encoder::new(&mut temporary, COMPRESSION_LEVEL).ok()?;
        serde_json::to_writer(
            &mut output,
            &TranscriptProjectionEnvelope {
                format_version: PROJECTION_FORMAT_VERSION,
                session_id,
                transcript_fingerprint: source_fingerprint,
                records,
            },
        )
        .ok()?;
        output.finish().ok()?;
    }
    temporary.flush().ok()?;
    if transcript_fingerprint(config_path).ok()?.as_slice() != source_fingerprint {
        return None;
    }
    temporary.persist(path).ok()?;
    Some(())
}

fn transcript_fingerprint(config_path: &Path) -> Result<Vec<TranscriptFingerprint>, SessionError> {
    transcript_paths(config_path)?
        .into_iter()
        .map(|path| {
            let metadata = fs::metadata(&path).map_err(|source| SessionError::ReadDirectory {
                path: path.clone(),
                source,
            })?;
            Ok(TranscriptFingerprint {
                filename: path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                bytes: metadata.len(),
                modified_unix_ns: modified_unix_ns(&metadata),
            })
        })
        .collect()
}

fn modified_unix_ns(metadata: &fs::Metadata) -> u128 {
    metadata
        .modified()
        .unwrap_or(UNIX_EPOCH)
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn transcript_projection_directory(config_path: &Path) -> PathBuf {
    data_directory(config_path).join("transcript-projections/v1")
}

fn transcript_projection_path(config_path: &Path, session_id: &str) -> PathBuf {
    transcript_projection_directory(config_path)
        .join(format!("{}.json.zst", encode_filename(session_id)))
}

fn checkpoint_directory(config_path: &Path) -> PathBuf {
    checkpoint_root(config_path).join(format!("v{CHECKPOINT_FORMAT_VERSION}"))
}

fn checkpoint_path(config_path: &Path, session_id: &str) -> PathBuf {
    checkpoint_directory(config_path).join(format!("{}.json.zst", encode_filename(session_id)))
}

fn checkpoint_root(config_path: &Path) -> PathBuf {
    data_directory(config_path).join("checkpoints")
}

fn obsolete_checkpoint_path(config_path: &Path, session_id: &str) -> PathBuf {
    checkpoint_root(config_path).join(format!("{}.json.zst", encode_filename(session_id)))
}

fn remove_obsolete_checkpoint(config_path: &Path, session_id: &str) -> Result<bool, SessionError> {
    let path = obsolete_checkpoint_path(config_path, session_id);
    match fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(SessionError::RemoveObsoleteCheckpoint { path, source }),
    }
}

pub(super) fn remove_obsolete_checkpoints(config_path: &Path) -> Result<(), SessionError> {
    let directory = checkpoint_root(config_path);
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(SessionError::ReadDirectory {
                path: directory,
                source,
            });
        }
    };
    for entry in entries {
        let entry = entry.map_err(|source| SessionError::ReadDirectory {
            path: directory.clone(),
            source,
        })?;
        let file_type = entry
            .file_type()
            .map_err(|source| SessionError::ReadDirectory {
                path: directory.clone(),
                source,
            })?;
        if !file_type.is_file() {
            continue;
        }
        let path = entry.path();
        if !is_unversioned_checkpoint(&path) {
            continue;
        }
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(source) if source.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(SessionError::RemoveObsoleteCheckpoint { path, source });
            }
        }
    }
    Ok(())
}

fn is_unversioned_checkpoint(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some(session_id) = name.strip_suffix(".json.zst") else {
        return false;
    };
    !session_id.is_empty()
        && session_id.len() % 2 == 0
        && session_id.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn has_checkpoint(config_path: &Path, session_id: &str) -> Result<bool, SessionError> {
    let path = checkpoint_path(config_path, session_id);
    match fs::metadata(&path) {
        Ok(metadata) => Ok(metadata.is_file()),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(SessionError::ReadCheckpoint { path, source }),
    }
}

fn data_directory(config_path: &Path) -> &Path {
    config_path.parent().unwrap_or_else(|| Path::new("."))
}

fn encode_filename(value: &str) -> String {
    use std::fmt::Write as _;
    let mut encoded = String::with_capacity(value.len() * 2);
    for byte in value.as_bytes() {
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
}

fn create_private_directory(path: &Path) -> Result<(), SessionError> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder
        .create(path)
        .map_err(|source| SessionError::CreateDirectory {
            path: path.to_path_buf(),
            source,
        })
}

#[cfg(test)]
mod tests {
    use super::{
        encode_filename, format_age, list, list_async, load_checkpoint, load_transcript,
        load_transcript_async, obsolete_checkpoint_path, save_checkpoint, transcript_fingerprint,
        transcript_paths, transcript_projection_path, write_transcript_projection,
    };
    use crate::{
        app::config::{ReasoningEffort, ReasoningMode},
        tui::transcript::{LocalEvent, SessionStarted, TranscriptJournal, TurnId},
    };
    use nanocodex::agent::{
        events::{AgentEvent, AgentEventKind},
        session::SessionSnapshot,
    };
    use serde_json::{Value, json, value::to_raw_value};
    use std::{fs, io::Write, path::Path, sync::Arc};
    use tempfile::tempdir;

    fn snapshot(lineage: &str) -> SessionSnapshot {
        serde_json::from_value(json!({
            "version": 1,
            "model": nanocodex::oai::MODEL,
            "lineage_id": lineage,
            "prompt_cache_key": "test-cache-key",
            "workspace": "/work",
            "request_prefix": [
                {"type": "additional_tools", "role": "developer", "tools": []},
                {"type": "message", "role": "developer", "content": []}
            ],
            "canonical_context": {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "hello"}]
            },
            "history": [{
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "hello"}]
            }]
        }))
        .unwrap()
    }

    async fn write_minimal_session(config: &Path, session_id: &str) {
        let (mut journal, writer) = TranscriptJournal::open(config, session_id).unwrap();
        journal
            .append_local(LocalEvent::SessionStarted(SessionStarted {
                session_id: session_id.to_owned(),
                parent_session_id: None,
                model: "model".to_owned(),
                effort: ReasoningEffort::Medium,
                reasoning_mode: ReasoningMode::Standard,
                fast_mode: false,
                workspace: "/work".into(),
                application_version: "test".to_owned(),
            }))
            .unwrap();
        drop(journal);
        writer.into_task().await.unwrap().unwrap();
    }

    fn write_projection_cache(config: &Path, session_id: &str) {
        let records = load_transcript(config, session_id).unwrap();
        let source_fingerprint = transcript_fingerprint(config).unwrap();
        write_transcript_projection(config, session_id, &source_fingerprint, &records);
    }

    #[test]
    fn checkpoint_filenames_are_distinct_and_path_safe() {
        assert_eq!(encode_filename("a/b"), "612f62");
        assert_ne!(encode_filename("a/b"), encode_filename("a_b"));
    }

    #[test]
    fn nanocodex_0_2_snapshot_shape_remains_compatible() {
        let snapshot = serde_json::to_value(snapshot("legacy-lineage")).unwrap();

        assert_eq!(snapshot["version"], 1);
        assert_eq!(snapshot["lineage_id"], "legacy-lineage");
        assert!(snapshot.get("base_instructions").is_none());
        assert!(snapshot.get("context_snapshot").is_none());
    }

    #[test]
    fn age_is_human_readable() {
        assert!(!format_age(0).is_empty());
    }

    #[test]
    fn checkpoint_is_compressed_and_atomically_replaced() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let obsolete = obsolete_checkpoint_path(&config, "session");
        std::fs::create_dir_all(obsolete.parent().unwrap()).unwrap();
        std::fs::write(&obsolete, b"obsolete checkpoint").unwrap();
        save_checkpoint(&config, "session", &snapshot("first"), "first instructions").unwrap();
        save_checkpoint(
            &config,
            "session",
            &snapshot("second"),
            "second instructions",
        )
        .unwrap();

        let restored = load_checkpoint(&config, "session").unwrap();
        let (restored, instructions) = restored.into_parts();
        let restored = serde_json::to_value(restored).unwrap();
        assert_eq!(restored["lineage_id"], Value::String("second".to_owned()));
        assert_eq!(instructions, "second instructions");
        assert!(!obsolete.exists());
        let checkpoints = std::fs::read_dir(directory.path().join("checkpoints/v1"))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(checkpoints.len(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = checkpoints[0].metadata().unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[tokio::test]
    async fn catalog_only_includes_sessions_from_the_requested_workspace() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let (mut journal, writer) = TranscriptJournal::open(&config, "session/one").unwrap();
        journal
            .append_local(LocalEvent::SessionStarted(SessionStarted {
                session_id: "session/one".to_owned(),
                parent_session_id: None,
                model: "model".to_owned(),
                effort: ReasoningEffort::High,
                reasoning_mode: ReasoningMode::Pro,
                fast_mode: false,
                workspace: "/work".into(),
                application_version: "test".to_owned(),
            }))
            .unwrap();
        journal
            .append_local(LocalEvent::UserSubmitted {
                id: TurnId::new(1),
                text: "  inspect\n the workspace  ".to_owned(),
            })
            .unwrap();
        journal
            .append_local(LocalEvent::EffortChanged {
                from: ReasoningEffort::High,
                to: ReasoningEffort::Low,
            })
            .unwrap();
        drop(journal);
        writer.into_task().await.unwrap().unwrap();
        save_checkpoint(&config, "session/one", &snapshot("lineage"), "instructions").unwrap();

        let (mut journal, writer) = TranscriptJournal::open(&config, "other-session").unwrap();
        journal
            .append_local(LocalEvent::SessionStarted(SessionStarted {
                session_id: "other-session".to_owned(),
                parent_session_id: None,
                model: "model".to_owned(),
                effort: ReasoningEffort::Medium,
                reasoning_mode: ReasoningMode::Standard,
                fast_mode: false,
                workspace: "/other-workspace".into(),
                application_version: "test".to_owned(),
            }))
            .unwrap();
        drop(journal);
        writer.into_task().await.unwrap().unwrap();
        save_checkpoint(
            &config,
            "other-session",
            &snapshot("other-lineage"),
            "instructions",
        )
        .unwrap();

        let sessions = list(&config, Path::new("/work")).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "session/one");
        assert_eq!(sessions[0].preview, "inspect the workspace");
        assert_eq!(sessions[0].effort, ReasoningEffort::Low);
        assert_eq!(sessions[0].reasoning_mode, ReasoningMode::Pro);
        let summaries = fs::read_dir(directory.path().join("transcripts/v1"))
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.to_string_lossy().ends_with(".summary.json"))
            .collect::<Vec<_>>();
        assert_eq!(summaries.len(), 1);
        fs::write(&summaries[0], b"invalid cache").unwrap();
        let async_sessions = list_async(config.clone(), Path::new("/work").to_path_buf())
            .await
            .unwrap();
        assert_eq!(async_sessions.len(), 1);
        assert_eq!(async_sessions[0].session_id, "session/one");
        assert_eq!(load_transcript(&config, "session/one").unwrap().len(), 3);
        let loaded = load_transcript_async(config.clone(), "session/one".to_owned())
            .await
            .unwrap();
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[0].kind(), "session.started");
        assert_eq!(loaded[2].kind(), "effort.changed");
    }

    #[tokio::test]
    async fn projection_cache_discards_streaming_api_events_and_recovers_from_corruption() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let (mut journal, writer) = TranscriptJournal::open(&config, "session").unwrap();
        journal
            .append_local(LocalEvent::SessionStarted(SessionStarted {
                session_id: "session".to_owned(),
                parent_session_id: None,
                model: "model".to_owned(),
                effort: ReasoningEffort::Medium,
                reasoning_mode: ReasoningMode::Standard,
                fast_mode: false,
                workspace: "/work".into(),
                application_version: "test".to_owned(),
            }))
            .unwrap();
        for (sequence, payload) in [
            json!({
                "direction": "outbound",
                "phase": "generation",
                "event": {"prompt_cache_key": "first"}
            }),
            json!({
                "direction": "inbound",
                "phase": "generation",
                "event": {"type": "response.output_text.delta", "delta": "discard me"}
            }),
            json!({
                "direction": "outbound",
                "phase": "generation",
                "event": {
                    "prompt_cache_key": "latest-secret-key",
                    "previous_response_id": "secret-response-id"
                }
            }),
            json!({
                "direction": "inbound",
                "phase": "generation",
                "event": {
                    "type": "response.completed",
                    "response": {"usage": {"total_tokens": 42}}
                }
            }),
        ]
        .into_iter()
        .enumerate()
        {
            journal
                .append_agent(AgentEvent {
                    protocol_version: 1,
                    request_id: Arc::from("request"),
                    seq: u64::try_from(sequence).unwrap(),
                    kind: AgentEventKind::ApiEvent,
                    payload: to_raw_value(&payload).unwrap().into(),
                })
                .unwrap();
        }
        for (sequence, kind, payload) in [
            (
                4,
                AgentEventKind::AssistantDelta,
                json!({
                    "model_call_index": 1,
                    "item_id": "message",
                    "phase": "final_answer",
                    "text": "partial"
                }),
            ),
            (
                5,
                AgentEventKind::AssistantDelta,
                json!({
                    "model_call_index": 1,
                    "item_id": "message",
                    "phase": "final_answer",
                    "text": " response"
                }),
            ),
            (
                6,
                AgentEventKind::AssistantMessage,
                json!({
                    "model_call_index": 1,
                    "item_id": "message",
                    "phase": "final_answer",
                    "text": "complete response"
                }),
            ),
        ] {
            journal
                .append_agent(AgentEvent {
                    protocol_version: 1,
                    request_id: Arc::from("request"),
                    seq: sequence,
                    kind,
                    payload: to_raw_value(&payload).unwrap().into(),
                })
                .unwrap();
        }
        drop(journal);
        writer.into_task().await.unwrap().unwrap();

        let projected = load_transcript(&config, "session").unwrap();
        assert_eq!(projected.len(), 4);
        assert_eq!(
            projected
                .iter()
                .filter(|record| record.kind() == "api.event")
                .count(),
            1
        );
        assert_eq!(
            projected
                .iter()
                .filter(|record| record.kind() == "assistant.delta")
                .count(),
            0
        );
        assert_eq!(
            projected
                .iter()
                .filter(|record| record.kind() == "assistant.message")
                .count(),
            1
        );

        let cache = transcript_projection_path(&config, "session");
        assert!(cache.is_file());
        let cached = load_transcript(&config, "session").unwrap();
        assert_eq!(cached.len(), projected.len());
        assert!(!format!("{cached:?}").contains("latest-secret-key"));
        assert!(!format!("{cached:?}").contains("secret-response-id"));

        fs::write(&cache, b"invalid cache").unwrap();
        let recovered = load_transcript(&config, "session").unwrap();
        assert_eq!(recovered.len(), projected.len());
        assert!(fs::metadata(cache).unwrap().len() > b"invalid cache".len() as u64);
    }

    #[tokio::test]
    async fn visible_transcript_does_not_create_projection_cache() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let (mut journal, writer) = TranscriptJournal::open(&config, "session").unwrap();
        journal
            .append_local(LocalEvent::SessionStarted(SessionStarted {
                session_id: "session".to_owned(),
                parent_session_id: None,
                model: "model".to_owned(),
                effort: ReasoningEffort::Medium,
                reasoning_mode: ReasoningMode::Standard,
                fast_mode: false,
                workspace: "/work".into(),
                application_version: "test".to_owned(),
            }))
            .unwrap();
        journal
            .append_local(LocalEvent::UserSubmitted {
                id: TurnId::new(1),
                text: "inspect the workspace".to_owned(),
            })
            .unwrap();
        for (sequence, kind, payload) in [
            (0, AgentEventKind::RunStarted, json!({})),
            (
                1,
                AgentEventKind::AssistantDelta,
                json!({
                    "model_call_index": 1,
                    "item_id": "message",
                    "phase": "final_answer",
                    "text": "partial response"
                }),
            ),
            (
                2,
                AgentEventKind::AssistantMessage,
                json!({
                    "model_call_index": 1,
                    "item_id": "message",
                    "phase": "final_answer",
                    "text": "complete response"
                }),
            ),
            (3, AgentEventKind::RunCompleted, json!({})),
        ] {
            journal
                .append_agent(AgentEvent {
                    protocol_version: 1,
                    request_id: Arc::from("request"),
                    seq: sequence,
                    kind,
                    payload: to_raw_value(&payload).unwrap().into(),
                })
                .unwrap();
        }
        drop(journal);
        writer.into_task().await.unwrap().unwrap();

        let projected = load_transcript(&config, "session").unwrap();

        assert_eq!(
            projected
                .iter()
                .filter(|record| record.kind() == "assistant.delta")
                .count(),
            1
        );
        assert!(!transcript_projection_path(&config, "session").exists());
    }

    #[tokio::test]
    async fn projection_cache_is_bound_to_its_session() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        write_minimal_session(&config, "session-one").await;
        write_minimal_session(&config, "session-two").await;
        write_projection_cache(&config, "session-one");
        write_projection_cache(&config, "session-two");
        let first = transcript_projection_path(&config, "session-one");
        let second = transcript_projection_path(&config, "session-two");
        let first_bytes = fs::read(&first).unwrap();
        fs::write(&first, fs::read(&second).unwrap()).unwrap();
        fs::write(&second, first_bytes).unwrap();

        let records = load_transcript(&config, "session-one").unwrap();
        let started = super::session_started(&records).unwrap();

        assert_eq!(started.session_id, "session-one");
    }

    #[tokio::test]
    async fn projection_cache_is_not_written_for_changed_source() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        write_minimal_session(&config, "session-one").await;
        let records = load_transcript(&config, "session-one").unwrap();
        let source_fingerprint = transcript_fingerprint(&config).unwrap();
        let cache = transcript_projection_path(&config, "session-one");
        assert!(!cache.exists());

        let transcript = transcript_paths(&config).unwrap().remove(0);
        std::fs::OpenOptions::new()
            .append(true)
            .open(transcript)
            .unwrap()
            .write_all(b"changed")
            .unwrap();

        write_transcript_projection(&config, "session-one", &source_fingerprint, &records);

        assert!(!cache.exists());
    }

    #[tokio::test]
    async fn segment_summary_is_bound_to_its_transcript() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        write_minimal_session(&config, "session-one").await;
        load_transcript(&config, "session-one").unwrap();
        assert!(!directory.path().join("transcript-projections").exists());
        let summaries = fs::read_dir(directory.path().join("transcripts/v1"))
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.to_string_lossy().ends_with(".summary.json"))
            .collect::<Vec<_>>();
        assert_eq!(summaries.len(), 1);
        let summary = fs::read_to_string(&summaries[0])
            .unwrap()
            .replace("session-one", "session-xxx");
        fs::write(&summaries[0], summary).unwrap();

        let records = load_transcript(&config, "session-one").unwrap();
        let started = super::session_started(&records).unwrap();

        assert_eq!(started.session_id, "session-one");
    }
}
