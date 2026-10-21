//! One tool-less model request outside any session, for small background jobs such as session
//! titles and the web monitor.
//!
//! Requests use the process's own credentials: OpenAI models authenticate like the agent does,
//! and Claude models need Claude enabled with an API key. Access-gated Claude models are not
//! served here; they authenticate with separate Glasswing credentials that only sessions use.

use crate::app::{auth::ClaudeApiKey, config::Config, model, secret::SecretString};
use nanocode::{
    ClaudeModel, Model as CodexModel, OpenAi, Thinking,
    claude::{ClaudeClient, ContentBlock, Effort, Message, MessagesRequest, OutputConfig, Role},
    oai::{auth::OpenAiAuth, transport::ResponsesTransport},
};
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::json;
use std::{num::NonZeroU32, time::Duration};
use tact_subagents::{HarnessModel as Model, is_access_gated};
use thiserror::Error;

/// One request: the model, its instructions and input, and the bounds it runs under.
pub(crate) struct DirectRequest<'a> {
    pub(crate) model: Model,
    pub(crate) instructions: &'a str,
    pub(crate) input: &'a str,
    /// The reasoning effort, applied as sessions apply it: the OpenAI effort, or adaptive
    /// thinking at that effort for Claude. A model that does not support the effort runs at its
    /// catalog default. `Thinking::None` sends a Claude request without extended thinking.
    pub(crate) thinking: Thinking,
    /// The reply budget of Claude requests.
    pub(crate) max_tokens: u32,
    pub(crate) timeout: Duration,
}

/// Why a request produced no reply. Messages are written for display and never contain
/// credentials.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub(crate) enum DirectError {
    /// The model cannot be used with the credentials this process has.
    #[error("{0}")]
    Unavailable(String),
    #[error("the model request failed: {0}")]
    Request(String),
    #[error("the model did not answer within {} seconds", .0.as_secs())]
    Timeout(Duration),
}

/// A model with the credentials that can authenticate a request to it.
enum Credentials {
    OpenAi(OpenAiAuth, CodexModel),
    Claude(ClaudeApiKey, ClaudeModel),
}

/// The model a background job uses when none is configured: Luna when OpenAI credentials load,
/// else Haiku when Claude is enabled with an API key.
pub(crate) fn automatic_model(config: &Config) -> Option<Model> {
    if config.auth().load().is_ok() {
        return Some(Model::Codex(CodexModel::Luna));
    }
    claude_key(config).map(|_| Model::Claude(ClaudeModel::Haiku55))
}

/// Checks that a request to `model` could authenticate, without sending one.
pub(crate) fn ensure_available(config: &Config, model: Model) -> Result<(), DirectError> {
    credentials(config, model).map(drop)
}

/// Sends `request` and returns the model's text reply.
pub(crate) async fn complete(
    config: &Config,
    request: &DirectRequest<'_>,
) -> Result<String, DirectError> {
    let credentials = credentials(config, request.model)?;
    tokio::time::timeout(request.timeout, async {
        match credentials {
            Credentials::OpenAi(auth, model) => openai(config, auth, model, request).await,
            Credentials::Claude(key, model) => claude(config, &key, model, request).await,
        }
    })
    .await
    .map_err(|_| DirectError::Timeout(request.timeout))?
}

fn credentials(config: &Config, model: Model) -> Result<Credentials, DirectError> {
    let name = model::name(model);
    match model {
        Model::Codex(codex) => config
            .auth()
            .load()
            .map(|auth| Credentials::OpenAi(auth, codex))
            .map_err(|_| {
                DirectError::Unavailable(format!(
                    "{name} needs OpenAI credentials, and none could be loaded."
                ))
            }),
        Model::Claude(_) if is_access_gated(model) => Err(DirectError::Unavailable(format!(
            "{name} is access-gated and only runs inside sessions."
        ))),
        Model::Claude(_) if !config.claude().enabled() => Err(DirectError::Unavailable(format!(
            "{name} needs [claude] enabled = true in the configuration."
        ))),
        Model::Claude(claude) => claude_key(config)
            .map(|key| Credentials::Claude(key, claude))
            .ok_or_else(|| {
                DirectError::Unavailable(format!(
                    "{name} needs a Claude API key ([claude] api_key or ANTHROPIC_API_KEY)."
                ))
            }),
    }
}

fn claude_key(config: &Config) -> Option<ClaudeApiKey> {
    let config = config.claude();
    if !config.enabled() {
        return None;
    }
    config
        .resolve_api_key(|| SecretString::from_environment("ANTHROPIC_API_KEY"))
        .ok()?
}

async fn openai(
    config: &Config,
    auth: OpenAiAuth,
    model: CodexModel,
    request: &DirectRequest<'_>,
) -> Result<String, DirectError> {
    let failed = |error: &dyn std::fmt::Display| DirectError::Request(error.to_string());
    let mut builder = OpenAi::builder(auth)
        .model(model)
        .thinking(supported(Model::Codex(model), request.thinking))
        .transport(ResponsesTransport::Https)
        .max_attempts(NonZeroU32::MIN);
    if let Some(url) = config.agent().api_base_url() {
        builder = builder.api_base_url(url);
    }
    let mut session = builder
        .build()
        .map_err(|error| failed(&error))?
        .instructions(request.instructions)
        .build()
        .map_err(|error| failed(&error))?;
    let response = session
        .turn()
        .create(request.input)
        .await
        .map_err(|error| failed(&error))?;
    Ok(response.output_text().to_owned())
}

async fn claude(
    config: &Config,
    key: &ClaudeApiKey,
    model: ClaudeModel,
    request: &DirectRequest<'_>,
) -> Result<String, DirectError> {
    let failed = |error: &dyn std::fmt::Display| DirectError::Request(error.to_string());
    let config = config.claude();
    let mut headers = HeaderMap::new();
    if let Some(workspace) = config.workspace_id() {
        headers.insert(
            "anthropic-workspace-id",
            HeaderValue::from_str(workspace).map_err(|error| failed(&error))?,
        );
    }
    let http = reqwest::Client::builder()
        .default_headers(headers)
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .build()
        .map_err(|error| failed(&error))?;
    // The native client retains a non-zeroizing copy of the resolved API key.
    let client = match config.api_base_url() {
        Some(base) => ClaudeClient::new(
            http,
            format!("{}/messages", base.trim_end_matches('/')),
            key.key().expose_secret(),
        ),
        None => ClaudeClient::official(http, key.key().expose_secret()),
    };
    let effort = match request.thinking {
        Thinking::None => None,
        thinking => claude_effort(supported(Model::Claude(model), thinking)),
    };
    let response = client
        .create(&MessagesRequest {
            model: model.as_str().to_owned(),
            max_tokens: request.max_tokens,
            cache_control: None,
            output_config: effort.map(|effort| OutputConfig { effort }),
            speed: None,
            tool_choice: None,
            thinking: effort.map(|_| json!({"type": "adaptive"})),
            context_management: None,
            diagnostics: None,
            system: Some(json!(request.instructions)),
            messages: vec![Message::text(Role::User, request.input)],
            container: None,
            tools: Vec::new(),
        })
        .await
        .map_err(|error| failed(&error))?;
    Ok(response
        .content
        .into_iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" "))
}

/// `thinking` when `model` supports it, else the model's catalog default.
fn supported(model: Model, thinking: Thinking) -> Thinking {
    if model.supports_thinking(thinking) {
        thinking
    } else {
        model.default_thinking()
    }
}

/// The Claude effort for `thinking`, as sessions map it.
const fn claude_effort(thinking: Thinking) -> Option<Effort> {
    match thinking {
        Thinking::None => None,
        Thinking::Low => Some(Effort::Low),
        Thinking::Medium => Some(Effort::Medium),
        Thinking::High => Some(Effort::High),
        Thinking::Xhigh => Some(Effort::Xhigh),
        Thinking::Max => Some(Effort::Max),
    }
}
