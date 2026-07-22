//! Typed errors exposed by the binary's internal module boundaries.

use crate::tui::{session::SessionError, transcript::TranscriptError};
use miette::Diagnostic;
use nanocodex::{ChatGptAuthError, McpBuildError, NanocodexError};
use nanocodex_core::EventError;
use std::{env::VarError, io, path::PathBuf, result::Result as StdResult};
use thiserror::Error;

pub(crate) type Result<T> = StdResult<T, Error>;
pub(crate) type AuthResult<T> = StdResult<T, AuthError>;

#[derive(Debug, Diagnostic, Error)]
pub(crate) enum Error {
    #[error(transparent)]
    Agent(#[from] NanocodexError),
    #[error(transparent)]
    Auth(#[from] AuthError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("failed to process the Nanocodex event stream: {0}")]
    Event(#[from] EventError),
    #[error(transparent)]
    ExternalEditor(#[from] ExternalEditorError),
    #[error("failed to configure MCP servers: {0}")]
    Mcp(#[source] McpBuildError),
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Transcript(#[from] TranscriptError),
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

#[derive(Debug, Error)]
pub(crate) enum AuthError {
    #[error(transparent)]
    ChatGpt(#[from] ChatGptAuthError),
    #[error("failed to inspect ChatGPT credential file {path}: {source}")]
    InspectCredentialFile {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("OPENAI_API_KEY is not set; set it or select ChatGPT authentication")]
    ApiKeyUnavailable,
    #[error(
        "no ChatGPT credentials found at {path} and OPENAI_API_KEY is not set; run `tact auth login` or set OPENAI_API_KEY"
    )]
    CredentialsUnavailable { path: PathBuf },
    #[error(transparent)]
    Secret(#[from] SecretError),
}

#[derive(Debug, Error)]
pub(crate) enum ConfigError {
    #[error("could not determine the config directory; set TACT_HOME or pass --config")]
    ConfigHomeUnavailable,
    #[error("could not determine the credential directory; set CODEX_HOME or pass --auth-file")]
    AuthHomeUnavailable,
    #[error("failed to determine the current directory: {0}")]
    CurrentDirectory(#[source] io::Error),
    #[error("failed to read configuration file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to parse configuration file {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("failed to serialize the effective configuration: {0}")]
    Serialize(#[source] toml::ser::Error),
    #[error("MCP server `{name}` is already configured")]
    McpServerExists { name: String },
    #[error("MCP environment variable {name} is not set")]
    McpEnvironmentNotPresent { name: String },
    #[error("MCP environment variable {name} is not valid Unicode")]
    McpEnvironmentNotUnicode { name: String },
    #[error("MCP server working directory is not valid Unicode: {0}")]
    McpWorkingDirectoryNotUnicode(PathBuf),
    #[error("failed to update configuration file {path}: {source}")]
    UpdateParse {
        path: PathBuf,
        #[source]
        source: toml_edit::TomlError,
    },
    #[error("failed to write configuration file {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

#[derive(Debug, Error)]
pub(crate) enum RuntimeError {
    #[error(
        "interactive mode requires terminal stdin and stdout; use `tact run <PROMPT>` for JSONL output"
    )]
    InteractiveTerminal,
    #[error("--resume is only available in interactive mode and cannot be used with a subcommand")]
    ResumeWithCommand,
    #[error("terminal operation failed: {0}")]
    Terminal(#[source] io::Error),
    #[error("the external-editor task stopped unexpectedly: {0}")]
    ExternalEditorTask(#[source] tokio::task::JoinError),
    #[error("the effort update task stopped unexpectedly: {0}")]
    EffortUpdateTask(#[source] tokio::task::JoinError),
    #[error("the new-session task stopped unexpectedly: {0}")]
    NewSessionTask(#[source] tokio::task::JoinError),
    #[error("the session task stopped unexpectedly: {0}")]
    SessionTask(#[source] tokio::task::JoinError),
    #[error("the Nanocodex worker stopped before accepting a command")]
    AgentWorkerStopped,
    #[error("failed to resolve workspace {path}: {source}")]
    ResolveWorkspace {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("workspace is not a directory: {0}")]
    WorkspaceNotDirectory(PathBuf),
    #[error("failed to listen for a shutdown signal: {0}")]
    ShutdownSignal(#[source] io::Error),
}

#[derive(Debug, Error)]
#[error("{name} is not valid Unicode")]
pub(crate) struct SecretError {
    pub(crate) name: &'static str,
}
