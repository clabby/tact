//! Durable transcript records and their structured projection.

mod entry;
mod journal;
mod model;
mod record;

pub(crate) use entry::{
    EntryId, EntryKind, MessagePhase, ToolEntry, ToolState, TranscriptEntry, TransientStatus,
};
pub(crate) use journal::{TranscriptJournal, load};
pub(crate) use model::TranscriptModel;
pub(crate) use record::{
    LocalEvent, SessionEnded, SessionOutcome, SessionStarted, ShellId, TranscriptRecord, TurnId,
};
use std::{io, path::PathBuf};
use thiserror::Error;

/// Errors retain the transcript path because failures otherwise occur after the
/// terminal has already yielded its screen to the application.
#[derive(Debug, Error)]
pub(crate) enum TranscriptError {
    #[error("failed to create transcript directory {path}: {source}")]
    CreateDirectory {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to create transcript {path}: {source}")]
    Create {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to encode transcript record for {path}: {source}")]
    Encode {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to write transcript {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to synchronize transcript {path}: {source}")]
    Sync {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("the transcript writer for {0} stopped unexpectedly")]
    WriterStopped(PathBuf),
    #[error("the transcript writer task stopped unexpectedly: {0}")]
    WriterTask(#[source] tokio::task::JoinError),
    #[error("failed to read transcript {path}: {source}")]
    #[allow(
        dead_code,
        reason = "used when the planned session picker loads transcripts"
    )]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to decode transcript {path} at line {line}: {source}")]
    #[allow(
        dead_code,
        reason = "used when the planned session picker loads transcripts"
    )]
    Decode {
        path: PathBuf,
        line: usize,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "transcript {path} line {line} uses schema version {found}; this build supports version {supported}"
    )]
    #[allow(
        dead_code,
        reason = "used when the planned session picker loads transcripts"
    )]
    UnsupportedVersion {
        path: PathBuf,
        line: usize,
        found: u32,
        supported: u32,
    },
    #[error(
        "transcript {path} line {line} has sequence {found}; expected monotonically ordered sequence {expected}"
    )]
    #[allow(
        dead_code,
        reason = "used when the planned session picker loads transcripts"
    )]
    Sequence {
        path: PathBuf,
        line: usize,
        found: u64,
        expected: u64,
    },
}
