//! The server's projection of every live session, and its fan-out to browsers.
//!
//! The terminal event loop is the only writer of session state. It publishes changes through the
//! bridge; the hub folds them into per-session projections and turns the result into
//! Server-Sent Events. Browsers follow the shared active session: they receive a snapshot of it on
//! connect and on every activation, and incremental events afterwards.
//!
//! Publications are applied immediately but delivered at most once per [`FLUSH_INTERVAL`], so a
//! streaming response costs one event per frame no matter how many records produced it. Each event
//! is serialized once and shared by every client. A client whose buffer fills up is dropped; it
//! reconnects and starts from a fresh snapshot, so a slow browser can never stall the hub.

use super::{
    bridge::{Busy, Draft, Origin, Publication, QueuedPrompt, SessionInfo},
    wire::{
        Frame, PROTOCOL_VERSION, SessionSnapshot, SessionSummary, SummaryState, ToolDetail,
        WireDraft, WireEntry, WireQueued, WireStatus, effort_name, frame, origin_label,
    },
};
use crate::{
    app::config::ReasoningEffort,
    tui::transcript::{EntryKind, TranscriptModel, TranscriptRecord, TransientStatus},
};
use serde::Serialize;
use std::{
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::mpsc::{self, UnboundedReceiver},
    time::{Instant, sleep_until},
};
use tokio_util::sync::CancellationToken;

const FLUSH_INTERVAL: Duration = Duration::from_nanos(8_333_334);
/// Frames buffered per client before it is considered too slow and dropped.
const CLIENT_BUFFER: usize = 512;
const MAX_STREAMS: usize = 32;
const MAX_TITLE_CHARS: usize = 80;

/// A cheap handle onto the shared projection.
#[derive(Clone)]
pub(super) struct Hub {
    shared: Arc<Shared>,
}

struct Shared {
    state: Mutex<State>,
    any_busy: AtomicBool,
    next_client: AtomicU64,
}

/// A stream of frames for one browser tab. Dropping it unsubscribes.
pub(super) struct Subscription {
    pub(super) frames: mpsc::Receiver<Frame>,
}

/// The reason a subscription was refused.
#[derive(Debug, Eq, PartialEq)]
pub(super) struct TooManyStreams;

#[derive(Default)]
struct State {
    sessions: Vec<Live>,
    active: Option<String>,
    clients: Vec<mpsc::Sender<Frame>>,
    /// What every connected client has already been told; `None` while nobody is connected.
    sent: Option<Sent>,
    closed: Vec<String>,
}

struct Live {
    info: SessionInfo,
    model: TranscriptModel,
    draft: Draft,
    draft_origin: Origin,
    queue: Vec<QueuedPrompt>,
    busy: Busy,
    unread: bool,
    last_activity_unix_ms: u64,
    title: Option<String>,
    /// Cancelled when the session closes so work running on its behalf stops.
    closed: CancellationToken,
    /// The transcript changed shape (an entry vanished or moved) and clients need a new snapshot.
    reshaped: bool,
}

/// The state of the active session as clients last saw it.
struct Sent {
    summaries: Vec<SessionSummary>,
    active: Option<ActiveSent>,
}

struct ActiveSent {
    session: String,
    entries: Vec<(usize, u64)>,
    draft_rev: u64,
    queue: Vec<QueuedPrompt>,
    settings: Settings,
    status: Option<WireStatus>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Settings {
    model: String,
    effort: ReasoningEffort,
    fast_mode: bool,
}

impl Hub {
    /// Starts folding publications into the projection until `shutdown` is cancelled or the
    /// terminal loop stops publishing.
    pub(super) fn spawn(
        mut publications: UnboundedReceiver<Publication>,
        shutdown: CancellationToken,
    ) -> Self {
        let hub = Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State::default()),
                any_busy: AtomicBool::new(false),
                next_client: AtomicU64::new(1),
            }),
        };
        let task_hub = hub.clone();
        tokio::spawn(async move {
            let mut flush_at: Option<Instant> = None;
            loop {
                tokio::select! {
                    biased;
                    () = shutdown.cancelled() => break,
                    publication = publications.recv() => {
                        let Some(publication) = publication else { break };
                        task_hub.apply(publication);
                        flush_at.get_or_insert_with(|| Instant::now() + FLUSH_INTERVAL);
                    }
                    () = async { sleep_until(flush_at.expect("guarded by the branch condition")).await }, if flush_at.is_some() => {
                        flush_at = None;
                        task_hub.flush();
                    }
                }
            }
            task_hub.close_all();
        });
        hub
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // The critical sections never panic while holding the lock; recover the data regardless.
        self.shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// True while any live session has a turn or shell in flight.
    pub(super) fn any_busy(&self) -> bool {
        self.shared.any_busy.load(Ordering::Acquire)
    }

    /// The number of live sessions and whether any is running.
    pub(super) fn counts(&self) -> (usize, bool) {
        (self.state().sessions.len(), self.any_busy())
    }

    pub(super) fn connected_clients(&self) -> usize {
        self.state()
            .clients
            .iter()
            .filter(|client| !client.is_closed())
            .count()
    }

    /// A token that is cancelled when `session` closes; `None` if it is not live.
    pub(super) fn closed_token(&self, session: &str) -> Option<CancellationToken> {
        self.state()
            .sessions
            .iter()
            .find(|live| live.info.id == session)
            .map(|live| live.closed.clone())
    }

    pub(super) fn entry_detail(&self, session: &str, entry: usize) -> Option<ToolDetail> {
        let state = self.state();
        let live = state.sessions.iter().find(|live| live.info.id == session)?;
        let entry = live
            .model
            .entries()
            .iter()
            .find(|candidate| candidate.id.index() == entry)?;
        match &entry.kind {
            EntryKind::Tool(tool) => Some(ToolDetail::new(tool)),
            _ => None,
        }
    }

    pub(super) fn is_live(&self, session: &str) -> bool {
        self.state()
            .sessions
            .iter()
            .any(|live| live.info.id == session)
    }

    /// Registers a client and queues the connect sequence: `hello`, `live`, `active`, `snapshot`.
    pub(super) fn subscribe(&self) -> Result<Subscription, TooManyStreams> {
        let client = self.shared.next_client.fetch_add(1, Ordering::Relaxed);
        let (sender, frames) = mpsc::channel(CLIENT_BUFFER);
        let mut state = self.state();
        state.clients.retain(|client| !client.is_closed());
        if state.clients.len() >= MAX_STREAMS {
            return Err(TooManyStreams);
        }
        let queued = [
            Some(frame(
                "hello",
                &Hello {
                    protocol_version: PROTOCOL_VERSION,
                    client_hint: client,
                },
            )),
            Some(frame("live", &state.live_event())),
            state.active_event(),
            state.active_snapshot(),
        ];
        for frame in queued.into_iter().flatten() {
            sender
                .try_send(frame)
                .expect("the connect sequence fits in an empty buffer");
        }
        if state.clients.is_empty() {
            state.sent = Some(state.baseline());
        }
        state.clients.push(sender);
        Ok(Subscription { frames })
    }

    /// Broadcasts an event outside the publication flow, such as a workspace change.
    pub(super) fn broadcast(&self, event: &str, data: &impl Serialize) {
        self.state().broadcast(&frame(event, data));
    }

    fn apply(&self, publication: Publication) {
        let mut state = self.state();
        state.apply(publication);
        self.shared.any_busy.store(
            state
                .sessions
                .iter()
                .any(|live| live.busy.turns > 0 || live.busy.shells > 0),
            Ordering::Release,
        );
    }

    fn flush(&self) {
        self.state().flush();
    }

    fn close_all(&self) {
        let mut state = self.state();
        state.clients.clear();
        for live in &state.sessions {
            live.closed.cancel();
        }
    }
}

#[derive(Serialize)]
struct Hello {
    protocol_version: u32,
    client_hint: u64,
}

#[derive(Serialize)]
struct LiveEvent {
    active: Option<String>,
    sessions: Vec<SessionSummary>,
}

#[derive(Serialize)]
struct SessionRef<'a> {
    session: &'a str,
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
}

impl Live {
    fn running(&self) -> bool {
        self.busy.turns > 0 || self.busy.shells > 0
    }

    fn settings(&self) -> Settings {
        Settings {
            model: self.info.model.clone(),
            effort: self.info.effort,
            fast_mode: self.info.fast_mode,
        }
    }

    fn title(&mut self) -> String {
        if self.title.is_none() {
            self.title = self.model.entries().iter().find_map(|entry| match &entry.kind {
                EntryKind::User { text } => Some(title_from(text)),
                _ => None,
            });
        }
        self.title.clone().unwrap_or_else(|| "New chat".to_owned())
    }

    fn errored(&self) -> bool {
        matches!(self.model.transient(), Some(TransientStatus::Error(_)))
            || self
                .model
                .entries()
                .iter()
                .rev()
                .find(|entry| !entry.hidden)
                .is_some_and(|entry| matches!(entry.kind, EntryKind::Error { .. }))
    }

    fn summary(&mut self) -> SessionSummary {
        let state = if self.running() {
            SummaryState::Running
        } else if self.errored() {
            SummaryState::Error
        } else {
            SummaryState::Idle
        };
        SessionSummary {
            id: self.info.id.clone(),
            title: self.title(),
            model: self.info.model.clone(),
            state,
            unread: self.unread,
            has_draft: !self.draft.text.is_empty(),
            last_activity_unix_ms: self.last_activity_unix_ms,
        }
    }

    fn visible(&self) -> impl Iterator<Item = &crate::tui::transcript::TranscriptEntry> {
        self.model.entries().iter().filter(|entry| !entry.hidden)
    }

    fn snapshot(&mut self) -> SessionSnapshot {
        SessionSnapshot {
            session: self.info.id.clone(),
            title: self.title(),
            model: self.info.model.clone(),
            effort: effort_name(self.info.effort),
            fast_mode: self.info.fast_mode,
            entries: self.visible().map(WireEntry::new).collect(),
            status: self.model.transient().map(WireStatus::from),
            queue: self.queue.iter().map(WireQueued::from).collect(),
            draft: WireDraft {
                rev: self.draft.rev,
                text: super::wire::cap(&self.draft.text),
            },
            running: self.running(),
        }
    }

    fn active_sent(&self) -> ActiveSent {
        ActiveSent {
            session: self.info.id.clone(),
            entries: self
                .visible()
                .map(|entry| (entry.id.index(), entry.revision))
                .collect(),
            draft_rev: self.draft.rev,
            queue: self.queue.clone(),
            settings: self.settings(),
            status: self.model.transient().map(WireStatus::from),
        }
    }
}

fn title_from(prompt: &str) -> String {
    let line = prompt.lines().find(|line| !line.trim().is_empty()).unwrap_or_default().trim();
    let mut chars = line.chars();
    let head = chars.by_ref().take(MAX_TITLE_CHARS).collect::<String>();
    if chars.next().is_some() {
        return format!("{}…", head.trim_end());
    }
    head
}

impl State {
    fn live_mut(&mut self, session: &str) -> Option<&mut Live> {
        self.sessions.iter_mut().find(|live| live.info.id == session)
    }

    fn apply(&mut self, publication: Publication) {
        match publication {
            Publication::Opened {
                info,
                records,
                draft,
                queue,
                busy,
            } => {
                let mut model = TranscriptModel::default();
                for record in &records {
                    model.apply(record);
                }
                let last_activity_unix_ms = records
                    .last()
                    .map_or_else(now_unix_ms, |record| record.recorded_at_unix_ms());
                let live = Live {
                    info,
                    model,
                    draft,
                    draft_origin: Origin::Terminal,
                    queue,
                    busy,
                    unread: false,
                    last_activity_unix_ms,
                    title: None,
                    closed: CancellationToken::new(),
                    reshaped: false,
                };
                match self.live_mut(&live.info.id) {
                    Some(existing) => {
                        existing.closed.cancel();
                        *existing = live;
                    }
                    None => self.sessions.push(live),
                }
            }
            Publication::Record { session, record } => {
                let Some(live) = self.live_mut(&session) else {
                    return;
                };
                apply_record(live, &record);
            }
            Publication::Closed { session } => {
                let Some(index) = self.sessions.iter().position(|live| live.info.id == session)
                else {
                    return;
                };
                self.sessions.remove(index).closed.cancel();
                if self.active.as_deref() == Some(session.as_str()) {
                    self.active = None;
                }
                self.closed.push(session);
            }
            Publication::Active { session } => {
                if let Some(live) = self.live_mut(&session) {
                    live.unread = false;
                    self.active = Some(session);
                }
            }
            Publication::Draft {
                session,
                draft,
                origin,
            } => {
                if let Some(live) = self.live_mut(&session) {
                    live.draft = draft;
                    live.draft_origin = origin;
                }
            }
            Publication::Queue { session, items } => {
                if let Some(live) = self.live_mut(&session) {
                    live.queue = items;
                }
            }
            Publication::Settings {
                session,
                model,
                effort,
                fast_mode,
            } => {
                if let Some(live) = self.live_mut(&session) {
                    live.info.model = model;
                    live.info.effort = effort;
                    live.info.fast_mode = fast_mode;
                }
            }
            Publication::Busy { session, busy } => {
                let active = self.active.as_deref() == Some(session.as_str());
                if let Some(live) = self.live_mut(&session) {
                    let was_running = live.running();
                    live.busy = busy;
                    if was_running && !live.running() && !active {
                        live.unread = true;
                    }
                }
            }
        }
    }

    fn live_event(&mut self) -> LiveEvent {
        LiveEvent {
            active: self.active.clone(),
            sessions: self.sessions.iter_mut().map(Live::summary).collect(),
        }
    }

    fn active_event(&self) -> Option<Frame> {
        self.active
            .as_deref()
            .map(|session| frame("active", &SessionRef { session }))
    }

    fn active_snapshot(&mut self) -> Option<Frame> {
        let active = self.active.clone()?;
        let live = self.live_mut(&active)?;
        Some(frame("snapshot", &live.snapshot()))
    }

    /// What a client holds right after the connect sequence.
    fn baseline(&mut self) -> Sent {
        let summaries = self.sessions.iter_mut().map(Live::summary).collect();
        let active = self.active.clone();
        Sent {
            summaries,
            active: active
                .and_then(|active| self.live_mut(&active).map(|live| live.active_sent())),
        }
    }

    /// Sends to every client, dropping those whose buffer is full or whose stream ended.
    fn broadcast(&mut self, frame: &Frame) {
        self.clients
            .retain(|client| client.try_send(Arc::clone(frame)).is_ok());
    }

    fn flush(&mut self) {
        let closed = std::mem::take(&mut self.closed);
        self.clients.retain(|client| !client.is_closed());
        if self.clients.is_empty() {
            self.sent = None;
            for live in &mut self.sessions {
                live.reshaped = false;
            }
            return;
        }
        let mut sent = self.sent.take().unwrap_or_else(|| self.baseline());
        for session in &closed {
            self.broadcast(&frame("closed", &SessionRef { session }));
        }

        let summaries: Vec<_> = self.sessions.iter_mut().map(Live::summary).collect();
        let active = self.active.clone();
        let activated = sent.active.as_ref().map(|sent| &sent.session) != active.as_ref();
        if summaries != sent.summaries || activated {
            self.broadcast(&frame(
                "live",
                &LiveEvent {
                    active: active.clone(),
                    sessions: summaries.clone(),
                },
            ));
            sent.summaries = summaries;
        }

        let Some(active) = active else {
            sent.active = None;
            self.sent = Some(sent);
            return;
        };
        let Some(live) = self.live_mut(&active) else {
            sent.active = None;
            self.sent = Some(sent);
            return;
        };
        let mut frames = Vec::new();
        let previous = sent.active.take().filter(|previous| previous.session == active);
        if let Some(previous) = previous.filter(|_| !live.reshaped) {
            diff_active(live, &previous, &mut frames);
        } else {
            frames.push(frame("active", &SessionRef { session: &active }));
            frames.push(frame("snapshot", &live.snapshot()));
        }
        live.reshaped = false;
        sent.active = Some(live.active_sent());
        for frame in &frames {
            self.broadcast(frame);
        }
        self.sent = Some(sent);
    }
}

fn apply_record(live: &mut Live, record: &Arc<TranscriptRecord>) {
    let change = live.model.apply(record);
    live.last_activity_unix_ms = live.last_activity_unix_ms.max(record.recorded_at_unix_ms());
    if change.removed.is_some() {
        live.reshaped = true;
    }
}

/// Appends one event per piece of the active session that differs from `previous`.
fn diff_active(live: &mut Live, previous: &ActiveSent, frames: &mut Vec<Frame>) {
    #[derive(Serialize)]
    struct Settings<'a> {
        session: &'a str,
        model: &'a str,
        effort: &'static str,
        fast_mode: bool,
    }
    #[derive(Serialize)]
    struct EntryEvent<'a> {
        session: &'a str,
        entry: WireEntry,
    }
    #[derive(Serialize)]
    struct StatusEvent<'a> {
        session: &'a str,
        status: Option<WireStatus>,
    }
    #[derive(Serialize)]
    struct QueueEvent<'a> {
        session: &'a str,
        items: Vec<WireQueued>,
    }
    #[derive(Serialize)]
    struct DraftEvent<'a> {
        session: &'a str,
        rev: u64,
        text: String,
        origin: String,
    }

    let session = live.info.id.clone();
    let session = session.as_str();
    if live.settings() != previous.settings {
        frames.push(frame(
            "settings",
            &Settings {
                session,
                model: &live.info.model,
                effort: effort_name(live.info.effort),
                fast_mode: live.info.fast_mode,
            },
        ));
    }
    if live.draft.rev != previous.draft_rev {
        frames.push(frame(
            "draft",
            &DraftEvent {
                session,
                rev: live.draft.rev,
                text: super::wire::cap(&live.draft.text),
                origin: origin_label(live.draft_origin),
            },
        ));
    }
    if live.queue != previous.queue {
        frames.push(frame(
            "queue",
            &QueueEvent {
                session,
                items: live.queue.iter().map(WireQueued::from).collect(),
            },
        ));
    }
    let status = live.model.transient().map(WireStatus::from);
    if status != previous.status {
        frames.push(frame("status", &StatusEvent { session, status }));
    }

    let current: Vec<_> = live.visible().collect();
    let append_only = current.len() >= previous.entries.len()
        && previous
            .entries
            .iter()
            .zip(&current)
            .all(|((id, _), entry)| *id == entry.id.index());
    if !append_only {
        frames.push(frame("snapshot", &live.snapshot()));
        return;
    }
    for (index, entry) in current.iter().enumerate() {
        let unchanged = previous
            .entries
            .get(index)
            .is_some_and(|(_, revision)| *revision == entry.revision);
        if !unchanged {
            frames.push(frame(
                "entry",
                &EntryEvent {
                    session,
                    entry: WireEntry::new(entry),
                },
            ));
        }
    }
}

