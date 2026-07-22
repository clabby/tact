//! Resumable Nanocodex checkpoints and transcript-derived session discovery.

use crate::{
    config::ReasoningEffort,
    tui::transcript::{self, SessionStarted, TranscriptRecord},
};
use nanocodex::SessionSnapshot;
use std::{
    collections::HashMap,
    fs::{self, File},
    io::{self, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tempfile::NamedTempFile;
use thiserror::Error;
use zstd::stream::{read::Decoder, write::Encoder};

const COMPRESSION_LEVEL: i32 = 3;

#[cfg(unix)]
use std::os::unix::fs::DirBuilderExt;

#[derive(Clone, Debug)]
pub(crate) struct SessionSummary {
    pub(crate) session_id: String,
    pub(crate) started_at_unix_ms: u64,
    pub(crate) model: String,
    pub(crate) effort: ReasoningEffort,
    pub(crate) workspace: PathBuf,
    pub(crate) preview: String,
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
    #[error("no resumable checkpoint exists for session {session_id}")]
    MissingCheckpoint { session_id: String },
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
    #[error(transparent)]
    Transcript(#[from] transcript::TranscriptError),
}

pub(crate) fn save_checkpoint(
    config_path: &Path,
    session_id: &str,
    snapshot: &SessionSnapshot,
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
    serde_json::to_writer(&mut output, snapshot).map_err(|source| {
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
    Ok(())
}

pub(crate) fn load_checkpoint(
    config_path: &Path,
    session_id: &str,
) -> Result<SessionSnapshot, SessionError> {
    let path = checkpoint_path(config_path, session_id);
    let file = File::open(&path).map_err(|source| {
        if source.kind() == io::ErrorKind::NotFound {
            SessionError::MissingCheckpoint {
                session_id: session_id.to_owned(),
            }
        } else {
            SessionError::ReadCheckpoint {
                path: path.clone(),
                source,
            }
        }
    })?;
    let decoder = Decoder::new(file).map_err(|source| SessionError::ReadCheckpoint {
        path: path.clone(),
        source,
    })?;
    serde_json::from_reader(BufReader::new(decoder))
        .map_err(|source| SessionError::DecodeCheckpoint { path, source })
}

pub(crate) fn list(
    config_path: &Path,
    workspace: &Path,
) -> Result<Vec<SessionSummary>, SessionError> {
    let mut sessions = HashMap::<String, SessionSummary>::new();
    for path in transcript_paths(config_path)? {
        let records = transcript::load(&path)?;
        let Some(started) = session_started(&records) else {
            continue;
        };
        if started.workspace != workspace {
            continue;
        }
        if !checkpoint_path(config_path, &started.session_id).is_file() {
            continue;
        }
        let started_at_unix_ms = records
            .first()
            .map_or(0, |record| record.recorded_at_unix_ms());
        let preview = first_user_message(&records).unwrap_or_else(|| "No user prompt".to_owned());
        let summary = SessionSummary {
            session_id: started.session_id.clone(),
            started_at_unix_ms,
            model: started.model,
            effort: latest_effort(&records, started.effort),
            workspace: started.workspace,
            preview,
        };
        sessions
            .entry(started.session_id)
            .and_modify(|existing| {
                if summary.started_at_unix_ms > existing.started_at_unix_ms {
                    existing.started_at_unix_ms = summary.started_at_unix_ms;
                    existing.effort = summary.effort;
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

pub(crate) fn load_transcript(
    config_path: &Path,
    session_id: &str,
) -> Result<Vec<Arc<TranscriptRecord>>, SessionError> {
    let mut records = Vec::new();
    for path in transcript_paths(config_path)? {
        let segment = transcript::load(&path)?;
        if session_started(&segment).is_some_and(|started| started.session_id == session_id) {
            records.extend(segment);
        }
    }
    Ok(records)
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
    let directory = data_directory(config_path).join("transcripts");
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
    record.decode_payload().ok()
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

fn checkpoint_directory(config_path: &Path) -> PathBuf {
    data_directory(config_path).join("checkpoints")
}

fn checkpoint_path(config_path: &Path, session_id: &str) -> PathBuf {
    checkpoint_directory(config_path).join(format!("{}.json.zst", encode_filename(session_id)))
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
        encode_filename, format_age, list, load_checkpoint, load_transcript, save_checkpoint,
    };
    use crate::{
        config::ReasoningEffort,
        tui::transcript::{LocalEvent, SessionStarted, TranscriptJournal, TurnId},
    };
    use nanocodex::SessionSnapshot;
    use serde_json::{Value, json};
    use std::path::Path;
    use tempfile::tempdir;

    fn snapshot(lineage: &str) -> SessionSnapshot {
        serde_json::from_value(json!({
            "version": 1,
            "model": nanocodex::MODEL,
            "lineage_id": lineage,
            "workspace": "/work",
            "request_prefix": [
                {"type": "additional_tools", "role": "developer", "tools": []},
                {"type": "message", "role": "developer", "content": []}
            ],
            "canonical_context": {"type": "message", "role": "developer", "content": []},
            "history": [],
            "checkpoint": null
        }))
        .unwrap()
    }

    #[test]
    fn checkpoint_filenames_are_distinct_and_path_safe() {
        assert_eq!(encode_filename("a/b"), "612f62");
        assert_ne!(encode_filename("a/b"), encode_filename("a_b"));
    }

    #[test]
    fn age_is_human_readable() {
        assert!(!format_age(0).is_empty());
    }

    #[test]
    fn checkpoint_is_compressed_and_atomically_replaced() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        save_checkpoint(&config, "session", &snapshot("first")).unwrap();
        save_checkpoint(&config, "session", &snapshot("second")).unwrap();

        let restored = load_checkpoint(&config, "session").unwrap();
        let restored = serde_json::to_value(restored).unwrap();
        assert_eq!(restored["lineage_id"], Value::String("second".to_owned()));
        let checkpoints = std::fs::read_dir(directory.path().join("checkpoints"))
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
        save_checkpoint(&config, "session/one", &snapshot("lineage")).unwrap();

        let (mut journal, writer) = TranscriptJournal::open(&config, "other-session").unwrap();
        journal
            .append_local(LocalEvent::SessionStarted(SessionStarted {
                session_id: "other-session".to_owned(),
                parent_session_id: None,
                model: "model".to_owned(),
                effort: ReasoningEffort::Medium,
                workspace: "/other-workspace".into(),
                application_version: "test".to_owned(),
            }))
            .unwrap();
        drop(journal);
        writer.into_task().await.unwrap().unwrap();
        save_checkpoint(&config, "other-session", &snapshot("other-lineage")).unwrap();

        let sessions = list(&config, Path::new("/work")).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "session/one");
        assert_eq!(sessions[0].preview, "inspect the workspace");
        assert_eq!(sessions[0].effort, ReasoningEffort::Low);
        assert_eq!(load_transcript(&config, "session/one").unwrap().len(), 3);
    }
}
