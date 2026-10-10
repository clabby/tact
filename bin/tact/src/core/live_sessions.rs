//! Cross-session messaging between the live sessions one process runs.
//!
//! A pane's session registers here while it is open. The `message_session` tool looks the target
//! up in this process-wide registry and forwards the message to the front-end event loop, which
//! owns the pane and decides how the worker delivers it. Every delivery steers: it joins the
//! target's running turn, or starts a turn with the message when the target is idle. Sessions
//! live in other processes are visible through `SessionLock` but cannot be messaged.

use nanocodex::{
    Tool,
    tools::contract::{
        ToolContext, ToolDefinition, ToolInput, ToolOutput, ToolResult, async_trait,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io,
    sync::{LazyLock, Mutex, MutexGuard, PoisonError},
};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

const MAX_MESSAGE_BYTES: usize = 16 * 1024;
const SHORT_ID_CHARS: usize = 8;

type Hosts = HashMap<String, mpsc::UnboundedSender<LiveSessionMessage>>;

static LIVE_SESSIONS: LazyLock<Mutex<Hosts>> = LazyLock::new(Mutex::default);

fn registry() -> MutexGuard<'static, Hosts> {
    LIVE_SESSIONS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether the session is open in a pane of this process, which makes it messageable.
pub(crate) fn is_live_here(session_id: &str) -> bool {
    registry().contains_key(session_id)
}

/// Keeps a session addressable by `message_session` until dropped.
pub(crate) struct LiveSessionRegistration {
    session_id: String,
}

impl LiveSessionRegistration {
    /// Routes messages for `session_id` to `sender`. A session is open in at most one pane, so
    /// the identifier is unique while registered.
    pub(crate) fn new(session_id: &str, sender: mpsc::UnboundedSender<LiveSessionMessage>) -> Self {
        registry().insert(session_id.to_owned(), sender);
        Self {
            session_id: session_id.to_owned(),
        }
    }
}

impl Drop for LiveSessionRegistration {
    fn drop(&mut self) {
        registry().remove(&self.session_id);
    }
}

/// A message addressed to a live session, with the channel that reports how it was delivered.
pub(crate) struct LiveSessionMessage {
    pub(crate) from: String,
    pub(crate) target: String,
    pub(crate) text: String,
    pub(crate) reply: oneshot::Sender<Result<Delivery, DeliveryError>>,
}

impl LiveSessionMessage {
    /// The prompt the target's agent receives, which names the sending session.
    pub(crate) fn prompt(&self) -> String {
        format!("Message from session {}:\n\n{}", self.from, self.text)
    }
}

/// How the target took a message.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Delivery {
    /// The message joined the target's running turn.
    Steered,
    /// The target was idle, so the message started a new turn.
    Started,
}

#[derive(Debug, Error)]
pub(crate) enum DeliveryError {
    #[error("session {0} is not live in this Tact process")]
    NotLive(String),
    #[error("the message was rejected: {0}")]
    Rejected(String),
    #[error("the session host stopped before delivering the message")]
    HostStopped,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MessageSessionInput {
    session_id: String,
    message: String,
}

pub(crate) struct MessageSessionTool;

#[async_trait]
impl Tool for MessageSessionTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            "message_session",
            "Sends a message to another session that is live in this Tact process (find_sessions reports `messageable`). Use this tool only when the user explicitly asks you to message or steer another session; never use it on your own initiative. The message always steers: it joins the target's running turn at its next safe boundary, or starts a new turn when the target is idle. Use it to watch and orchestrate other sessions; read their progress with read_session. Delivery is not a reply, and the target is told which session sent the message.",
            json!({
                "type": "object",
                "properties": {
                    "session_id": {
                        "type": "string",
                        "description": "The live target session, from find_sessions."
                    },
                    "message": {
                        "type": "string",
                        "description": "The message, at most 16384 bytes."
                    }
                },
                "required": ["session_id", "message"],
                "additionalProperties": false
            }),
        )
        .with_output_schema(json!({
            "type": "object",
            "properties": {
                "session_id": {"type": "string"},
                "delivery": {"type": "string", "enum": ["steered", "started"]}
            },
            "required": ["session_id", "delivery"],
            "additionalProperties": false
        }))
    }

    async fn execute(&self, input: ToolInput, context: ToolContext<'_>) -> ToolResult {
        let MessageSessionInput {
            session_id: target,
            message,
        } = input.decode_json()?;
        if message.trim().is_empty() || message.len() > MAX_MESSAGE_BYTES {
            return Err(io::Error::other("message must contain 1 to 16384 bytes").into());
        }
        let sender = context.session_id();
        if target == sender {
            return Err(io::Error::other("a session cannot message itself").into());
        }
        let host = registry().get(&target).cloned();
        let host = host.ok_or_else(|| DeliveryError::NotLive(target.clone()))?;
        let (reply, delivered) = oneshot::channel();
        host.send(LiveSessionMessage {
            from: sender.to_owned(),
            target: target.clone(),
            text: message,
            reply,
        })
        .map_err(|_| DeliveryError::HostStopped)?;
        let delivery = delivered.await.map_err(|_| DeliveryError::HostStopped)??;
        Ok(ToolOutput::from_json(
            json!({"session_id": target, "delivery": delivery}),
            true,
        ))
    }
}

/// The leading characters of a session identifier, enough to tell sessions apart in a transcript.
pub(crate) fn short_session_id(session_id: &str) -> String {
    session_id.chars().take(SHORT_ID_CHARS).collect()
}

/// The `me → recipient` label of a `message_session` call for transcript rows. Only the session
/// that owns a transcript sends the calls it shows, so the sender is always the reader.
pub(crate) fn route_label(arguments: &Value) -> String {
    let recipient = arguments
        .get("session_id")
        .and_then(Value::as_str)
        .map_or("unknown session".to_owned(), short_session_id);
    format!("me → {recipient}")
}

/// The `me → recipient` label of a `send_agent_message` call for transcript rows, for the same
/// reason as [`route_label`].
pub(crate) fn agent_route_label(arguments: &Value) -> String {
    let recipient = arguments
        .get("agent_id")
        .and_then(Value::as_u64)
        .map_or("unknown agent".to_owned(), |id| format!("#{id}"));
    format!("me → {recipient}")
}

#[cfg(test)]
mod tests {
    use super::{agent_route_label, route_label};
    use serde_json::json;

    #[test]
    fn routes_start_at_the_transcript_owner() {
        assert_eq!(
            route_label(&json!({"session_id": "9f3c1a7e-52d4"})),
            "me → 9f3c1a7e"
        );
        assert_eq!(route_label(&json!({})), "me → unknown session");
        assert_eq!(agent_route_label(&json!({"agent_id": 2})), "me → #2");
        assert_eq!(agent_route_label(&json!({})), "me → unknown agent");
    }
}
