//! Errors from running sessions, the terminal interface, and their background tasks.

use crate::core::pane::PaneId;
use std::{env::VarError, io, path::PathBuf};
use tact_memory::RemoteClientError;
use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum RuntimeError {
    #[error(
        "interactive mode requires terminal stdin and stdout; use `tact run <PROMPT>` for JSONL output"
    )]
    InteractiveTerminal,
    #[error("terminal operation failed: {0}")]
    Terminal(#[source] io::Error),
    #[error("failed to configure remote memory: {0}")]
    RemoteMemory(#[source] RemoteClientError),
    #[error("the external-editor task stopped unexpectedly: {0}")]
    ExternalEditorTask(#[source] tokio::task::JoinError),
    #[error("the effort update task stopped unexpectedly: {0}")]
    EffortUpdateTask(#[source] tokio::task::JoinError),
    #[error("the speed update task stopped unexpectedly: {0}")]
    SpeedUpdateTask(#[source] tokio::task::JoinError),
    #[error("the new-session task stopped unexpectedly: {0}")]
    NewSessionTask(#[source] tokio::task::JoinError),
    #[error("the handoff task stopped unexpectedly: {0}")]
    HandoffTask(#[source] tokio::task::JoinError),
    #[error("the session task stopped unexpectedly: {0}")]
    SessionTask(#[source] tokio::task::JoinError),
    #[error("the Nanocodex worker stopped before accepting a command")]
    AgentWorkerStopped,
    #[error("pane {0:?} has no open session")]
    PaneUnavailable(PaneId),
    #[error("failed to resolve workspace {path}: {source}")]
    ResolveWorkspace {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("workspace is not a directory: {0}")]
    WorkspaceNotDirectory(PathBuf),
    #[cfg(feature = "harbor-evals")]
    #[error("failed to create orchestration log {path}: {source}")]
    CreateOrchestrationLog {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[cfg(feature = "harbor-evals")]
    #[error("failed to encode orchestration log {path}: {source}")]
    EncodeOrchestrationLog {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[cfg(feature = "harbor-evals")]
    #[error("failed to write orchestration log {path}: {source}")]
    WriteOrchestrationLog {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[cfg(feature = "harbor-evals")]
    #[error("the orchestration log task stopped unexpectedly: {0}")]
    OrchestrationLogTask(#[source] tokio::task::JoinError),
    #[error("failed to listen for a shutdown signal: {0}")]
    ShutdownSignal(#[source] io::Error),
}

#[derive(Debug, Error)]
pub(crate) enum ExternalEditorError {
    #[error("$EDITOR is unavailable: {0}")]
    Unavailable(#[source] VarError),
    #[error("failed to parse $EDITOR value `{command}`")]
    Parse { command: String },
    #[error("failed to create an external-editor draft: {0}")]
    CreateDraft(#[source] io::Error),
    #[error("failed to write the external-editor draft: {0}")]
    WriteDraft(#[source] io::Error),
    #[error("failed to launch external editor `{program}`: {source}")]
    Launch {
        program: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to read the external-editor draft: {0}")]
    ReadDraft(#[source] io::Error),
}
