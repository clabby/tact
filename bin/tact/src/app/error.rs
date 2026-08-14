//! Typed errors exposed by the binary's internal module boundaries.

use crate::tui::{session::SessionError, transcript::TranscriptError};
use miette::Diagnostic;
use nanocodex::{
    NanocodexError,
    oai::{OpenAiError, auth::ChatGptAuthError, events::EventError},
    tools::mcp::McpBuildError,
};
use std::{
    env::VarError, error::Error as StdError, io, path::PathBuf, result::Result as StdResult,
};
use tact_memory::{MemoryError, RemoteClientError};
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
    OpenAi(#[from] OpenAiError),
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
    #[error(transparent)]
    MemoryTransfer(#[from] MemoryTransferError),
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Transcript(#[from] TranscriptError),
    #[error("update failed: {0}")]
    Update(#[source] Box<dyn StdError + Send + Sync>),
}

#[derive(Debug, Error)]
pub(crate) enum MemoryTransferError {
    #[error("remote memory is not configured")]
    RemoteNotConfigured,
    #[error("failed to read the local memory snapshot for upload: {0}")]
    Local(#[source] MemoryError),
    #[error(
        "local memories kept changing while the remote snapshot was synchronized; retry once writes settle"
    )]
    LocalChanged,
    #[error("memory upload failed: {0}")]
    Upload(#[source] RemoteClientError),
    #[error("remote memory rejected the upload: {0}")]
    UploadStore(#[source] MemoryError),
    #[error("memory pull failed: {0}")]
    Pull(#[source] RemoteClientError),
    #[error("remote memory rejected the pull: {0}")]
    PullStore(#[source] MemoryError),
    #[error("failed to merge pulled memories into the local store: {0}")]
    Merge(#[source] MemoryError),
}

impl Error {
    pub(crate) fn update(source: impl StdError + Send + Sync + 'static) -> Self {
        Self::Update(Box::new(source))
    }
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
    #[cfg(unix)]
    #[error(
        "configuration file {path} contains a remote memory bearer token but has insecure permissions {mode:#o}; remove all group and other permissions"
    )]
    InsecureRemoteMemoryPermissions { path: PathBuf, mode: u32 },
    #[cfg(not(unix))]
    #[error(
        "configuration file {path} contains a remote memory bearer token, but this platform's file privacy cannot be verified"
    )]
    UnsupportedRemoteMemoryPermissions { path: PathBuf },
    #[error("failed to serialize the effective configuration: {0}")]
    Serialize(#[source] toml::ser::Error),
    #[error("MCP server `{name}` is already configured")]
    McpServerExists { name: String },
    #[error("MCP server `{name}` has an invalid URL: {source}")]
    McpUrl {
        name: String,
        #[source]
        source: McpUrlError,
    },
    #[error(transparent)]
    RemoteMemory(#[from] RemoteMemoryConfigError),
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
pub(crate) enum McpUrlError {
    #[error("the URL must not be empty or whitespace-only")]
    Empty,
    #[error("the URL is not valid")]
    Parse(#[source] url::ParseError),
    #[error("the URL must use the http or https scheme")]
    UnsupportedScheme,
    #[error("the URL must not contain credentials")]
    Credentials,
}

#[derive(Debug, Error)]
pub(crate) enum RemoteMemoryConfigError {
    #[error(
        "remote memory configuration requires an endpoint, namespace, bearer token, and at least one workspace root"
    )]
    Incomplete,
    #[error("remote memory endpoint is invalid: {0}")]
    Endpoint(#[source] McpUrlError),
    #[error("remote memory namespace must be non-empty and have no leading or trailing whitespace")]
    NamespaceWhitespace,
    #[error("remote memory namespace must not contain control characters")]
    NamespaceControl,
    #[error(
        "remote memory namespace must use at most 128 ASCII letters, digits, periods, hyphens, or underscores"
    )]
    NamespaceInvalid,
    #[error("failed to resolve remote memory workspace root {path}: {source}")]
    ResolveWorkspaceRoot {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("remote memory workspace root is not a directory: {0}")]
    WorkspaceRootNotDirectory(PathBuf),
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
    #[error("failed to configure remote memory: {0}")]
    RemoteMemory(#[source] RemoteClientError),
    #[error("the external-editor task stopped unexpectedly: {0}")]
    ExternalEditorTask(#[source] tokio::task::JoinError),
    #[error("the effort update task stopped unexpectedly: {0}")]
    EffortUpdateTask(#[source] tokio::task::JoinError),
    #[error("the fast-mode update task stopped unexpectedly: {0}")]
    FastModeUpdateTask(#[source] tokio::task::JoinError),
    #[error("the new-session task stopped unexpectedly: {0}")]
    NewSessionTask(#[source] tokio::task::JoinError),
    #[error("the handoff task stopped unexpectedly: {0}")]
    HandoffTask(#[source] tokio::task::JoinError),
    #[error("the session task stopped unexpectedly: {0}")]
    SessionTask(#[source] tokio::task::JoinError),
    #[error("the Nanocodex worker stopped before accepting a command")]
    AgentWorkerStopped,
    #[error("invalid Nanocodex session ID: {0}")]
    InvalidSessionId(#[source] nanocodex::oai::session::SessionIdError),
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
#[error("{name} is not valid Unicode")]
pub(crate) struct SecretError {
    pub(crate) name: &'static str,
}
