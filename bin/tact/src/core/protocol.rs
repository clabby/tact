//! The typed protocol between a session-owning event loop and a remote front-end.
//!
//! The event loop is the only owner of session state. It publishes every change in shared state
//! (see `docs/web.md`) through a [`Publisher`] and applies commands received as [`Request`]s
//! through exactly the same effects a keypress would produce. A remote front-end holds
//! projections of what was published and never mutates session state directly.
//!
//! Commands and queries are serde-tagged enums deserialized straight from request bodies, so a
//! new feature is a new variant plus its loop-side handler, never a new route. Query replies are
//! typed structures owned by the UI-agnostic modules that compute them for the terminal as well.

use crate::{
    app::{
        config::{ConfigDocument, ReasoningEffort, ReasoningMode, Speed},
        model::ModelCatalog,
    },
    core::{
        context::{ContextBudget, ContextDiagnostics},
        extensions::SkillMatches,
        session::{HistoryPage, RecentPromptScope, RecentPrompts},
        subagent_roster::SubagentRoster,
        transcript::TranscriptRecord,
    },
    search::FileMatches,
};
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, sync::Arc};
use tact_memory::{MemoryAccess, MemoryKey, MemoryRecord};
use tact_subagents::{AgentId, AgentMessageUpdate};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

/// Identifies one browser tab so it can ignore the echo of its own draft writes.
pub(crate) type ClientId = u64;

/// The window that caused a change to shared state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Origin {
    Terminal,
    Web(ClientId),
}

/// Immutable facts about a live session that are not derived from its transcript.
#[derive(Clone, Debug)]
pub(crate) struct SessionInfo {
    pub(crate) id: String,
    /// The directory the session's agent runs in.
    pub(crate) workspace: PathBuf,
    /// The model identifier accepted by `app::model::parse`.
    pub(crate) model: String,
    pub(crate) effort: ReasoningEffort,
    pub(crate) reasoning_mode: ReasoningMode,
    /// The requested speed preference; the model may run it at a lower tier (`Speed::for_model`).
    pub(crate) speed: Speed,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Draft {
    /// Increases on every change to the text or images, whichever window made it.
    pub(crate) rev: u64,
    pub(crate) text: String,
    /// Images pasted into the draft, in marker order. Each is shown in `text` as its marker.
    pub(crate) images: Vec<DraftImage>,
}

/// An image attached to a draft. An image belongs to the draft while its marker (for example
/// `[Image #2]`) occurs in the text; a text change that removes the marker removes the image.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DraftImage {
    pub(crate) marker: String,
    pub(crate) data_url: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct QueuedPrompt {
    pub(crate) id: u64,
    pub(crate) text: String,
    /// A steering prompt is delivered into the running turn instead of after it.
    pub(crate) steering: bool,
}

/// In-flight work of one session. A session is running while either count is non-zero.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct Busy {
    pub(crate) turns: usize,
    pub(crate) shells: usize,
}

/// A change to shared state, from the loop to the server.
#[derive(Debug)]
pub(crate) enum Publication {
    /// A pane became live. `records` is the persisted transcript of a resumed session.
    Opened {
        info: SessionInfo,
        records: Vec<Arc<TranscriptRecord>>,
        draft: Draft,
        queue: Vec<QueuedPrompt>,
        busy: Busy,
    },
    Record {
        session: String,
        record: Arc<TranscriptRecord>,
    },
    Closed {
        session: String,
    },
    /// The shared active session, which is the terminal's focused pane.
    Active {
        session: String,
    },
    Draft {
        session: String,
        draft: Draft,
        origin: Origin,
    },
    Queue {
        session: String,
        items: Vec<QueuedPrompt>,
    },
    Settings {
        session: String,
        model: String,
        effort: ReasoningEffort,
        reasoning_mode: ReasoningMode,
        speed: Speed,
    },
    Busy {
        session: String,
        busy: Busy,
    },
    /// The context-window usage the composer shows. Published when either value changes.
    Context {
        session: String,
        budget: ContextBudget,
    },
    /// The subagent tree of a session, the same data the terminal's Subagents overlay renders.
    /// Published whenever an agent is added or changes status, or the process-wide limit changes.
    Subagents {
        session: String,
        roster: SubagentRoster,
    },
    /// A transcript record of one subagent of `session`.
    SubagentRecord {
        session: String,
        agent: AgentId,
        record: Arc<TranscriptRecord>,
    },
    /// A directed message between agents of `session` changed delivery state. Carries the whole
    /// retained thread, as the terminal's transcripts receive it.
    Message {
        session: String,
        update: AgentMessageUpdate,
    },
}

/// The loop's non-blocking sending half. Publishing never fails the loop: a stopped server only
/// means nobody is listening.
#[derive(Clone)]
pub(crate) struct Publisher(mpsc::UnboundedSender<Publication>);

impl Publisher {
    pub(crate) const fn new(sender: mpsc::UnboundedSender<Publication>) -> Self {
        Self(sender)
    }

    pub(crate) fn publish(&self, publication: Publication) {
        drop(self.0.send(publication));
    }
}

/// A command from a browser. Each carries the session it targets where one applies.
///
/// The wire form is `{ "cmd": "<snake_case variant>", "args": { ...fields } }`; unit variants
/// omit `args`. See [`CommandEnvelope`].
#[derive(Debug, Deserialize, Eq, PartialEq)]
#[serde(tag = "cmd", content = "args", rename_all = "snake_case")]
pub(crate) enum Command {
    SetDraft {
        session: String,
        text: String,
    },
    /// Submits the draft provided it is still at `rev`. While a turn runs the prompt steers it, as
    /// the composer's Enter does, or waits in the queue when `queue` is set, as Shift+Tab does.
    Submit {
        session: String,
        rev: u64,
        #[serde(default)]
        queue: bool,
    },
    Interrupt {
        session: String,
    },
    Steer {
        session: String,
        queue_id: u64,
    },
    Dequeue {
        session: String,
        queue_id: u64,
    },
    /// Replaces the text of a queued prompt, as saving a queue edit in the terminal does.
    EditQueued {
        session: String,
        queue_id: u64,
        text: String,
    },
    Compact {
        session: String,
    },
    SetModel {
        session: String,
        model: String,
    },
    SetEffort {
        session: String,
        effort: ReasoningEffort,
    },
    SetReasoningMode {
        session: String,
        mode: ReasoningMode,
    },
    SetSpeed {
        session: String,
        speed: Speed,
    },
    Activate {
        session: String,
    },
    #[serde(rename = "open_session")]
    Open(OpenSpec),
    #[serde(rename = "close_session")]
    Close {
        session: String,
        #[serde(default)]
        force: bool,
    },
    /// Appends an image to the draft as a new marker, like pasting an image into the composer.
    AttachImage {
        session: String,
        data_url: String,
    },
    /// Starts a reflection turn with optional hidden instructions (Actions: Reflection).
    Reflect {
        session: String,
        #[serde(default)]
        instructions: String,
    },
    /// Prepares a handoff and continues in the prepared session (Actions: Prepare handoff).
    Handoff {
        session: String,
    },
    ReloadConfig,
    /// Replaces the configuration file with `text` if it still has `revision` (from
    /// `Query::Config`) and `text` is a valid configuration, then reloads it.
    WriteConfig {
        text: String,
        revision: String,
    },
    DeleteMemory {
        key: MemoryKey,
    },
    /// Sets the process-wide subagent concurrency limit and persists it to the configuration.
    SetMaxSubagents {
        limit: usize,
    },
}

#[derive(Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OpenSpec {
    New {
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        workspace: Option<String>,
    },
    Resume {
        session: String,
    },
    Fork {
        session: String,
    },
}

/// The body of `POST /api/cmd`.
#[derive(Debug, Deserialize)]
pub(crate) struct CommandEnvelope {
    pub(crate) client: ClientId,
    #[serde(flatten)]
    pub(crate) command: Command,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum Reply {
    Done,
    /// The session that `Command::Open` created or activated.
    Opened {
        session: String,
    },
}

/// The wire form of a successful command: `{}` or, for `open_session`, `{ "session": id }`.
impl Serialize for Reply {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap as _;
        match self {
            Self::Done => serializer.serialize_map(Some(0))?.end(),
            Self::Opened { session } => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("session", session)?;
                map.end()
            }
        }
    }
}

/// A read-only request for data computed on demand, answered by the loop. The wire form is
/// `{ "query": "<snake_case variant>", "args": { ...fields } }`, the body of `POST /api/query`.
#[derive(Debug, Deserialize, Eq, PartialEq)]
#[serde(tag = "query", content = "args", rename_all = "snake_case")]
pub(crate) enum Query {
    /// The selectable models with their effort, reasoning-mode, and speed couplings.
    Models,
    /// Persisted sessions, newest first, filtered like the Resume picker.
    History {
        #[serde(default)]
        query: String,
        #[serde(default)]
        cursor: Option<String>,
    },
    /// Workspace paths ranked for an `@` mention.
    Files {
        #[serde(default)]
        query: String,
    },
    /// Skills ranked for a `$` mention.
    Skills {
        #[serde(default)]
        query: String,
    },
    RecentPrompts {
        session: String,
        #[serde(default)]
        scope: RecentPromptScope,
        #[serde(default)]
        query: String,
    },
    ContextDiagnostics {
        session: String,
    },
    Memories,
    /// The configuration file's text, for the in-browser editor.
    Config,
    /// The checkouts of a session's repository. The server answers it without the terminal loop.
    Workspaces {
        #[serde(default)]
        session: Option<String>,
    },
}

/// The data answering a [`Query`]. Serialized as the bare payload of its variant.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum QueryReply {
    Models(ModelCatalog),
    History(HistoryPage),
    Files(FileMatches),
    Skills(SkillMatches),
    RecentPrompts(RecentPrompts),
    ContextDiagnostics(ContextDiagnostics),
    Memories {
        access: MemoryAccess,
        records: Vec<ListedMemory>,
    },
    Config(ConfigDocument),
}

/// A memory as the browser lists it. `deletable` is [`MemoryAccess::can_delete`], the same rule
/// the terminal's memory browser applies.
#[derive(Debug, Serialize)]
pub(crate) struct ListedMemory {
    #[serde(flatten)]
    pub(crate) record: MemoryRecord,
    pub(crate) deletable: bool,
}

pub(crate) struct QueryRequest {
    pub(crate) query: Query,
    pub(crate) reply: oneshot::Sender<Result<QueryReply, CommandError>>,
}

/// Typed refusals of commands and queries. Each maps one-to-one onto a wire error code.
#[derive(Debug, Error, Eq, PartialEq)]
pub(crate) enum CommandError {
    #[error("a turn is running")]
    TurnRunning,
    #[error("the queue is not empty")]
    QueueNotEmpty,
    #[error("nothing is running")]
    NothingRunning,
    #[error("the draft changed")]
    DraftChanged,
    #[error("the session is open in another Tact")]
    SessionLocked,
    #[error("unknown session")]
    UnknownSession,
    #[error("too many live sessions")]
    TooManySessions,
    #[error("not available from the web interface")]
    NotAvailableRemotely,
    /// The target changed since the client read it (the configuration file).
    #[error("changed since it was read")]
    Stale,
    /// The feature is turned off in the configuration (for example memory).
    #[error("{0}")]
    Disabled(String),
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Failed(String),
}

impl CommandError {
    /// The wire error code. HTTP statuses are the server's concern.
    pub(crate) const fn code(&self) -> &'static str {
        match self {
            Self::TurnRunning => "turn_running",
            Self::QueueNotEmpty => "queue_not_empty",
            Self::NothingRunning => "nothing_running",
            Self::DraftChanged => "draft_changed",
            Self::SessionLocked => "session_locked",
            Self::UnknownSession => "unknown_session",
            Self::TooManySessions => "too_many_sessions",
            Self::NotAvailableRemotely => "not_available_remotely",
            Self::Stale => "stale",
            Self::Disabled(_) => "disabled",
            Self::Invalid(_) => "invalid_request",
            Self::Failed(_) => "failed",
        }
    }
}

pub(crate) struct Request {
    pub(crate) command: Command,
    pub(crate) client: ClientId,
    pub(crate) reply: oneshot::Sender<Result<Reply, CommandError>>,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum AuxiliaryError {
    Cancelled,
    Failed(String),
}

/// A one-shot agent prompt in a clean context, run through a session's worker. Overviews,
/// inline questions, and AI review use these.
pub(crate) struct AuxiliaryRequest {
    pub(crate) session: String,
    pub(crate) prompt: String,
    pub(crate) shutdown: CancellationToken,
    pub(crate) completion: oneshot::Sender<Result<String, AuxiliaryError>>,
}

#[cfg(test)]
mod tests {
    use super::{Command, CommandEnvelope, OpenSpec, Query};
    use crate::{
        app::config::{ReasoningMode, Speed},
        core::session::RecentPromptScope,
    };
    use serde_json::json;
    use tact_memory::MemoryKey;

    fn command(body: serde_json::Value) -> (u64, Command) {
        let envelope = serde_json::from_value::<CommandEnvelope>(body).unwrap();
        (envelope.client, envelope.command)
    }

    #[test]
    fn commands_deserialize_from_one_tagged_envelope() {
        assert_eq!(
            command(json!({
                "client": 7,
                "cmd": "set_speed",
                "args": { "session": "s", "speed": "ultrafast" },
            })),
            (
                7,
                Command::SetSpeed {
                    session: "s".to_owned(),
                    speed: Speed::Ultrafast,
                }
            )
        );
        assert_eq!(
            command(json!({ "client": 1, "cmd": "reload_config" })).1,
            Command::ReloadConfig
        );
        assert_eq!(
            command(json!({ "client": 1, "cmd": "open_session", "args": { "new": {} } })).1,
            Command::Open(OpenSpec::New {
                model: None,
                workspace: None
            })
        );
        assert_eq!(
            command(json!({
                "client": 1,
                "cmd": "close_session",
                "args": { "session": "s" },
            }))
            .1,
            Command::Close {
                session: "s".to_owned(),
                force: false,
            }
        );
        assert_eq!(
            command(json!({
                "client": 1,
                "cmd": "set_reasoning_mode",
                "args": { "session": "s", "mode": "pro" },
            }))
            .1,
            Command::SetReasoningMode {
                session: "s".to_owned(),
                mode: ReasoningMode::Pro,
            }
        );
        assert_eq!(
            command(json!({
                "client": 1,
                "cmd": "delete_memory",
                "args": { "key": { "id": 3, "version": 2, "namespace": "team" } },
            }))
            .1,
            Command::DeleteMemory {
                key: serde_json::from_value::<MemoryKey>(
                    json!({ "id": 3, "version": 2, "namespace": "team" })
                )
                .unwrap(),
            }
        );
    }

    #[test]
    fn malformed_commands_are_rejected() {
        for body in [
            json!({ "cmd": "reload_config" }),
            json!({ "client": 1, "cmd": "launch_rockets" }),
            json!({ "client": 1, "cmd": "set_speed", "args": { "session": "s", "speed": "warp" } }),
            json!({ "client": 1, "cmd": "interrupt" }),
        ] {
            assert!(serde_json::from_value::<CommandEnvelope>(body).is_err());
        }
    }

    #[test]
    fn queries_deserialize_with_defaults() {
        assert_eq!(
            serde_json::from_value::<Query>(json!({ "query": "models" })).unwrap(),
            Query::Models
        );
        assert_eq!(
            serde_json::from_value::<Query>(json!({
                "query": "recent_prompts",
                "args": { "session": "s", "scope": "current_session" },
            }))
            .unwrap(),
            Query::RecentPrompts {
                session: "s".to_owned(),
                scope: RecentPromptScope::CurrentSession,
                query: String::new(),
            }
        );
        assert_eq!(
            serde_json::from_value::<Query>(json!({ "query": "history", "args": {} })).unwrap(),
            Query::History {
                query: String::new(),
                cursor: None,
            }
        );
    }
}
