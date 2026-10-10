//! Errors from selecting and using provider credentials.

use nanocodex::oai::auth::ChatGptAuthError;
use std::{io, path::PathBuf};
use thiserror::Error;

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
    #[error("OpenAI API-key authentication requires openai.api_key or OPENAI_API_KEY")]
    ApiKeyUnavailable,
    #[error(
        "decisions require an OpenAI API key; set openai.api_key or OPENAI_API_KEY, or set openai.decisions_api_key when signed in with ChatGPT"
    )]
    DecisionsApiKeyUnavailable,
    #[error("Claude API-key authentication requires claude.api_key or ANTHROPIC_API_KEY")]
    ClaudeApiKeyUnavailable,
    #[error(
        "Claude API-key authentication requires an Anthropic API key (sk-ant-api... or sk-ant-usr-...); Claude subscription and OAuth tokens are not supported"
    )]
    InvalidClaudeApiKey,
    #[error(
        "no ChatGPT credentials found at {path} and no OpenAI API key is configured; run `tact auth login` or set openai.api_key or OPENAI_API_KEY"
    )]
    CredentialsUnavailable { path: PathBuf },
    #[error(transparent)]
    Secret(#[from] SecretError),
}

#[derive(Debug, Error)]
#[error("{name} is not valid Unicode")]
pub(crate) struct SecretError {
    pub(crate) name: &'static str,
}
