//! Typed channels between the TUI event loop and the web server.
//!
//! The event loop is the only owner of session state. It publishes every change in shared state
//! (see `docs/web.md`) through a [`Publisher`] and applies web commands received as
//! [`Request`]s through exactly the same effects a keypress would produce. The server holds
//! projections of what was published and never mutates session state directly.

use crate::{
    app::config::ReasoningEffort,
    tui::transcript::TranscriptRecord,
};
use std::{path::PathBuf, sync::Arc};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot, watch};
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
    /// The model identifier accepted by `app::model::parse`.
    pub(crate) model: String,
    pub(crate) effort: ReasoningEffort,
    pub(crate) fast_mode: bool,
    pub(crate) workspace: PathBuf,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Draft {
    /// Increases on every change to the text, whichever window made it.
    pub(crate) rev: u64,
    pub(crate) text: String,
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
        fast_mode: bool,
    },
    Busy {
        session: String,
        busy: Busy,
    },
}

/// The loop's non-blocking sending half. Publishing never fails the loop: a stopped server only
/// means nobody is listening.
#[derive(Clone)]
pub(crate) struct Publisher(mpsc::UnboundedSender<Publication>);

impl Publisher {
    pub(crate) fn publish(&self, publication: Publication) {
        drop(self.0.send(publication));
    }
}

/// A command from a browser. Each carries the session it targets where one applies.
#[derive(Debug)]
pub(crate) enum Command {
    SetDraft { session: String, text: String },
    /// Submits the draft exactly as the composer's Enter would, provided it is still at `rev`.
    Submit { session: String, rev: u64 },
    Interrupt { session: String },
    Steer { session: String, queue_id: u64 },
    Dequeue { session: String, queue_id: u64 },
    Compact { session: String },
    SetModel { session: String, model: String },
    SetEffort { session: String, effort: ReasoningEffort },
    SetFast { session: String, enabled: bool },
    Activate { session: String },
    Open(OpenSpec),
    Close { session: String, force: bool },
}

#[derive(Debug)]
pub(crate) enum OpenSpec {
    New { model: Option<String> },
    Resume { session: String },
    Fork { session: String },
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum Reply {
    Done,
    /// The session that `Command::Open` created or activated.
    Opened { session: String },
}

/// Typed refusals. Each maps one-to-one onto a wire error code.
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
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Failed(String),
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

/// What the TUI shows for the web interface.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum WebStatus {
    Starting,
    /// `url` embeds the login credential in its fragment and must only be shown on request.
    Ready { url: String },
    /// The web interface bundle is not installed; opening it needs the user's consent to download.
    AssetsMissing,
    Unavailable { reason: String },
}

/// The loop's half of the bridge.
pub(crate) struct LoopEnd {
    pub(crate) publisher: Publisher,
    pub(crate) requests: mpsc::UnboundedReceiver<Request>,
    pub(crate) auxiliary: mpsc::UnboundedReceiver<AuxiliaryRequest>,
    pub(crate) status: watch::Sender<WebStatus>,
}

/// The server's half of the bridge.
pub(crate) struct WebEnd {
    pub(crate) publications: mpsc::UnboundedReceiver<Publication>,
    pub(crate) requests: mpsc::UnboundedSender<Request>,
    pub(crate) auxiliary: mpsc::UnboundedSender<AuxiliaryRequest>,
    pub(crate) status: watch::Receiver<WebStatus>,
}

pub(crate) fn bridge() -> (LoopEnd, WebEnd) {
    let (publish, publications) = mpsc::unbounded_channel();
    let (requests_tx, requests) = mpsc::unbounded_channel();
    let (auxiliary_tx, auxiliary) = mpsc::unbounded_channel();
    let (status_tx, status) = watch::channel(WebStatus::Starting);
    (
        LoopEnd {
            publisher: Publisher(publish),
            requests,
            auxiliary,
            status: status_tx,
        },
        WebEnd {
            publications,
            requests: requests_tx,
            auxiliary: auxiliary_tx,
            status,
        },
    )
}
