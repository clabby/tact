//! The server's projection of every live session, and its fan-out to browsers.
//!
//! The terminal event loop is the only writer of session state. It publishes changes through the
//! bridge; the hub folds them into per-session projections and turns the result into
//! Server-Sent Events. Browsers follow the shared active session: they receive a snapshot of it on
//! connect and on every activation, and incremental events afterwards.
//!
//! Publications are applied immediately but delivered at most once per [`FLUSH_INTERVAL`], so a
//! streaming response costs one event per frame no matter how many records produced it. Each event
//! is serialized once into a shared buffer, and every client receives a reference to it. A client
//! whose buffer fills up is dropped; it reconnects and starts from a fresh snapshot, so a slow
//! browser can never stall the hub.
//!
//! All clients share one record of what they have been told, so a flush computes each event once.
//! A client that connects between flushes gets a snapshot that already includes changes the others
//! have not been sent yet, and then receives those changes again at the next flush. Browsers
//! upsert entries by id and revision, so the repeated events are harmless.
//!
//! The projection sits behind one synchronous lock. Every critical section is short and never
//! awaits; events are serialized inside it so they reflect a single consistent state.

use super::wire::{
    Frame, PROTOCOL_VERSION, SessionSnapshot, SessionSummary, SummaryState, ToolDetail, WireDraft,
    WireEntry, WireImage, WireQueued, WireStatus, frame, origin_label,
};
use crate::{
    app::config::{ReasoningEffort, ReasoningMode, Speed},
    core::{
        context::ContextBudget,
        protocol::{Busy, Draft, Origin, Publication, QueuedPrompt, SessionInfo},
        transcript::{
            EntryKind, TranscriptEntry, TranscriptModel, TranscriptRecord, TransientStatus,
        },
    },
};
use base64::{Engine as _, prelude::BASE64_STANDARD};
use serde::Serialize;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tact_subagents::{AgentId, AgentMessageUpdate, MessageSender, SubagentRoster};
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
    context: Option<ContextBudget>,
    roster: SubagentRoster,
    /// The transcript of each subagent, projected like the session's own.
    agents: HashMap<AgentId, TranscriptModel>,
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
    context: Option<ContextBudget>,
    roster: SubagentRoster,
    agent_entries: HashMap<AgentId, Vec<(usize, u64)>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Settings {
    model: String,
    effort: ReasoningEffort,
    reasoning_mode: ReasoningMode,
    speed: Speed,
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

    /// The directory the session's agent runs in.
    pub(super) fn session_workspace(&self, session: &str) -> Option<PathBuf> {
        Some(self.state().live(session)?.info.workspace.clone())
    }

    /// The directories of every live session.
    pub(super) fn live_workspaces(&self) -> Vec<PathBuf> {
        self.state()
            .sessions
            .iter()
            .map(|live| live.info.workspace.clone())
            .collect()
    }

    /// The arguments of the most recent tool calls of a session and its subagents, as one text.
    /// Clients use it to tell which checkouts the agents are working in.
    pub(super) fn recent_tool_arguments(&self, session: &str) -> Option<String> {
        const RECENT_TOOL_CALLS: usize = 80;
        let state = self.state();
        let live = state.live(session)?;
        let mut text = String::new();
        for model in std::iter::once(&live.model).chain(live.agents.values()) {
            let tools = model
                .entries()
                .iter()
                .filter_map(|entry| match &entry.kind {
                    EntryKind::Tool(tool) => Some(tool),
                    _ => None,
                });
            for tool in tools.rev().take(RECENT_TOOL_CALLS) {
                text.push_str(&tool.arguments.to_string());
                text.push('\n');
            }
        }
        Some(text)
    }

    pub(super) fn entry_detail(&self, session: &str, entry: usize) -> Option<ToolDetail> {
        let state = self.state();
        tool_detail(&state.live(session)?.model, entry)
    }

    /// The bytes and media type of the `index`-th image attached to a user entry.
    pub(super) fn user_image(
        &self,
        session: &str,
        entry: usize,
        index: usize,
    ) -> Option<(&'static str, Vec<u8>)> {
        let state = self.state();
        user_image(&state.live(session)?.model, entry, index)
    }

    /// The projected transcript of one subagent of `session`.
    pub(super) fn agent_entries(&self, session: &str, agent: AgentId) -> Option<Vec<WireEntry>> {
        let state = self.state();
        let model = state.live(session)?.agents.get(&agent)?;
        Some(visible(model).map(WireEntry::new).collect())
    }

    pub(super) fn agent_entry_detail(
        &self,
        session: &str,
        agent: AgentId,
        entry: usize,
    ) -> Option<ToolDetail> {
        let state = self.state();
        tool_detail(state.live(session)?.agents.get(&agent)?, entry)
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

pub(super) fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

impl Live {
    fn running(&self) -> bool {
        self.busy.turns > 0 || self.busy.shells > 0
    }

    fn settings(&self) -> Settings {
        Settings {
            model: self.info.model.clone(),
            effort: self.info.effort,
            reasoning_mode: self.info.reasoning_mode,
            speed: self.info.speed,
        }
    }

    fn title(&mut self) -> String {
        if self.title.is_none() {
            self.title = self
                .model
                .entries()
                .iter()
                .find_map(|entry| match &entry.kind {
                    EntryKind::User { text, .. } => Some(title_from(text)),
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
            workspace: self.info.workspace.to_string_lossy().into_owned(),
            state,
            unread: self.unread,
            has_draft: !self.draft.text.is_empty(),
            last_activity_unix_ms: self.last_activity_unix_ms,
        }
    }

    fn visible(&self) -> impl Iterator<Item = &TranscriptEntry> {
        visible(&self.model)
    }

    fn snapshot(&mut self) -> SessionSnapshot {
        SessionSnapshot {
            session: self.info.id.clone(),
            title: self.title(),
            model: self.info.model.clone(),
            workspace: self.info.workspace.to_string_lossy().into_owned(),
            effort: self.info.effort,
            reasoning_mode: self.info.reasoning_mode,
            speed: self.info.speed,
            entries: self.visible().map(WireEntry::new).collect(),
            status: self.model.transient().map(WireStatus::from),
            queue: self.queue.iter().map(WireQueued::from).collect(),
            draft: WireDraft {
                rev: self.draft.rev,
                text: super::wire::cap(&self.draft.text),
                images: self.draft.images.iter().map(WireImage::from).collect(),
            },
            running: self.running(),
            context: self.context,
            subagents: self.roster.clone(),
        }
    }

    fn active_sent(&self) -> ActiveSent {
        ActiveSent {
            session: self.info.id.clone(),
            entries: revisions(self.visible()),
            draft_rev: self.draft.rev,
            queue: self.queue.clone(),
            settings: self.settings(),
            status: self.model.transient().map(WireStatus::from),
            context: self.context,
            roster: self.roster.clone(),
            agent_entries: self
                .agents
                .iter()
                .map(|(agent, model)| (*agent, revisions(visible(model))))
                .collect(),
        }
    }
}

fn visible(model: &TranscriptModel) -> impl Iterator<Item = &TranscriptEntry> {
    model.entries().iter().filter(|entry| !entry.hidden)
}

fn revisions<'a>(entries: impl Iterator<Item = &'a TranscriptEntry>) -> Vec<(usize, u64)> {
    entries
        .map(|entry| (entry.id.index(), entry.revision))
        .collect()
}

fn tool_detail(model: &TranscriptModel, entry: usize) -> Option<ToolDetail> {
    let entry = model
        .entries()
        .iter()
        .find(|candidate| candidate.id.index() == entry)?;
    match &entry.kind {
        EntryKind::Tool(tool) => Some(ToolDetail::new(tool)),
        _ => None,
    }
}

/// Decodes an attached image. Only raster formats a browser renders inertly are served.
fn user_image(
    model: &TranscriptModel,
    entry: usize,
    index: usize,
) -> Option<(&'static str, Vec<u8>)> {
    let entry = model
        .entries()
        .iter()
        .find(|candidate| candidate.id.index() == entry)?;
    let EntryKind::User { images, .. } = &entry.kind else {
        return None;
    };
    let (header, data) = images
        .get(index)?
        .data_url
        .strip_prefix("data:")?
        .split_once(";base64,")?;
    let media_type = ["image/png", "image/jpeg", "image/gif", "image/webp"]
        .into_iter()
        .find(|candidate| *candidate == header)?;
    Some((media_type, BASE64_STANDARD.decode(data).ok()?))
}

fn title_from(prompt: &str) -> String {
    let line = prompt
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .trim();
    let mut chars = line.chars();
    let head = chars.by_ref().take(MAX_TITLE_CHARS).collect::<String>();
    if chars.next().is_some() {
        return format!("{}…", head.trim_end());
    }
    head
}

impl State {
    fn live(&self, session: &str) -> Option<&Live> {
        self.sessions.iter().find(|live| live.info.id == session)
    }

    fn live_mut(&mut self, session: &str) -> Option<&mut Live> {
        self.sessions
            .iter_mut()
            .find(|live| live.info.id == session)
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
                    context: None,
                    roster: SubagentRoster::default(),
                    agents: HashMap::new(),
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
                // A new fork starts from its parent's visible history, as the terminal's does.
                let inherited = fork_parent(&record)
                    .and_then(|parent| self.live(&parent))
                    .map(|parent| parent.model.fork_snapshot());
                let Some(live) = self.live_mut(&session) else {
                    return;
                };
                if let Some(inherited) = inherited {
                    live.model = inherited;
                }
                apply_record(live, &record);
            }
            Publication::Closed { session } => {
                let Some(index) = self
                    .sessions
                    .iter()
                    .position(|live| live.info.id == session)
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
                reasoning_mode,
                speed,
            } => {
                if let Some(live) = self.live_mut(&session) {
                    live.info.model = model;
                    live.info.effort = effort;
                    live.info.reasoning_mode = reasoning_mode;
                    live.info.speed = speed;
                }
            }
            Publication::Context { session, budget } => {
                if let Some(live) = self.live_mut(&session) {
                    live.context = Some(budget);
                }
            }
            Publication::Subagents { session, roster } => {
                if let Some(live) = self.live_mut(&session) {
                    live.roster = roster;
                }
            }
            Publication::SubagentRecord {
                session,
                agent,
                record,
            } => {
                if let Some(live) = self.live_mut(&session) {
                    live.agents.entry(agent).or_default().apply(&record);
                }
            }
            Publication::Message { session, update } => {
                if let Some(live) = self.live_mut(&session) {
                    apply_message(live, update);
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
            active: active.and_then(|active| self.live_mut(&active).map(|live| live.active_sent())),
        }
    }

    /// Sends to every client, dropping those whose buffer is full or whose stream ended.
    fn broadcast(&mut self, frame: &Frame) {
        self.clients
            .retain(|client| client.try_send(frame.clone()).is_ok());
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
        let previous = sent
            .active
            .take()
            .filter(|previous| previous.session == active);
        match previous {
            Some(previous) if !live.reshaped => diff_active(live, &previous, &mut frames),
            previous => {
                if previous.is_none() {
                    frames.push(frame("active", &SessionRef { session: &active }));
                }
                frames.push(frame("snapshot", &live.snapshot()));
            }
        }
        live.reshaped = false;
        sent.active = Some(live.active_sent());
        for frame in &frames {
            self.broadcast(frame);
        }
        self.sent = Some(sent);
    }
}

/// The parent named by a fork's `session.started` record.
fn fork_parent(record: &TranscriptRecord) -> Option<String> {
    record.session_started()?.parent_session_id
}

/// Applies a directed-message update the way the terminal does: the session's own transcript shows
/// messages that agents sent, and each agent's transcript shows its conversations.
fn apply_message(live: &mut Live, update: AgentMessageUpdate) {
    let from_agent = update.thread.messages.iter().any(|message| {
        message.id == update.message_id && matches!(message.from, MessageSender::Agent { .. })
    });
    if from_agent {
        let change = live
            .model
            .apply_message(MessageSender::Root, update.clone());
        if change.removed.is_some() {
            live.reshaped = true;
        }
    }
    let mut previous = None;
    for participant in update.thread.participants {
        let MessageSender::Agent { agent_id } = participant else {
            continue;
        };
        if previous == Some(agent_id) {
            continue;
        }
        previous = Some(agent_id);
        live.agents
            .entry(agent_id)
            .or_default()
            .apply_message(participant, update.clone());
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
    struct SettingsEvent<'a> {
        session: &'a str,
        model: &'a str,
        effort: ReasoningEffort,
        reasoning_mode: ReasoningMode,
        speed: Speed,
    }
    #[derive(Serialize)]
    struct ContextEvent<'a> {
        session: &'a str,
        #[serde(flatten)]
        budget: ContextBudget,
    }
    #[derive(Serialize)]
    struct SubagentsEvent<'a> {
        session: &'a str,
        #[serde(flatten)]
        roster: &'a SubagentRoster,
    }
    #[derive(Serialize)]
    struct SubagentEntryEvent<'a> {
        session: &'a str,
        agent: AgentId,
        entry: WireEntry,
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
        images: Vec<WireImage>,
        origin: String,
    }

    let session = live.info.id.clone();
    let session = session.as_str();
    if live.settings() != previous.settings {
        frames.push(frame(
            "settings",
            &SettingsEvent {
                session,
                model: &live.info.model,
                effort: live.info.effort,
                reasoning_mode: live.info.reasoning_mode,
                speed: live.info.speed,
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
                images: live.draft.images.iter().map(WireImage::from).collect(),
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

    if live.context != previous.context
        && let Some(budget) = live.context
    {
        frames.push(frame("context", &ContextEvent { session, budget }));
    }
    if live.roster != previous.roster {
        frames.push(frame(
            "subagents",
            &SubagentsEvent {
                session,
                roster: &live.roster,
            },
        ));
    }
    for (agent, model) in &live.agents {
        let sent = previous.agent_entries.get(agent);
        for (index, entry) in visible(model).enumerate() {
            let unchanged = sent
                .and_then(|sent| sent.get(index))
                .is_some_and(|(id, revision)| {
                    *id == entry.id.index() && *revision == entry.revision
                });
            if !unchanged {
                frames.push(frame(
                    "subagent_entry",
                    &SubagentEntryEvent {
                        session,
                        agent: *agent,
                        entry: WireEntry::new(entry),
                    },
                ));
            }
        }
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

#[cfg(test)]
mod tests {
    use super::{CLIENT_BUFFER, FLUSH_INTERVAL, Hub, MAX_STREAMS, Subscription, user_image};
    use crate::{
        app::config::{ReasoningEffort, ReasoningMode, Speed},
        core::{
            protocol::{Busy, Draft, Origin, Publication, QueuedPrompt, SessionInfo},
            transcript::{
                LocalEvent, SessionStarted, TranscriptModel, TranscriptRecord, TurnId,
                UserSubmitted,
            },
        },
        web::{
            bridge::{self, LoopEnd},
            testing::sse_event,
        },
    };
    use nanocodex::agent::events::{AgentEvent, AgentEventKind};
    use serde_json::{Value, json, value::to_raw_value};
    use std::{sync::Arc, time::Duration};
    use tokio_util::sync::CancellationToken;

    struct Fixture {
        terminal: LoopEnd,
        hub: Hub,
        _shutdown: CancellationToken,
    }

    fn fixture() -> Fixture {
        let (terminal, end) = bridge::bridge();
        let shutdown = CancellationToken::new();
        let hub = Hub::spawn(end.publications, shutdown.clone());
        Fixture {
            terminal,
            hub,
            _shutdown: shutdown,
        }
    }

    impl Fixture {
        fn publish(&self, publication: Publication) {
            self.terminal.publisher.publish(publication);
        }

        fn open(&self, id: &str, prompt: Option<&str>) {
            let records = prompt
                .map(|text| vec![user_record(1, text)])
                .unwrap_or_default();
            self.publish(Publication::Opened {
                info: SessionInfo {
                    id: id.to_owned(),
                    workspace: "/work".into(),
                    model: "gpt-6.1-sol".to_owned(),
                    effort: ReasoningEffort::Low,
                    reasoning_mode: ReasoningMode::Standard,
                    speed: Speed::Standard,
                },
                records,
                draft: Draft::default(),
                queue: Vec::new(),
                busy: Busy::default(),
            });
        }

        fn activate(&self, id: &str) {
            self.publish(Publication::Active {
                session: id.to_owned(),
            });
        }
    }

    fn user_record(sequence: u64, text: &str) -> Arc<TranscriptRecord> {
        Arc::new(
            TranscriptRecord::from_local(
                sequence,
                1_000 + sequence,
                LocalEvent::UserSubmitted(UserSubmitted {
                    id: TurnId::new(sequence),
                    text: text.to_owned(),
                }),
            )
            .unwrap(),
        )
    }

    #[test]
    fn attached_images_are_served_as_decoded_raster_bytes_only() {
        let mut model = TranscriptModel::default();
        model.apply(&user_record(1, "look [Image #1] and [Image #2]"));
        let accepted = |payload: Value| {
            TranscriptRecord::from_agent(
                2,
                2_000,
                AgentEvent {
                    protocol_version: 1,
                    request_id: Arc::from("s1"),
                    seq: 1,
                    kind: AgentEventKind::InputAccepted,
                    payload: to_raw_value(&payload).unwrap().into(),
                },
            )
        };
        model.apply(&accepted(json!({"input": [
            {"type": "image", "image_url": "data:image/png;base64,aGk="},
            {"type": "image", "image_url": "data:image/svg+xml;base64,aGk="},
        ]})));
        let entry = model.entries()[0].id.index();

        assert_eq!(
            user_image(&model, entry, 0),
            Some(("image/png", b"hi".to_vec()))
        );
        assert_eq!(
            user_image(&model, entry, 1),
            None,
            "scriptable formats are refused"
        );
        assert_eq!(user_image(&model, entry, 2), None);
    }

    /// Lets the hub apply publications and flush them.
    async fn settle() {
        tokio::time::sleep(FLUSH_INTERVAL * 3).await;
    }

    fn drain(subscription: &mut Subscription) -> Vec<(String, Value)> {
        let mut events = Vec::new();
        while let Ok(frame) = subscription.frames.try_recv() {
            events.push(sse_event(&frame));
        }
        events
    }

    fn names(events: &[(String, Value)]) -> Vec<&str> {
        events.iter().map(|(name, _)| name.as_str()).collect()
    }

    #[tokio::test(start_paused = true)]
    async fn connecting_yields_hello_live_active_and_a_snapshot() {
        let fixture = fixture();
        fixture.open("s1", Some("Fix the flaky test\nwith details"));
        fixture.activate("s1");
        settle().await;

        let mut subscription = fixture.hub.subscribe().unwrap();
        let events = drain(&mut subscription);

        assert_eq!(names(&events), ["hello", "live", "active", "snapshot"]);
        assert_eq!(events[0].1["protocol_version"], 9);
        let live = &events[1].1;
        assert_eq!(live["active"], "s1");
        assert_eq!(live["sessions"][0]["title"], "Fix the flaky test");
        assert_eq!(live["sessions"][0]["state"], "idle");
        let snapshot = &events[3].1;
        assert_eq!(snapshot["session"], "s1");
        assert_eq!(snapshot["entries"][0]["kind"], "user");
        assert_eq!(
            snapshot["entries"][0]["text"],
            "Fix the flaky test\nwith details"
        );
        assert_eq!(snapshot["effort"], "low");
        assert_eq!(snapshot["speed"], "standard");
        assert_eq!(snapshot["context"], Value::Null);
        assert_eq!(snapshot["subagents"]["agents"], json!([]));
        assert_eq!(snapshot["running"], false);
    }

    #[tokio::test(start_paused = true)]
    async fn changes_are_coalesced_into_one_event_per_frame() {
        let fixture = fixture();
        fixture.open("s1", None);
        fixture.activate("s1");
        let mut subscription = fixture.hub.subscribe().unwrap();
        settle().await;
        drain(&mut subscription);

        for rev in 1..=3 {
            fixture.publish(Publication::Draft {
                session: "s1".into(),
                draft: Draft {
                    rev,
                    text: format!("draft {rev}"),
                    ..Draft::default()
                },
                origin: Origin::Web(5),
            });
        }
        fixture.publish(Publication::Record {
            session: "s1".into(),
            record: user_record(1, "hello"),
        });
        fixture.publish(Publication::Queue {
            session: "s1".into(),
            items: vec![QueuedPrompt {
                id: 4,
                text: "later".into(),
                steering: false,
            }],
        });
        fixture.publish(Publication::Settings {
            session: "s1".into(),
            model: "gpt-6.1-sol".into(),
            effort: ReasoningEffort::High,
            reasoning_mode: ReasoningMode::Pro,
            speed: Speed::Fast,
        });
        settle().await;

        let events = drain(&mut subscription);
        let count = |name: &str| events.iter().filter(|(event, _)| event == name).count();
        assert_eq!(count("draft"), 1);
        assert_eq!(count("entry"), 1);
        assert_eq!(count("queue"), 1);
        assert_eq!(count("settings"), 1);
        let draft = events.iter().find(|(name, _)| name == "draft").unwrap();
        assert_eq!(
            draft.1,
            json!({"session": "s1", "rev": 3, "text": "draft 3", "images": [], "origin": "web:5"})
        );
        let entry = events.iter().find(|(name, _)| name == "entry").unwrap();
        assert_eq!(entry.1["entry"]["kind"], "user");
        let live = events.iter().find(|(name, _)| name == "live").unwrap();
        assert_eq!(live.1["sessions"][0]["has_draft"], true);
        assert_eq!(live.1["sessions"][0]["title"], "hello");
    }

    #[tokio::test(start_paused = true)]
    async fn a_session_that_finishes_in_the_background_is_unread_until_activated() {
        let fixture = fixture();
        fixture.open("s1", None);
        fixture.open("s2", None);
        fixture.activate("s1");
        let mut subscription = fixture.hub.subscribe().unwrap();
        settle().await;
        drain(&mut subscription);

        let busy = |turns| Publication::Busy {
            session: "s2".into(),
            busy: Busy { turns, shells: 0 },
        };
        fixture.publish(busy(1));
        settle().await;
        let events = drain(&mut subscription);
        assert_eq!(events[0].1["sessions"][1]["state"], "running");
        assert_eq!(events[0].1["sessions"][1]["unread"], false);
        assert!(fixture.hub.any_busy());

        fixture.publish(busy(0));
        settle().await;
        let events = drain(&mut subscription);
        assert_eq!(events[0].1["sessions"][1]["unread"], true);
        assert!(!fixture.hub.any_busy());

        fixture.activate("s2");
        settle().await;
        let events = drain(&mut subscription);
        assert_eq!(names(&events), ["live", "active", "snapshot"]);
        assert_eq!(events[0].1["active"], "s2");
        assert_eq!(events[0].1["sessions"][1]["unread"], false);
        assert_eq!(events[2].1["session"], "s2");
    }

    #[tokio::test(start_paused = true)]
    async fn closing_a_session_notifies_clients_and_cancels_its_work() {
        let fixture = fixture();
        fixture.open("s1", None);
        fixture.open("s2", None);
        fixture.activate("s1");
        settle().await;
        let token = fixture.hub.closed_token("s2").unwrap();
        let mut subscription = fixture.hub.subscribe().unwrap();
        drain(&mut subscription);

        fixture.publish(Publication::Closed {
            session: "s2".into(),
        });
        settle().await;

        let events = drain(&mut subscription);
        assert_eq!(names(&events), ["closed", "live"]);
        assert_eq!(events[0].1, json!({"session": "s2"}));
        assert_eq!(events[1].1["sessions"].as_array().unwrap().len(), 1);
        assert!(token.is_cancelled());
        assert!(fixture.hub.closed_token("s2").is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_client_is_dropped_and_resynchronizes_from_a_snapshot() {
        let fixture = fixture();
        fixture.open("s1", None);
        fixture.activate("s1");
        let mut slow = fixture.hub.subscribe().unwrap();
        let mut healthy = fixture.hub.subscribe().unwrap();
        settle().await;
        drain(&mut healthy);

        for rev in 1..=u64::try_from(CLIENT_BUFFER).unwrap() {
            fixture.publish(Publication::Draft {
                session: "s1".into(),
                draft: Draft {
                    rev,
                    text: rev.to_string(),
                    ..Draft::default()
                },
                origin: Origin::Terminal,
            });
            settle().await;
            drain(&mut healthy);
        }

        assert_eq!(fixture.hub.connected_clients(), 1);
        let backlog = drain(&mut slow);
        assert!(backlog.len() <= CLIENT_BUFFER);
        assert!(
            slow.frames.recv().await.is_none(),
            "the stream ends so the browser reconnects"
        );

        let mut reconnected = fixture.hub.subscribe().unwrap();
        let events = drain(&mut reconnected);
        assert_eq!(events[3].1["draft"]["rev"], CLIENT_BUFFER);
    }

    #[tokio::test(start_paused = true)]
    async fn concurrent_streams_are_bounded() {
        let fixture = fixture();
        let mut streams = (0..MAX_STREAMS)
            .map(|_| fixture.hub.subscribe().unwrap())
            .collect::<Vec<_>>();

        assert!(fixture.hub.subscribe().is_err());
        streams.pop();
        assert!(fixture.hub.subscribe().is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn no_events_are_produced_without_clients() {
        let fixture = fixture();
        fixture.open("s1", None);
        fixture.activate("s1");
        settle().await;
        fixture.publish(Publication::Record {
            session: "s1".into(),
            record: user_record(1, "unseen"),
        });
        tokio::time::sleep(Duration::from_secs(1)).await;

        let mut subscription = fixture.hub.subscribe().unwrap();
        let events = drain(&mut subscription);
        assert_eq!(events[3].1["entries"][0]["text"], "unseen");
        assert_eq!(events.len(), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn a_fork_shows_its_parents_transcript_before_the_fork_marker() {
        let fixture = fixture();
        fixture.open("parent", Some("before the fork"));
        fixture.open("fork", None);
        fixture.activate("fork");
        fixture.publish(Publication::Record {
            session: "fork".into(),
            record: Arc::new(
                TranscriptRecord::from_local(
                    1,
                    2_000,
                    LocalEvent::SessionStarted(SessionStarted {
                        session_id: "fork".to_owned(),
                        parent_session_id: Some("parent".to_owned()),
                        parent_sequence: Some(1),
                        model: "gpt-6.1-sol".to_owned(),
                        effort: ReasoningEffort::Low,
                        reasoning_mode: ReasoningMode::Standard,
                        speed: Speed::Standard,
                        workspace: "/work".into(),
                        application_version: "test".to_owned(),
                    }),
                )
                .unwrap(),
            ),
        });
        settle().await;

        let mut subscription = fixture.hub.subscribe().unwrap();
        let events = drain(&mut subscription);
        let entries = &events[3].1["entries"];
        assert_eq!(entries[0]["text"], "before the fork");
        assert_eq!(entries[1]["kind"], "forked_from");
        assert_eq!(entries[1]["session"], "parent");
    }

    #[tokio::test(start_paused = true)]
    async fn context_and_subagents_are_projected_and_coalesced() {
        use crate::core::context::ContextBudget;
        use tact_subagents::{AgentId, SubagentRoster};

        let fixture = fixture();
        fixture.open("s1", None);
        fixture.activate("s1");
        let mut subscription = fixture.hub.subscribe().unwrap();
        settle().await;
        drain(&mut subscription);

        for window in [100, 200] {
            fixture.publish(Publication::Context {
                session: "s1".into(),
                budget: ContextBudget {
                    active_tokens: 10,
                    window_tokens: window,
                },
            });
        }
        fixture.publish(Publication::Subagents {
            session: "s1".into(),
            roster: SubagentRoster {
                max_subagents: 3,
                agents: Vec::new(),
            },
        });
        fixture.publish(Publication::SubagentRecord {
            session: "s1".into(),
            agent: AgentId::new(4),
            record: user_record(1, "child task"),
        });
        settle().await;

        let events = drain(&mut subscription);
        let find = |name: &str| events.iter().find(|(event, _)| event == name).unwrap();
        assert_eq!(
            find("context").1,
            json!({"session": "s1", "active_tokens": 10, "window_tokens": 200})
        );
        assert_eq!(find("subagents").1["max_subagents"], 3);
        assert_eq!(find("subagents").1["session"], "s1");
        let entry = find("subagent_entry");
        assert_eq!(entry.1["agent"], 4);
        assert_eq!(entry.1["entry"]["text"], "child task");
        let transcript = fixture.hub.agent_entries("s1", AgentId::new(4)).unwrap();
        assert_eq!(transcript.len(), 1);
        assert!(fixture.hub.agent_entries("s1", AgentId::new(9)).is_none());
    }
    #[tokio::test(start_paused = true)]
    async fn agent_messages_reach_the_session_and_each_agents_transcript() {
        use tact_subagents::{AgentId, AgentMessageUpdate};

        let update: AgentMessageUpdate = serde_json::from_value(json!({
            "message_id": 7,
            "thread": {
                "id": 7,
                "participants": [
                    {"kind": "agent", "agent_id": 4},
                    {"kind": "agent", "agent_id": 2}
                ],
                "messages": [{
                    "id": 7, "thread_id": 7,
                    "from": {"kind": "agent", "agent_id": 4}, "to": 2,
                    "priority": "urgent", "purpose": "finding",
                    "body": "the stream coalesces events"
                }]
            },
            "delivery": {"state": "delivered", "disposition": "steered"}
        }))
        .unwrap();
        let fixture = fixture();
        fixture.open("s1", None);
        fixture.activate("s1");
        let mut subscription = fixture.hub.subscribe().unwrap();
        settle().await;
        drain(&mut subscription);

        fixture.publish(Publication::Message {
            session: "s1".into(),
            update,
        });
        settle().await;

        let events = drain(&mut subscription);
        let entry = events
            .iter()
            .find(|event| event.0 == "entry")
            .expect("the session transcript shows messages sent by agents");
        assert_eq!(entry.1["entry"]["kind"], "directed_message");
        assert_eq!(entry.1["entry"]["delivery"], "delivered");
        let message = &entry.1["entry"]["messages"][0];
        assert_eq!(message["from"], 4);
        assert_eq!(message["to"], 2);
        assert_eq!(message["purpose"], "finding");
        assert_eq!(message["priority"], "urgent");
        assert_eq!(message["detail"], "steered");
        for agent in [4, 2] {
            let entries = fixture
                .hub
                .agent_entries("s1", AgentId::new(agent))
                .unwrap();
            assert_eq!(entries.len(), 1, "agent {agent}");
        }
    }
}
