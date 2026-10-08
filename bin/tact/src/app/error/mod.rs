//! Typed errors exposed by the binary's internal module boundaries.
//!
//! [`Error`] is the application-wide error. Each layer owns a narrower error type: command-line
//! dispatch ([`CliError`]), configuration ([`ConfigError`]), credentials ([`AuthError`]), and
//! running sessions ([`RuntimeError`]). Every wrapper preserves its cause as an error source.

mod auth;
mod cli;
mod config;
mod runtime;

pub(crate) use auth::{AuthError, SecretError};
pub(crate) use cli::CliError;
pub(crate) use config::{
    ConfigEditError, ConfigError, ConfigSyntaxError, McpUrlError, RemoteMemoryConfigError,
};
pub(crate) use runtime::{ExternalEditorError, RuntimeError};

use crate::core::{session::SessionError, transcript::TranscriptError};
use miette::Diagnostic;
use nanocodex::{
    NanocodexError,
    oai::{OpenAiError, events::EventError},
    tools::mcp::McpBuildError,
};
use std::result::Result as StdResult;
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
    Cli(#[from] CliError),
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
    Session(#[from] SessionError),
    #[error(transparent)]
    Transcript(#[from] TranscriptError),
}
