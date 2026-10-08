//! Interactive terminal runtime.
//!
//! [`run`] owns a single event loop that serves the terminal and the web front-end. Terminal
//! input, web commands, queries, and auxiliary requests from the bridge, worker updates, agent and
//! subagent events, and background task completions all arrive as `tokio::select!` branches. Each
//! branch runs to completion before the next event is taken, so handlers mutate the app tree and
//! pane runtimes without locks. Before waiting, every iteration advances shutdown, publishes
//! changed state to web clients, and draws the terminal when the render scheduler is due.
//!
//! Ownership: the turn worker owns every agent; the loop owns each pane's [`PaneRuntime`], which
//! holds the session lock and the transcript journal that records the pane's agent events and
//! local events. Each journal has a writer task whose completion the loop awaits.
//!
//! Shutdown runs in this order (see [`Lifecycle`]):
//! 1. The shutdown token is cancelled or a journal writer fails. The loop drops terminal input and
//!    disables every branch that would start new work.
//! 2. Shell, open, handoff, and memory tasks and the update check are aborted, and each pane's
//!    subagents are asked to close.
//! 3. The worker observes the same token, cancels its turns, and reports
//!    [`WorkerEvent::Stopped`]. Until then the loop keeps journaling worker updates and agent events,
//!    so in-flight turns are recorded.
//! 4. Once the worker has stopped, every agent event stream has ended, and shells and subagent
//!    shutdowns have finished, each journal records how its session ended and is closed.
//! 5. The loop exits after every journal writer has drained. The terminal is restored, the web
//!    server gets a short grace period, and the main session's ID is returned when it is resumable.

mod clipboard;
mod components;
mod editor;
mod format;
mod handoff_controller;
mod remote;
mod scheduler;
mod spinner;
mod system_scheme;
mod terminal;

use crate::{
    app::{
        config::{Config, ReasoningEffort, ReasoningMode, Speed},
        error::{Result, RuntimeError},
        herdr, hook,
    },
    core::{
        ConfiguredAgent,
        agent_events::{self, ForwardedAgentEvent},
        extensions::Skill,
        pane::PaneId,
        prompt::Submission,
        protocol::{
            AuxiliaryError, AuxiliaryRequest, Command, CommandError, OpenSpec, Origin, Reply,
            Request,
        },
        session::{self, RecentPrompt, SessionLock, SessionSummary},
        shell::{self, ShellExecution},
        subagent_updates::{self, ForwardedSubagentUpdate},
        supported_reasoning_mode,
        transcript::{
            LocalEvent, SessionEnded, SessionOutcome, SessionStarted, ShellId, TranscriptError,
            TranscriptJournal, TranscriptRecord, TurnId,
        },
        worker::{self, AuxiliaryContext, ReflectionContext, WorkerCommand, WorkerEvent},
    },
    tui::{
        components::{
            AppEffect, AppEvent, AppNode, ComponentUpdate, RecentPromptDraft, RenderRequest,
            RestoredSessionProjection, RootNode,
        },
        editor::EditorOutcome,
        handoff_controller::{HandoffCompletion, HandoffController, PreparedHandoff},
        scheduler::{RenderScheduler, STREAM_FRAME_INTERVAL},
        terminal::TerminalSession,
    },
    web::bridge::{self, WebStatus},
};
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use futures_util::StreamExt;
use nanocodex::HarnessModel as Model;
use std::{
    collections::{HashMap, HashSet},
    io::{self, IsTerminal},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};
use tact_memory::{
    MemoryAccess, MemoryError, MemoryKey, MemoryRecord, MemorySource, MemoryStore,
    SelectedMemoryStore,
};
use tact_subagents::Subagents;
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::{JoinHandle, JoinSet},
    time::sleep_until,
};
use tokio_util::sync::CancellationToken;

pub(crate) enum StartupMode {
    NewSession(Model),
    ResumeSession(String),
    ResumeSelector(Model),
}

type EditorTask =
    JoinHandle<std::result::Result<EditorCompletion, crate::app::error::ExternalEditorError>>;

type EffortUpdateTask = JoinHandle<Result<EffortUpdate>>;

type SpeedUpdateTask = JoinHandle<Result<SpeedUpdate>>;

type NewSessionTask = JoinHandle<(
    PaneId,
    ReasoningEffort,
    ReasoningMode,
    Speed,
    Model,
    components::DraftReset,
    Result<ConfiguredAgent>,
)>;

type SessionListTask = JoinHandle<(PaneId, Result<Vec<SessionSummary>>)>;

type RecentPromptTask = JoinHandle<Result<Vec<RecentPrompt>>>;

type ResumeSessionTask = JoinHandle<(
    PaneId,
    ReasoningEffort,
    ReasoningMode,
    Speed,
    Result<RestoredSession>,
)>;

type UpdateCheckTask =
    JoinHandle<std::result::Result<Option<semver::Version>, crate::app::update::UpdateError>>;

struct WebTaskCompletion {
    pane: PaneId,
    /// A sign-in link to show as a QR code, when the task was a QR request; otherwise a user-facing
    /// failure message is the only thing worth reporting.
    result: std::result::Result<Option<String>, String>,
}

type CommandReply = oneshot::Sender<std::result::Result<Reply, CommandError>>;

/// A session being started for a pane that was opened beside the others.
enum OpenedSession {
    Fresh {
        configured: Box<ConfiguredAgent>,
        settings: PaneSettings,
    },
    Restored {
        restored: Box<RestoredSession>,
        settings: PaneSettings,
        preferred_reasoning_mode: ReasoningMode,
    },
}

type OpenTask = (PaneId, Result<OpenedSession>);

const WEB_SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Shutdown progress of the event loop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Lifecycle {
    /// Serving input, web requests, and background tasks.
    Running,
    /// Shutdown was requested; subagent shutdown has not started yet.
    Stopping,
    /// No new work is accepted; the loop drains the worker, agent streams, shells, subagents, and
    /// journal writers.
    Draining,
}

impl Lifecycle {
    fn is_running(self) -> bool {
        self == Self::Running
    }

    fn stop(&mut self) {
        if self.is_running() {
            *self = Self::Stopping;
        }
    }
}

const HANDOFF_PROMPT: &str = concat!(
    "Prepare a self-contained continuation prompt for a new coding agent that will take over this ",
    "thread. Summarize the user's objective and requirements, important decisions and constraints, ",
    "work already completed, the current repository and revision state, relevant files and symbols, ",
    "validation performed, unresolved blockers, and concrete next steps. Preserve exact technical ",
    "details that the next agent would otherwise need to rediscover. Do not continue the task, use ",
    "tools, or address the user. Return only the continuation prompt, ready to be edited and sent ",
    "to the new agent."
);

fn spawn_update_check() -> Option<UpdateCheckTask> {
    if crate::app::installation::current().is_development() {
        return None;
    }
    Some(tokio::spawn(crate::app::update::check_for_update()))
}

struct RestoredSession {
    configured: ConfiguredAgent,
    lock: SessionLock,
    records: Vec<Arc<TranscriptRecord>>,
    projection: RestoredSessionProjection,
    reasoning_mode: ReasoningMode,
    model: Model,
    next_sequence: u64,
}

enum EditorTarget {
    Draft { pane: PaneId, text: String },
    Config(PathBuf),
    File(PathBuf),
}

enum EditorCompletion {
    Draft {
        pane: PaneId,
        outcome: EditorOutcome,
    },
    Config,
    File,
}

struct EffortUpdate {
    pane: PaneId,
    to: ReasoningEffort,
    preferred_reasoning_mode: ReasoningMode,
}

struct SpeedUpdate {
    pane: PaneId,
    speed: Speed,
}

struct PendingSubmission {
    id: TurnId,
    prompt: Submission,
}

/// A pane and the incarnation of its runtime. Replacing a pane's agent starts a new generation,
/// so events and writer completions from the replaced session can be recognised and ignored.
#[derive(Clone, Copy)]
struct PaneGeneration {
    pane: PaneId,
    generation: u64,
}

/// Whether a pane's session already had a stored transcript when the pane opened it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionOrigin {
    /// A fresh session or fork, stored only once its transcript receives a record.
    New,
    /// A resumed session that is already listed in storage.
    Resumed,
}

struct PaneSession<'a> {
    id: &'a str,
    parent_id: Option<&'a str>,
    parent_sequence: Option<u64>,
    next_sequence: u64,
    origin: SessionOrigin,
    skills_catalog_present: bool,
}

#[derive(Clone, Copy)]
struct PaneSettings {
    effort: ReasoningEffort,
    reasoning_mode: ReasoningMode,
    speed: Speed,
    model: Model,
}

impl PaneSettings {
    const fn new(
        effort: ReasoningEffort,
        reasoning_mode: ReasoningMode,
        speed: Speed,
        model: Model,
    ) -> Self {
        Self {
            effort,
            reasoning_mode,
            speed,
            model,
        }
    }
}

impl<'a> PaneSession<'a> {
    const fn new(
        id: &'a str,
        parent_id: Option<&'a str>,
        parent_sequence: Option<u64>,
        next_sequence: u64,
        skills_catalog_present: bool,
    ) -> Self {
        Self {
            id,
            parent_id,
            parent_sequence,
            next_sequence,
            origin: SessionOrigin::New,
            skills_catalog_present,
        }
    }

    const fn resumed(id: &'a str, next_sequence: u64, skills_catalog_present: bool) -> Self {
        Self {
            id,
            parent_id: None,
            parent_sequence: None,
            next_sequence,
            origin: SessionOrigin::Resumed,
            skills_catalog_present,
        }
    }
}

/// Everything the event loop owns for one open pane's session.
struct PaneRuntime {
    session_id: String,
    instructions: Arc<str>,
    skills_catalog_present: bool,
    origin: SessionOrigin,
    /// The transcript journal; absent once the pane's journal has been closed.
    journal: Option<TranscriptJournal>,
    writer_path: PathBuf,
    /// Set by the journal writer once a record of this session reaches storage.
    persisted_transcript: Arc<AtomicBool>,
    agent: AgentState,
    next_turn: u64,
    next_shell: u64,
    pending_shell_context: Vec<String>,
    pending_submission: Option<PendingSubmission>,
    /// The settings the pane's agent is currently running with.
    settings: PaneSettings,
    active_shells: usize,
    /// See [`PaneGeneration`].
    generation: u64,
    subagent_control: Subagents,
    _lock: SessionLock,
}

/// Where a pane's agent is in its lifetime, as observed through its event stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AgentState {
    /// The agent's event stream is forwarding into the pane.
    Running,
    /// The pane was closed; its runtime is removed once the agent's event stream ends.
    Closing,
    /// The agent's event stream ended while the pane stayed open.
    Stopped,
}

struct WriterCompletion {
    pane: PaneId,
    session_id: String,
    generation: u64,
    result: std::result::Result<(), TranscriptError>,
}

struct RecentPromptRequest {
    pane: PaneId,
    session_id: String,
    workspace: PathBuf,
    current_prompts: Vec<RecentPromptDraft>,
}

enum MemoryOperation {
    List,
    Delete(MemoryKey),
}

enum MemoryCompletion {
    Listed {
        pane: PaneId,
        generation: u64,
        source: MemorySource,
        result:
            std::result::Result<(MemoryAccess, Vec<MemoryRecord>), (Option<MemoryAccess>, String)>,
    },
    Deleted {
        pane: PaneId,
        generation: u64,
        key: MemoryKey,
        conflict: bool,
        result: std::result::Result<(), String>,
    },
}

impl MemoryCompletion {
    const fn identity(&self) -> (PaneId, u64) {
        match self {
            Self::Listed {
                pane, generation, ..
            }
            | Self::Deleted {
                pane, generation, ..
            } => (*pane, *generation),
        }
    }

    fn into_event(self) -> AppEvent {
        match self {
            Self::Listed {
                pane,
                result: Ok((access, records)),
                ..
            } => AppEvent::MemoriesLoaded {
                pane,
                access,
                records,
            },
            Self::Listed {
                pane,
                source,
                result: Err((access, error)),
                ..
            } => AppEvent::MemoryLoadFailed {
                pane,
                source,
                access,
                error,
            },
            Self::Deleted {
                pane,
                key,
                result: Ok(()),
                ..
            } => AppEvent::MemoryDeleted { pane, key },
            Self::Deleted {
                pane,
                conflict,
                result: Err(error),
                ..
            } => AppEvent::MemoryDeleteFailed {
                pane,
                error,
                conflict,
            },
        }
    }
}

async fn run_memory_operation(
    pane: PaneId,
    generation: u64,
    store: &SelectedMemoryStore,
    operation: MemoryOperation,
) -> MemoryCompletion {
    match operation {
        MemoryOperation::List => MemoryCompletion::Listed {
            pane,
            generation,
            source: store.source(),
            result: match store.access().await {
                Ok(access) => store
                    .list()
                    .await
                    .map(|records| (access.clone(), records))
                    .map_err(|error| (Some(access), error.to_string())),
                Err(error) => Err((None, error.to_string())),
            },
        },
        MemoryOperation::Delete(key) => {
            let result = store.delete(key.clone()).await;
            MemoryCompletion::Deleted {
                pane,
                generation,
                key,
                conflict: matches!(result, Err(MemoryError::Conflict)),
                result: result.map_err(|error| error.to_string()),
            }
        }
    }
}

/// Per-pane request counters for memory operations. Only the completion of a pane's newest
/// request is presented; a configuration reload makes every outstanding completion stale.
#[derive(Default)]
struct MemoryGenerations(HashMap<PaneId, u64>);

impl MemoryGenerations {
    /// Starts a new request for the pane and returns its generation.
    fn next(&mut self, pane: PaneId) -> u64 {
        let generation = self.0.entry(pane).or_default();
        *generation = generation.wrapping_add(1).max(1);
        *generation
    }

    fn invalidate_all(&mut self) {
        for generation in self.0.values_mut() {
            *generation = generation.wrapping_add(1).max(1);
        }
    }

    fn is_current(&self, pane: PaneId, generation: u64) -> bool {
        self.0.get(&pane) == Some(&generation)
    }
}

impl PaneRuntime {
    fn journal_mut(&mut self) -> Result<&mut TranscriptJournal> {
        self.journal
            .as_mut()
            .ok_or_else(|| TranscriptError::WriterStopped(self.writer_path.clone()).into())
    }

    fn exit_session_id(&self) -> Option<String> {
        (self.origin == SessionOrigin::Resumed || self.persisted_transcript.load(Ordering::Acquire))
            .then(|| self.session_id.clone())
    }
}

fn subagent_pane(
    panes: &HashMap<PaneId, PaneRuntime>,
    event: &ForwardedSubagentUpdate,
) -> Option<PaneId> {
    panes.iter().find_map(|(&pane, runtime)| {
        (runtime.session_id == event.root_session_id
            && runtime.subagent_control.runtime_id() == event.runtime_id)
            .then_some(pane)
    })
}

pub(crate) async fn run(
    mut config: Config,
    startup: StartupMode,
    shutdown: CancellationToken,
) -> Result<Option<String>> {
    ensure_interactive()?;

    let initial_effort = config.agent().thinking();
    let initial_speed = config.agent().speed();
    let initial_max_subagents = config.agent().max_subagents();
    let preferred_reasoning_mode = config.agent().reasoning_mode();
    let open_resume_selector = matches!(&startup, StartupMode::ResumeSelector(_));
    let (resume_session_id, fresh_model) = match startup {
        StartupMode::NewSession(model) | StartupMode::ResumeSelector(model) => (None, Some(model)),
        StartupMode::ResumeSession(session_id) => (Some(session_id), None),
    };
    let resuming = resume_session_id.is_some();
    let resume_lock = resume_session_id
        .as_deref()
        .map(|session_id| SessionLock::acquire(config.path(), session_id))
        .transpose()?;
    let (configured, restored, reasoning_mode, model, next_sequence) =
        if let Some(session_id) = resume_session_id {
            let restored_config = config.clone();
            let config_path = restored_config.path().to_path_buf();
            let checkpoint_session_id = session_id.clone();
            let checkpoint = tokio::task::spawn_blocking(move || {
                session::load_checkpoint(&config_path, &checkpoint_session_id)
            });
            let transcript = session::load_transcript_async(
                restored_config.path().to_path_buf(),
                session_id.clone(),
            );
            let (snapshot, records) = tokio::join!(checkpoint, transcript);
            let snapshot = snapshot.map_err(RuntimeError::SessionTask)??;
            let records = records?;
            tokio::task::spawn_blocking(move || -> Result<_> {
                let restored_config = restored_config.with_workspace(session::workspace(&records)?);
                let reasoning_mode = session::reasoning_mode(&records);
                let model = session::model(&records)?;
                let next_sequence = session::next_sequence(&records);
                let projection = RootNode::project_session(initial_effort, records.clone());
                let configured = ConfiguredAgent::from_config_with_session(
                    &restored_config,
                    initial_effort,
                    reasoning_mode,
                    model,
                    Some(&session_id),
                    Some(snapshot),
                )?;
                Ok((
                    configured,
                    Some((projection, records)),
                    reasoning_mode,
                    model,
                    next_sequence,
                ))
            })
            .await
            .map_err(RuntimeError::SessionTask)??
        } else {
            let model = fresh_model.expect("a fresh TUI startup must select a model");
            let reasoning_mode = supported_reasoning_mode(model, preferred_reasoning_mode);
            (
                ConfiguredAgent::from_config_with_model(
                    &config,
                    initial_effort,
                    reasoning_mode,
                    model,
                )?,
                None,
                reasoning_mode,
                model,
                1,
            )
        };
    let workspace = config.agent().workspace().to_path_buf();
    let mut terminal = TerminalSession::enter().map_err(RuntimeError::Terminal)?;
    let ConfiguredAgent {
        workspace: initial_workspace,
        agent,
        context,
        events,
        instructions,
        skills,
        memory_enabled,
        subagent_updates,
        subagent_control,
    } = configured;
    terminal
        .report_working_directory(&initial_workspace)
        .map_err(RuntimeError::Terminal)?;
    let initial_config = config.with_workspace(initial_workspace.clone());
    let main_session_id = agent.session_id().to_string();
    let main_lock = match resume_lock {
        Some(lock) => lock,
        None => SessionLock::acquire(config.path(), &main_session_id)?,
    };
    let mut herdr = herdr::Reporter::from_env(&main_session_id);
    let (writer_sender, mut writer_updates) = mpsc::unbounded_channel();
    let mut panes = HashMap::new();
    panes.insert(
        PaneId::Main,
        open_pane(
            PaneGeneration {
                pane: PaneId::Main,
                generation: 0,
            },
            if resuming {
                PaneSession::resumed(&main_session_id, next_sequence, !skills.is_empty())
            } else {
                PaneSession::new(&main_session_id, None, None, 1, !skills.is_empty())
            },
            &initial_config,
            PaneSettings::new(initial_effort, reasoning_mode, initial_speed, model),
            instructions,
            subagent_control.clone(),
            &writer_sender,
            main_lock,
        )?,
    );
    let memory_review = if resuming {
        worker::MemoryReviewState::restored(memory_enabled)
    } else {
        worker::MemoryReviewState::fresh(memory_enabled)
    };
    let (commands, mut worker_updates) =
        worker::spawn(agent, context, memory_review, shutdown.clone());
    let (agent_event_sender, mut agent_events) = mpsc::unbounded_channel();
    agent_events::forward(PaneId::Main, 0, events, agent_event_sender.clone());
    let (subagent_sender, mut subagent_events) = mpsc::unbounded_channel();
    subagent_updates::forward(
        subagent_control.runtime_id(),
        subagent_updates,
        subagent_sender.clone(),
    );
    let mut root = RootNode::new(&initial_workspace, initial_effort);
    root.set_tui_config(*config.tui());
    root.set_claude_enabled(config.claude().enabled());
    root.set_reasoning_modes(reasoning_mode, preferred_reasoning_mode);
    root.set_speed(initial_speed);
    root.set_max_subagents(initial_max_subagents);
    let mut memory_store = crate::core::configured_memory_store(&config, &workspace)?;
    root.set_memory_enabled(memory_store.is_some());
    let restored_records = restored.map(|(projection, records)| {
        root.install_session_projection(
            &initial_workspace,
            initial_effort,
            reasoning_mode,
            preferred_reasoning_mode,
            initial_speed,
            projection,
        );
        records
    });
    root.set_model(model);
    root.set_skills(skills);
    let mut theme = config.theme().clone();
    if let Some(scheme) = system_scheme::detect_system_scheme() {
        theme.set_system_scheme(scheme);
    }
    let mut app = AppNode::new(theme, workspace.clone(), root);
    app.set_max_live_sessions(config.web().max_live_sessions());
    let (loop_end, web_end) = bridge::bridge();
    let bridge::LoopEnd {
        publisher,
        requests: mut web_requests,
        queries: mut web_queries,
        auxiliary: mut auxiliary_requests,
        status: web_status,
    } = loop_end;
    app.attach_publisher(publisher);
    app.session_opened(
        PaneId::Main,
        main_session_id.clone(),
        restored_records.unwrap_or_default(),
    );
    // The server runs beside the terminal and never stops it: its failure only changes the
    // status that "Open in browser" reports.
    let web_shutdown = shutdown.child_token();
    let web_server = config
        .web()
        .enabled()
        .then(|| crate::web::spawn(&config, &workspace, web_end, web_shutdown.clone()));
    let prompt_warmup_config = config.path().to_path_buf();
    let mut recent_prompt_task = Some(tokio::spawn(async move {
        session::load_recent_prompts_async(prompt_warmup_config)
            .await
            .map_err(Into::into)
    }));
    let mut recent_prompt_cache = None::<Vec<RecentPrompt>>;
    let mut recent_prompt_request = None::<RecentPromptRequest>;
    let mut update_check_task = spawn_update_check();
    let (system_theme_sender, mut system_theme_updates) = mpsc::unbounded_channel();
    system_scheme::watch_system_scheme(system_theme_sender, shutdown.clone());
    let mut input = Some(EventStream::new());
    let mut editor_task = None::<EditorTask>;
    let mut effort_task = None::<EffortUpdateTask>;
    let mut speed_task = None::<SpeedUpdateTask>;
    let mut new_session_task = None::<NewSessionTask>;
    let mut session_list_task = None::<SessionListTask>;
    let mut handoff_controller = HandoffController::new();
    let mut open_tasks = JoinSet::<OpenTask>::new();
    let mut open_replies = HashMap::<PaneId, CommandReply>::new();
    let mut resume_session_task = None::<ResumeSessionTask>;
    let mut scheduler = RenderScheduler::new(STREAM_FRAME_INTERVAL, Instant::now());
    let mut lifecycle = Lifecycle::Running;
    let mut worker_stopped = false;
    let mut herdr_turns = HashSet::new();
    let mut worker_error = None::<nanocodex::NanocodexError>;
    let mut writer_error = None::<TranscriptError>;
    let mut writers_open = 1_usize;
    let mut shell_tasks = JoinSet::<(PaneId, ShellExecution)>::new();
    let mut web_tasks = JoinSet::<WebTaskCompletion>::new();
    let mut memory_tasks = JoinSet::<MemoryCompletion>::new();
    let mut memory_generations = MemoryGenerations::default();
    let mut subagent_shutdowns = JoinSet::<()>::new();

    macro_rules! effect_context {
        () => {
            EffectContext {
                app: &mut app,
                commands: &commands,
                workspace: &workspace,
                config: &mut config,
                shutdown: &shutdown,
                input: &mut input,
                editor_task: &mut editor_task,
                effort_task: &mut effort_task,
                speed_task: &mut speed_task,
                new_session_task: &mut new_session_task,
                session_list_task: &mut session_list_task,
                recent_prompt_task: &mut recent_prompt_task,
                recent_prompt_cache: &mut recent_prompt_cache,
                recent_prompt_request: &mut recent_prompt_request,
                handoff_controller: &mut handoff_controller,
                web_status: &web_status,
                open_tasks: &mut open_tasks,
                resume_session_task: &mut resume_session_task,
                terminal: &mut terminal,
                scheduler: &mut scheduler,
                panes: &mut panes,
                shell_tasks: &mut shell_tasks,
                web_tasks: &mut web_tasks,
                memory_store: &mut memory_store,
                memory_tasks: &mut memory_tasks,
                memory_generations: &mut memory_generations,
                subagent_shutdowns: &mut subagent_shutdowns,
            }
        };
    }

    macro_rules! apply_app_update {
        ($update:expr) => {
            apply_update($update, effect_context!()).await?;
        };
    }

    macro_rules! install_agent {
        ($pane:expr, $configured:expr, $history:expr, $settings:expr) => {{
            let configured = $configured;
            app.set_pane_workspace($pane, configured.workspace.clone());
            install_agent(
                $pane,
                configured,
                $history,
                $settings,
                &config,
                &mut panes,
                &commands,
                &agent_event_sender,
                &subagent_sender,
                &writer_sender,
                &mut writers_open,
                &mut subagent_shutdowns,
            )?
        }};
    }

    if open_resume_selector {
        apply_app_update!(app.open_resume_selector());
    }

    let mut reported_workspace = initial_workspace;
    loop {
        if let Some(root) = app.root(app.active_pane())
            && root.workspace() != reported_workspace
        {
            terminal
                .report_working_directory(root.workspace())
                .map_err(RuntimeError::Terminal)?;
            reported_workspace = root.workspace().to_owned();
        }
        if !lifecycle.is_running() {
            if let Some(task) = update_check_task.take() {
                task.abort();
            }
            shell_tasks.abort_all();
            open_tasks.abort_all();
            handoff_controller.cancel();
            memory_tasks.abort_all();
        }
        if lifecycle == Lifecycle::Stopping {
            for runtime in panes.values() {
                schedule_subagent_shutdown(runtime, &mut subagent_shutdowns);
            }
            lifecycle = Lifecycle::Draining;
        }
        if lifecycle == Lifecycle::Draining
            && worker_stopped
            && panes.values().all(|pane| pane.agent == AgentState::Stopped)
            && shell_tasks.is_empty()
            && subagent_shutdowns.is_empty()
        {
            close_journals(&mut panes, worker_error.as_ref())?;
            if writers_open == 0 {
                break;
            }
        }

        request_render(app.publish_changes(Origin::Terminal), &mut scheduler);
        if editor_task.is_none() && lifecycle.is_running() && scheduler.is_due(Instant::now()) {
            terminal
                .draw(|frame| app.render(frame))
                .map_err(RuntimeError::Terminal)?;
            scheduler.presented(Instant::now());
        }

        let render_deadline = scheduler.deadline();
        let animation_deadline = app.animation_deadline();
        tokio::select! {
            () = shutdown.cancelled(), if lifecycle.is_running() => {
                lifecycle.stop();
                input = None;
                if let Some(task) = editor_task.take() {
                    task.abort();
                    drop(task.await);
                }
                if let Some(task) = effort_task.take() {
                    task.abort();
                    drop(task.await);
                }
                if let Some(task) = speed_task.take() {
                    task.abort();
                    drop(task.await);
                }
                if let Some(task) = new_session_task.take() {
                    task.abort();
                    drop(task.await);
                }
            }
            event = async {
                input
                    .as_mut()
                    .expect("input branch is disabled without an event stream")
                    .next()
                    .await
            }, if input.is_some() && lifecycle.is_running() => {
                let event = event
                    .transpose()
                    .map_err(RuntimeError::Terminal)?
                    .ok_or_else(|| RuntimeError::Terminal(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "terminal input closed",
                    )))?;
                let refresh_cursor = matches!(&event, Event::FocusGained)
                    || matches!(
                        &event,
                        Event::Mouse(mouse) if matches!(mouse.kind, MouseEventKind::Down(_))
                    );
                if refresh_cursor {
                    terminal.invalidate_cursor_visibility();
                }
                let mut update = if is_image_paste(&event)
                    && let Some(data_url) = clipboard::image_data_url()
                {
                    app.update(AppEvent::PasteImage(data_url))
                } else {
                    app.update(AppEvent::Terminal(event))
                };
                if refresh_cursor {
                    update.render = update.render.max(RenderRequest::Immediate);
                }
                apply_app_update!(update);
            }
            Some(scheme) = system_theme_updates.recv(), if lifecycle.is_running() => {
                schedule(app.update(AppEvent::SystemThemeChanged(scheme)), &mut scheduler);
            }
            result = async {
                update_check_task
                    .as_mut()
                    .expect("update-check branch is disabled without a task")
                    .await
            }, if update_check_task.is_some() && lifecycle.is_running() => {
                update_check_task = None;
                if let Ok(Ok(Some(version))) = result {
                    schedule(app.update(AppEvent::UpdateAvailable(version)), &mut scheduler);
                }
            }
            event = agent_events.recv(), if panes.values().any(|pane| pane.agent != AgentState::Stopped) => {
                let Some(event) = event else {
                    for (&pane, runtime) in &mut panes {
                        if runtime.agent != AgentState::Stopped {
                            runtime.agent = AgentState::Stopped;
                            schedule(app.update(AppEvent::AgentStreamClosed(pane)), &mut scheduler);
                        }
                    }
                    continue;
                };
                match event {
                    ForwardedAgentEvent::Event { pane, session_id, generation, event } => {
                        let Some(runtime) = panes.get_mut(&pane) else {
                            continue;
                        };
                        if runtime.session_id != session_id || runtime.generation != generation {
                            continue;
                        }
                        let record = runtime.journal_mut()?.append_agent(event)?;
                        // `tool.result` is the canonical completion event for every agent tool.
                        let tool_finished = record.kind() == "tool.result";
                        apply_app_update!(app.update(AppEvent::Transcript { pane, record }));
                        if tool_finished {
                            terminal
                                .report_working_directory(app.root(app.active_pane()).map_or(&workspace, RootNode::workspace))
                                .map_err(RuntimeError::Terminal)?;
                        }
                    }
                    ForwardedAgentEvent::Closed { pane, session_id, generation } => {
                        let Some(runtime) = panes
                            .get_mut(&pane)
                            .filter(|runtime| runtime.session_id == session_id && runtime.generation == generation)
                        else {
                            continue;
                        };
                        if std::mem::replace(&mut runtime.agent, AgentState::Stopped) == AgentState::Closing {
                            if let Some(mut runtime) = panes.remove(&pane) {
                                close_pane_journal(&mut runtime, SessionOutcome::Closed, None)?;
                            }
                            continue;
                        }
                        schedule(app.update(AppEvent::AgentStreamClosed(pane)), &mut scheduler);
                    }
                }
            }
            Some(event) = subagent_events.recv(), if lifecycle.is_running() => {
                if let Some(pane) = subagent_pane(&panes, &event) {
                    apply_app_update!(app.update(AppEvent::Subagent {
                        pane,
                        update: event.update,
                    }));
                }
            }
            Some(request) = web_requests.recv(),
                if lifecycle.is_running()
                    && effort_task.is_none()
                    && speed_task.is_none()
                    && new_session_task.is_none()
                    && resume_session_task.is_none() =>
            {
                let Request { command, client, reply } = request;
                let active = app.active_pane();
                match command {
                    Command::DeleteMemory { key } => {
                        remote::delete_memory(memory_store.as_ref(), key).deliver(reply);
                    }
                    Command::ReloadConfig => {
                        let result = reload_config(&mut effect_context!(), active);
                        request_render(app.publish_changes(Origin::Web(client)), &mut scheduler);
                        drop(reply.send(result.map(|()| Reply::Done)));
                    }
                    Command::WriteConfig { text, revision } => {
                        let result = config
                            .replace_document(&text, &revision)
                            .map_err(remote::config_edit_error)
                            .and_then(|()| reload_config(&mut effect_context!(), active));
                        request_render(app.publish_changes(Origin::Web(client)), &mut scheduler);
                        drop(reply.send(result.map(|()| Reply::Done)));
                    }
                    Command::SetMaxSubagents { limit } => {
                        let result = apply_pane_effect(
                            active,
                            components::RootEffect::SetMaxSubagents(limit),
                            &mut effect_context!(),
                        )
                        .map_err(|error| CommandError::Failed(error.to_string()));
                        request_render(app.publish_changes(Origin::Web(client)), &mut scheduler);
                        drop(reply.send(result.map(|()| Reply::Done)));
                    }
                    Command::Open(spec) => {
                        match open_session(spec, &mut app, &config, &mut panes, &commands, &mut open_tasks).await {
                            Ok(OpenStarted::Pending(pane)) => {
                                open_replies.insert(pane, reply);
                            }
                            Ok(OpenStarted::Activated(session)) => {
                                drop(reply.send(Ok(Reply::Opened { session })));
                            }
                            Err(error) => drop(reply.send(Err(error))),
                        }
                        request_render(RenderRequest::Immediate, &mut scheduler);
                    }
                    command => {
                        let result = app.remote_command(command);
                        let result = match result {
                            Ok(update) => {
                                apply_app_update!(update);
                                Ok(Reply::Done)
                            }
                            Err(error) => Err(error),
                        };
                        request_render(app.publish_changes(Origin::Web(client)), &mut scheduler);
                        drop(reply.send(result));
                    }
                }
            }
            Some(request) = web_queries.recv(), if lifecycle.is_running() => {
                let state = remote::QueryState {
                    app: &app,
                    config: &config,
                    workspace: app.root(app.active_pane()).map_or(&workspace, RootNode::workspace),
                    memory_store: memory_store.as_ref(),
                    recent_prompts: recent_prompt_cache.as_deref(),
                };
                remote::answer(request.query, &state).deliver(request.reply);
            }
            Some(request) = auxiliary_requests.recv(), if lifecycle.is_running() => {
                let AuxiliaryRequest {
                    session,
                    prompt,
                    shutdown,
                    completion,
                } = request;
                if shutdown.is_cancelled() {
                    drop(completion.send(Err(AuxiliaryError::Cancelled)));
                    continue;
                }
                let Some((pane, runtime)) = app
                    .pane_for_session(&session)
                    .and_then(|pane| Some((pane, panes.get_mut(&pane)?)))
                else {
                    drop(completion.send(Err(AuxiliaryError::Failed(
                        "unknown session".to_owned(),
                    ))));
                    continue;
                };
                let id = TurnId::new(runtime.next_turn);
                runtime.next_turn = runtime.next_turn.saturating_add(1);
                commands
                    .send(WorkerCommand::Auxiliary {
                        pane,
                        id,
                        prompt: prompt.into(),
                        context: AuxiliaryContext::Clean,
                        shutdown,
                        completion,
                    })
                    .map_err(|_| RuntimeError::AgentWorkerStopped)?;
            }
            Some(result) = open_tasks.join_next(), if lifecycle.is_running() => {
                let (pane, opened) = result.map_err(RuntimeError::SessionTask)?;
                let reply = open_replies.remove(&pane);
                let opened = match opened {
                    Ok(opened) if app.root(pane).is_some() => opened,
                    Ok(_) => {
                        if let Some(reply) = reply {
                            drop(reply.send(Err(CommandError::Failed(
                                "the session was closed while it opened".to_owned(),
                            ))));
                        }
                        continue;
                    }
                    Err(error) => {
                        let error = error.to_string();
                        apply_app_update!(app.update(AppEvent::OpenFailed {
                            pane,
                            error: error.clone(),
                        }));
                        if let Some(reply) = reply {
                            drop(reply.send(Err(CommandError::Failed(error))));
                        }
                        continue;
                    }
                };
                let records = match opened {
                    OpenedSession::Fresh { configured, settings } => {
                        let skills = install_agent!(pane, *configured, InstalledSession::Fresh, settings);
                        schedule(
                            app.update(AppEvent::NewSessionReady {
                                pane,
                                effort: settings.effort,
                                reasoning_mode: settings.reasoning_mode,
                                speed: settings.speed,
                                model: settings.model,
                                draft_reset: components::DraftReset::Clear,
                                skills,
                            }),
                            &mut scheduler,
                        );
                        Vec::new()
                    }
                    OpenedSession::Restored { restored, settings, preferred_reasoning_mode } => {
                        let RestoredSession { configured, lock, records, projection, next_sequence, .. } = *restored;
                        let skills = install_agent!(
                            pane,
                            configured,
                            InstalledSession::Restored { next_sequence, lock },
                            settings
                        );
                        schedule(
                            app.update(AppEvent::SessionRestored {
                                pane,
                                projection: Box::new(projection),
                                effort: settings.effort,
                                reasoning_mode: settings.reasoning_mode,
                                preferred_reasoning_mode,
                                speed: settings.speed,
                                model: settings.model,
                                skills,
                            }),
                            &mut scheduler,
                        );
                        records
                    }
                };
                let session = panes[&pane].session_id.clone();
                app.session_opened(pane, session.clone(), records);
                if let Some(reply) = reply {
                    drop(reply.send(Ok(Reply::Opened { session })));
                }
            }
            update = worker_updates.recv(), if !worker_stopped => {
                let Some(update) = update else {
                    worker_stopped = true;
                    continue;
                };
                match update {
                    WorkerEvent::Stopped { error } => {
                        for (&pane, runtime) in &mut panes {
                            let journal = runtime.journal_mut()?;
                            if journal.is_empty() {
                                continue;
                            }
                            let record = journal.append_local(LocalEvent::WorkerStopped {
                                error: error.as_ref().map(ToString::to_string),
                            })?;
                            schedule(app.update(AppEvent::Transcript { pane, record }), &mut scheduler);
                        }
                        worker_stopped = true;
                        worker_error = error;
                    }
                    WorkerEvent::ContextBudget { pane, session_id, budget } => {
                        let Some(runtime) = panes.get_mut(&pane).filter(|runtime| runtime.session_id == session_id) else {
                            continue;
                        };
                        let update = if runtime.journal_mut()?.is_empty() {
                            app.update(AppEvent::ContextBudget { pane, budget })
                        } else {
                            let record = runtime.journal_mut()?.append_local(LocalEvent::ContextBudget(budget))?;
                            app.update(AppEvent::Transcript { pane, record })
                        };
                        schedule(update, &mut scheduler);
                    }
                    WorkerEvent::TurnAccepted { pane, id } => {
                        if herdr_turns.insert((pane, id)) && herdr_turns.len() == 1 {
                            let session_id = app
                                .main_pane()
                                .and_then(|pane| panes.get(&pane))
                                .map(|runtime| runtime.session_id.as_str());
                            herdr.working(session_id);
                        }
                        let Some(runtime) = panes.get_mut(&pane) else {
                            continue;
                        };
                        let record = runtime.journal_mut()?.append_local(LocalEvent::WorkerTurnAccepted { id })?;
                        schedule(app.update(AppEvent::Transcript { pane, record }), &mut scheduler);
                    }
                    WorkerEvent::CompactionFinished { pane, result, terminal_stop, duration_ns } => {
                        let Some(runtime) = panes.get_mut(&pane) else { continue; };
                        let (snapshot, error) = match result {
                            Ok(snapshot) => (Some(snapshot), None),
                            Err(error) => (None, Some(error)),
                        };
                        let event = LocalEvent::CompactionFinished { error: error.map(|error| error.to_string()), duration_ns, terminal_stop };
                        let record = match snapshot.as_ref() {
                            Some(snapshot) => {
                                let state = session::encode_checkpoint(snapshot, &runtime.instructions, runtime.skills_catalog_present)?;
                                runtime.journal_mut()?.append_local_with_resume_state(event, state)?
                            }
                            None => runtime.journal_mut()?.append_local(event)?,
                        };
                        runtime.journal_mut()?.flush().await?;
                        schedule(app.update(AppEvent::Transcript { pane, record }), &mut scheduler);
                        if let Some(budget) = snapshot.as_ref().and_then(|snapshot| snapshot.context_budget()) {
                            let record = runtime.journal_mut()?.append_local(LocalEvent::ContextBudget(budget))?;
                            schedule(app.update(AppEvent::Transcript { pane, record }), &mut scheduler);
                        }
                        apply_app_update!(app.update(AppEvent::CompactionFinished(pane)));
                    }
                    WorkerEvent::TurnFinished {
                        pane,
                        id,
                        error,
                        terminal_stop,
                        snapshot,
                        terminal_expected,
                    } => {
                        if herdr_turns.remove(&(pane, id)) && herdr_turns.is_empty() {
                            let session_id = app
                                .main_pane()
                                .and_then(|pane| panes.get(&pane))
                                .map(|runtime| runtime.session_id.as_str());
                            herdr.idle(session_id);
                        }
                        let Some(runtime) = panes.get_mut(&pane) else {
                            continue;
                        };
                        let resume_state = snapshot
                            .as_ref()
                            .map(|snapshot| {
                                session::encode_checkpoint(
                                    snapshot,
                                &runtime.instructions,
                                runtime.skills_catalog_present,
                                )
                            })
                            .transpose()?;
                        let event = LocalEvent::WorkerTurnFinished { id, error: error.map(|error| error.to_string()), terminal_stop };
                        let record = match resume_state {
                            Some(resume_state) => runtime
                                .journal_mut()?
                                .append_local_with_resume_state(event, resume_state)?,
                            None => runtime.journal_mut()?.append_local(event)?,
                        };
                        if terminal_stop.is_some() {
                            // Resume reads must observe the stop before the pane admits another command.
                            runtime.journal_mut()?.flush().await?;
                        }
                        schedule(app.update(AppEvent::Transcript { pane, record }), &mut scheduler);
                        if let Some(command) = config.agent().completion_hook() {
                            let command = command.to_owned();
                            let workspace = app.root(pane).expect("hook pane exists").workspace().to_owned();
                            tokio::spawn(async move {
                                drop(hook::execute(&command, &workspace).await);
                            });
                        }
                        apply_app_update!(app.update(AppEvent::WorkerTurnFinished {
                            pane,
                            terminal_expected,
                        }));
                    }
                    WorkerEvent::SteerAdmitted { pane, queue_id } => {
                        apply_app_update!(app.update(AppEvent::SteerAdmitted { pane, id: queue_id }));
                    }
                    WorkerEvent::SteerPromoted { pane, queue_id, id, prompt } => {
                        let Some(runtime) = panes.get_mut(&pane) else {
                            continue;
                        };
                        let record = runtime.journal_mut()?.append_local(LocalEvent::UserSubmitted {
                            id,
                            text: prompt.display_text().to_owned(),
                        })?;
                        schedule(app.update(AppEvent::Transcript { pane, record }), &mut scheduler);
                        schedule(app.update(AppEvent::SteerPromoted { pane, id: queue_id }), &mut scheduler);
                    }
                    WorkerEvent::SteerFailed {
                        pane,
                        queue_id,
                        error,
                    } => {
                        let Some(runtime) = panes.get_mut(&pane) else {
                            continue;
                        };
                        let record = runtime.journal_mut()?.append_local(LocalEvent::WorkerSteerFailed {
                            error: error.to_string(),
                        })?;
                        schedule(app.update(AppEvent::Transcript { pane, record }), &mut scheduler);
                        apply_app_update!(app.update(AppEvent::SteerFailed { pane, id: queue_id }));
                    }
                    WorkerEvent::TurnsCancelled { pane, count, error } => {
                        let Some(runtime) = panes.get_mut(&pane) else {
                            continue;
                        };
                        if count > 0 || error.is_some() {
                            let record = runtime.journal_mut()?.append_local(
                                LocalEvent::WorkerTurnsInterrupted { count, error: error.map(|error| error.to_string()) },
                            )?;
                            schedule(
                                app.update(AppEvent::Transcript { pane, record }),
                                &mut scheduler,
                            );
                        }
                        schedule(app.update(AppEvent::TurnsCancelled(pane)), &mut scheduler);
                    }
                    WorkerEvent::ForkOpened {
                        pane,
                        parent,
                        parent_sequence,
                        events,
                    } => {
                        let session_id = events.request_id().to_owned();
                        let Some(parent_runtime) = panes
                            .get(&parent)
                            .filter(|_| app.root(pane).is_some())
                        else {
                            // The fork or its parent closed while the worker forked.
                            commands
                                .send(WorkerCommand::ClosePane(pane))
                                .map_err(|_| RuntimeError::AgentWorkerStopped)?;
                            let error = "the session closed while it was being forked".to_owned();
                            if let Some(reply) = open_replies.remove(&pane) {
                                drop(reply.send(Err(CommandError::Failed(error.clone()))));
                            }
                            apply_app_update!(app.update(AppEvent::ForkFailed { pane, error }));
                            continue;
                        };
                        let effort = app
                            .root(pane)
                            .map_or_else(|| config.agent().thinking(), |root| root.composer().effort());
                        let mut runtime = open_pane(
                            PaneGeneration {
                                pane,
                                generation: 0,
                            },
                            PaneSession::new(
                                &session_id,
                                Some(&parent_runtime.session_id),
                                Some(parent_sequence),
                                1,
                                parent_runtime.skills_catalog_present,
                            ),
                            &config.with_workspace(app.root(pane).expect("fork pane exists").workspace().to_owned()),
                            PaneSettings {
                                effort,
                                ..parent_runtime.settings
                            },
                            Arc::clone(&parent_runtime.instructions),
                            parent_runtime.subagent_control.clone(),
                            &writer_sender,
                            SessionLock::acquire(config.path(), &session_id)?,
                        )?;
                        let started = runtime
                            .journal_mut()?
                            .persist_start()
                            .await?
                            .expect("a new fork must have a deferred session start");
                        panes.insert(pane, runtime);
                        writers_open = writers_open.saturating_add(1);
                        agent_events::forward(pane, 0, events, agent_event_sender.clone());
                        app.session_opened(pane, session_id.clone(), Vec::new());
                        apply_app_update!(app.update(AppEvent::Transcript {
                            pane,
                            record: started,
                        }));
                        apply_app_update!(app.update(AppEvent::ForkReady { pane }));
                        if let Some(reply) = open_replies.remove(&pane) {
                            drop(reply.send(Ok(Reply::Opened { session: session_id })));
                        }
                    }
                    WorkerEvent::ForkFailed { pane, error } => {
                        let error = error.to_string();
                        if let Some(reply) = open_replies.remove(&pane) {
                            drop(reply.send(Err(CommandError::Failed(error.clone()))));
                        }
                        apply_app_update!(app.update(AppEvent::ForkFailed { pane, error }));
                    }
                    WorkerEvent::ThinkingUpdated {
                        pane,
                        effort,
                        result,
                    } => {
                        let runtime = panes.get_mut(&pane).expect("effort pane must exist");
                        if let Err(error) = result {
                            let effort = runtime.settings.effort;
                            input.get_or_insert_with(EventStream::new);
                            apply_app_update!(app.update(AppEvent::EffortUpdateFailed {
                                pane, effort, error: format!("Could not change effort: {error}"),
                            }));
                            continue;
                        }
                        let previous_effort = runtime.settings.effort;
                        let journal = runtime.journal_mut()?;
                        if journal.is_empty() {
                            journal.set_initial_effort(effort);
                        } else {
                            let record = journal.append_local(LocalEvent::EffortChanged {
                                from: previous_effort,
                                to: effort,
                            })?;
                            schedule(app.update(AppEvent::Transcript { pane, record }), &mut scheduler);
                        }
                        runtime.settings.effort = effort;
                        input.get_or_insert_with(EventStream::new);
                        scheduler.request_immediate(Instant::now());
                    }
                    WorkerEvent::SpeedUpdated { pane, speed, result } => {
                        result?;
                        let runtime = panes.get_mut(&pane).expect("speed pane must exist");
                        let previous = runtime.settings.speed;
                        let journal = runtime.journal_mut()?;
                        if journal.is_empty() {
                            journal.set_initial_speed(speed);
                        } else {
                            let record = journal.append_local(LocalEvent::SpeedChanged {
                                from: previous,
                                to: speed,
                            })?;
                            schedule(app.update(AppEvent::Transcript { pane, record }), &mut scheduler);
                        }
                        runtime.settings.speed = speed;
                        runtime.subagent_control.set_speed(speed);
                        if app.main_pane() == Some(pane) {
                            config.set_speed(speed);
                        }
                        input.get_or_insert_with(EventStream::new);
                        scheduler.request_immediate(Instant::now());
                    }
                }
            }
            result = shell_tasks.join_next(), if !shell_tasks.is_empty() => {
                let Some(result) = result else {
                    continue;
                };
                let Ok((pane, execution)) = result else {
                    continue;
                };
                let Some(runtime) = panes.get_mut(&pane) else {
                    continue;
                };
                runtime.active_shells = runtime.active_shells.saturating_sub(1);
                runtime.pending_shell_context.push(execution.model_context());
                let record = runtime.journal_mut()?.append_local(LocalEvent::ShellFinished {
                    id: execution.id,
                    output: execution.output,
                    exit_code: execution.exit_code,
                    duration_ns: execution.duration_ns,
                    truncated: execution.truncated,
                    error: execution.error,
                })?;
                let submission = if runtime.active_shells == 0 {
                    runtime.pending_submission.take()
                } else {
                    None
                };
                apply_app_update!(app.update(AppEvent::Transcript { pane, record }));
                schedule(app.update(AppEvent::ShellFinished(pane)), &mut scheduler);
                if let Some(submission) = submission {
                    let runtime = panes.get_mut(&pane).expect("shell pane must exist");
                    send_submission(
                        &commands,
                        pane,
                        &mut runtime.pending_shell_context,
                        submission,
                    )?;
                }
            }
            result = web_tasks.join_next(), if !web_tasks.is_empty() => {
                let Some(Ok(completion)) = result else {
                    continue;
                };
                let pane = completion.pane;
                match completion.result {
                    Ok(Some(link)) => {
                        schedule(app.update(AppEvent::ShowWebQr { pane, link }), &mut scheduler);
                    }
                    Ok(None) => {}
                    Err(error) => {
                        schedule(app.update(AppEvent::NotifyError { pane, error }), &mut scheduler);
                    }
                }
            }
            result = memory_tasks.join_next(), if !memory_tasks.is_empty() && lifecycle.is_running() => {
                let Some(Ok(completion)) = result else {
                    continue;
                };
                let (pane, generation) = completion.identity();
                if !memory_generations.is_current(pane, generation) {
                    continue;
                }
                schedule(app.update(completion.into_event()), &mut scheduler);
            }
            result = subagent_shutdowns.join_next(), if !subagent_shutdowns.is_empty() => {
                drop(result);
            }
            result = async {
                editor_task
                    .as_mut()
                    .expect("editor branch is disabled without an editor task")
                    .await
            }, if editor_task.is_some() && lifecycle.is_running() => {
                editor_task = None;
                terminal.resume().map_err(RuntimeError::Terminal)?;
                terminal
                    .report_working_directory(app.root(app.active_pane()).map_or(&workspace, RootNode::workspace))
                    .map_err(RuntimeError::Terminal)?;
                app.refresh_terminal_images();
                input.get_or_insert_with(EventStream::new);
                match result.map_err(RuntimeError::ExternalEditorTask)?? {
                    EditorCompletion::Draft { pane, outcome: EditorOutcome::Updated(draft) } => {
                        schedule(app.update(AppEvent::EditorDraft { pane, draft }), &mut scheduler);
                    }
                    EditorCompletion::Draft { outcome: EditorOutcome::Unchanged, .. }
                    | EditorCompletion::Config
                    | EditorCompletion::File => {}
                }
                scheduler.request_immediate(Instant::now());
            }
            result = async {
                effort_task
                    .as_mut()
                    .expect("effort branch is disabled without an effort task")
                    .await
            }, if effort_task.is_some() && lifecycle.is_running() => {
                effort_task = None;
                let update = result.map_err(RuntimeError::EffortUpdateTask)??;
                config.set_reasoning_mode(update.preferred_reasoning_mode);
                if app.main_pane() == Some(update.pane) {
                    config.set_thinking(update.to);
                    for runtime in panes.values() {
                        runtime.subagent_control.set_max_thinking(update.to.into());
                    }
                }
                app.set_preferred_reasoning_mode(update.preferred_reasoning_mode);
                commands
                    .send(WorkerCommand::SetThinking {
                        pane: update.pane,
                        effort: update.to,
                    })
                    .map_err(|_| RuntimeError::AgentWorkerStopped)?;
            }
            result = async {
                speed_task
                    .as_mut()
                    .expect("speed branch is disabled without a task")
                    .await
            }, if speed_task.is_some() && lifecycle.is_running() => {
                speed_task = None;
                let update = result.map_err(RuntimeError::SpeedUpdateTask)??;
                commands
                    .send(WorkerCommand::SetSpeed {
                        pane: update.pane,
                        speed: update.speed,
                    })
                    .map_err(|_| RuntimeError::AgentWorkerStopped)?;
            }
            result = async {
                handoff_controller
                    .task_mut()
                    .expect("handoff branch is disabled without a task")
                    .await
            }, if handoff_controller.task_mut().is_some() && lifecycle.is_running() => {
                let completion = result.map_err(RuntimeError::HandoffTask)?;
                if !handoff_controller.complete(completion.identity) {
                    continue;
                }
                let pane = completion.identity.pane;
                if !panes.get(&pane).is_some_and(|runtime| {
                    runtime.generation == completion.identity.pane_generation
                }) {
                    continue;
                }
                match completion.result {
                    Ok(prepared) => {
                        let PreparedHandoff {
                            prompt,
                            effort,
                            reasoning_mode,
                            speed,
                            model,
                            configured,
                        } = prepared;
                        let skills = install_agent!(
                            pane,
                            configured,
                            InstalledSession::Fresh,
                            PaneSettings::new(effort, reasoning_mode, speed, model)
                        );
                        app.session_opened(pane, panes[&pane].session_id.clone(), Vec::new());
                        schedule(
                            app.update(AppEvent::HandoffReady {
                                pane,
                                prompt,
                                effort,
                                reasoning_mode,
                                speed,
                                model,
                                skills,
                            }),
                            &mut scheduler,
                        );
                    }
                    Err(AuxiliaryError::Cancelled) => schedule(
                        app.update(AppEvent::HandoffCancelled(pane)),
                        &mut scheduler,
                    ),
                    Err(AuxiliaryError::Failed(error)) => schedule(
                        app.update(AppEvent::HandoffFailed { pane, error }),
                        &mut scheduler,
                    ),
                }
            }
            result = async {
                new_session_task
                    .as_mut()
                    .expect("new-session branch is disabled without a task")
                    .await
            }, if new_session_task.is_some() && lifecycle.is_running() => {
                new_session_task = None;
                input.get_or_insert_with(EventStream::new);
                let (pane, effort, reasoning_mode, speed, model, draft_reset, configured) =
                    result.map_err(RuntimeError::NewSessionTask)?;
                match configured {
                    Ok(_) if app.root(pane).is_none() => {}
                    Ok(configured) => {
                        let skills = install_agent!(
                            pane,
                            configured,
                            InstalledSession::Fresh,
                            PaneSettings::new(effort, reasoning_mode, speed, model)
                        );
                        app.session_opened(pane, panes[&pane].session_id.clone(), Vec::new());
                        schedule(
                            app.update(AppEvent::NewSessionReady {
                                pane,
                                effort,
                                reasoning_mode,
                                speed,
                                model,
                                draft_reset,
                                skills,
                            }),
                            &mut scheduler,
                        );
                    }
                    Err(error) => schedule(
                        app.update(AppEvent::NewSessionFailed {
                            pane,
                            error: error.to_string(),
                        }),
                        &mut scheduler,
                    ),
                }
                scheduler.request_immediate(Instant::now());
            }
            result = async {
                session_list_task
                    .as_mut()
                    .expect("session-list branch is disabled without a task")
                    .await
            }, if session_list_task.is_some() && lifecycle.is_running() => {
                session_list_task = None;
                input.get_or_insert_with(EventStream::new);
                let (pane, sessions) = result.map_err(RuntimeError::SessionTask)?;
                match sessions {
                    Ok(sessions) => schedule(
                        app.update(AppEvent::SessionsLoaded { pane, sessions }),
                        &mut scheduler,
                    ),
                    Err(error) => schedule(
                        app.update(AppEvent::SessionLoadFailed {
                            pane,
                            error: format!("Could not load sessions: {error}"),
                        }),
                        &mut scheduler,
                    ),
                }
                scheduler.request_immediate(Instant::now());
            }
            result = async {
                recent_prompt_task
                    .as_mut()
                    .expect("recent-prompt branch is disabled without a task")
                    .await
            }, if recent_prompt_task.is_some() && lifecycle.is_running() => {
                recent_prompt_task = None;
                let prompts = result.map_err(RuntimeError::SessionTask)?;
                match (prompts, recent_prompt_request.take()) {
                    (Ok(prompts), Some(request)) => {
                        recent_prompt_cache = Some(prompts.clone());
                        input.get_or_insert_with(EventStream::new);
                        schedule(
                            app.update(recent_prompts_loaded_event(prompts, request)),
                            &mut scheduler,
                        );
                    }
                    (Ok(prompts), None) => recent_prompt_cache = Some(prompts),
                    (Err(error), Some(request)) => {
                        input.get_or_insert_with(EventStream::new);
                        schedule(
                            app.update(AppEvent::RecentPromptLoadFailed {
                                pane: request.pane,
                                error: format!("Could not load recent prompts: {error}"),
                            }),
                            &mut scheduler,
                        );
                    }
                    (Err(_), None) => {}
                }
                scheduler.request_immediate(Instant::now());
            }
            result = async {
                resume_session_task
                    .as_mut()
                    .expect("resume-session branch is disabled without a task")
                    .await
            }, if resume_session_task.is_some() && lifecycle.is_running() => {
                resume_session_task = None;
                input.get_or_insert_with(EventStream::new);
                let (pane, effort, preferred_reasoning_mode, speed, restored) =
                    result.map_err(RuntimeError::SessionTask)?;
                match restored {
                    Ok(_) if app.root(pane).is_none() => {}
                    Ok(RestoredSession {
                        configured,
                        lock,
                        records,
                        projection,
                        reasoning_mode,
                        model,
                        next_sequence,
                    }) => {
                        let skills = install_agent!(
                            pane,
                            configured,
                            InstalledSession::Restored { next_sequence, lock },
                            PaneSettings::new(effort, reasoning_mode, speed, model)
                        );
                        app.session_opened(pane, panes[&pane].session_id.clone(), records);
                        schedule(
                            app.update(AppEvent::SessionRestored {
                                pane,
                                projection: Box::new(projection),
                                effort,
                                reasoning_mode,
                                preferred_reasoning_mode,
                                speed,
                                model,
                                skills,
                            }),
                            &mut scheduler,
                        );
                    }
                    Err(error) => schedule(
                        app.update(AppEvent::SessionLoadFailed {
                            pane,
                            error: format!("Could not resume session: {error}"),
                        }),
                        &mut scheduler,
                    ),
                }
                scheduler.request_immediate(Instant::now());
            }
            completion = writer_updates.recv(), if writers_open > 0 => {
                let Some(completion) = completion else {
                    writers_open = 0;
                    continue;
                };
                writers_open = writers_open.saturating_sub(1);
                if let Err(error) = completion.result {
                    writer_error = Some(error);
                    lifecycle.stop();
                    input = None;
                    shutdown.cancel();
                }
                if let Some(runtime) = panes.get_mut(&completion.pane)
                    && runtime.session_id == completion.session_id
                    && runtime.generation == completion.generation
                {
                    runtime.journal = None;
                }
            }
            () = async {
                sleep_until(animation_deadline.expect("animation branch is disabled without a deadline").into()).await;
            }, if animation_deadline.is_some() && editor_task.is_none() && lifecycle.is_running() => {
                schedule(app.update(AppEvent::AnimationFrame(Instant::now())), &mut scheduler);
            }
            () = async {
                sleep_until(render_deadline.expect("deadline branch is disabled without a deadline").into()).await;
            }, if render_deadline.is_some() && editor_task.is_none() && lifecycle.is_running() => {}
        }
    }

    let session_id = app
        .main_pane()
        .and_then(|pane| panes.get(&pane))
        .and_then(PaneRuntime::exit_session_id);
    drop(terminal);
    web_shutdown.cancel();
    if let Some(server) = web_server {
        // A server that does not stop promptly must not hold the terminal's exit.
        drop(tokio::time::timeout(WEB_SHUTDOWN_GRACE, server).await);
    }
    if let Some(error) = writer_error {
        return Err(error.into());
    }
    worker_error.map_or(Ok(session_id), |error| Err(error.into()))
}

pub(crate) fn ensure_interactive() -> Result<()> {
    validate_interactive(io::stdin().is_terminal(), io::stdout().is_terminal())
}

/// How a pane's newly configured agent relates to stored history.
enum InstalledSession {
    Fresh,
    Restored {
        next_sequence: u64,
        lock: SessionLock,
    },
}

/// Installs `configured` as `pane`'s agent, replacing and closing the pane's previous session if it
/// has one.
#[allow(clippy::too_many_arguments)]
fn install_agent(
    pane: PaneId,
    configured: ConfiguredAgent,
    history: InstalledSession,
    settings: PaneSettings,
    config: &Config,
    panes: &mut HashMap<PaneId, PaneRuntime>,
    commands: &mpsc::UnboundedSender<WorkerCommand>,
    agent_event_sender: &mpsc::UnboundedSender<ForwardedAgentEvent>,
    subagent_sender: &mpsc::UnboundedSender<ForwardedSubagentUpdate>,
    writer_sender: &mpsc::UnboundedSender<WriterCompletion>,
    writers_open: &mut usize,
    subagent_shutdowns: &mut JoinSet<()>,
) -> Result<Arc<[Skill]>> {
    let config = &config.with_workspace(configured.workspace.clone());
    let ConfiguredAgent {
        workspace: _,
        agent,
        context,
        events,
        instructions,
        skills,
        memory_enabled,
        subagent_updates,
        subagent_control,
    } = configured;
    let session_id = events.request_id().to_owned();
    let previous = panes.get_mut(&pane);
    let replacing = previous.is_some();
    let generation = match previous {
        Some(runtime) => {
            schedule_subagent_shutdown(runtime, subagent_shutdowns);
            close_pane_journal(runtime, SessionOutcome::Closed, None)?;
            runtime.generation.saturating_add(1)
        }
        None => 0,
    };
    let skills_catalog_present = !skills.is_empty();
    let (session, lock, memory_review) = match history {
        InstalledSession::Fresh => (
            PaneSession::new(&session_id, None, None, 1, skills_catalog_present),
            SessionLock::acquire(config.path(), &session_id)?,
            worker::MemoryReviewState::fresh(memory_enabled),
        ),
        InstalledSession::Restored {
            next_sequence,
            lock,
        } => (
            PaneSession::resumed(&session_id, next_sequence, skills_catalog_present),
            lock,
            worker::MemoryReviewState::restored(memory_enabled),
        ),
    };
    panes.insert(
        pane,
        open_pane(
            PaneGeneration { pane, generation },
            session,
            config,
            settings,
            instructions,
            subagent_control.clone(),
            writer_sender,
            lock,
        )?,
    );
    *writers_open = writers_open.saturating_add(1);
    agent_events::forward(pane, generation, events, agent_event_sender.clone());
    subagent_updates::forward(
        subagent_control.runtime_id(),
        subagent_updates,
        subagent_sender.clone(),
    );
    let command = if replacing {
        WorkerCommand::ReplaceAgent {
            pane,
            agent,
            context,
            memory_review,
        }
    } else {
        WorkerCommand::OpenAgent {
            pane,
            agent,
            context,
            memory_review,
        }
    };
    commands
        .send(command)
        .map_err(|_| RuntimeError::AgentWorkerStopped)?;
    Ok(skills)
}

fn validate_interactive(stdin: bool, stdout: bool) -> Result<()> {
    if stdin && stdout {
        return Ok(());
    }
    Err(RuntimeError::InteractiveTerminal.into())
}

#[allow(clippy::too_many_arguments)]
fn open_pane(
    identity: PaneGeneration,
    session: PaneSession<'_>,
    config: &Config,
    settings: PaneSettings,
    instructions: Arc<str>,
    subagent_control: Subagents,
    writer_updates: &mpsc::UnboundedSender<WriterCompletion>,
    lock: SessionLock,
) -> Result<PaneRuntime> {
    let PaneGeneration { pane, generation } = identity;
    let PaneSession {
        id: session_id,
        parent_id: parent_session_id,
        parent_sequence,
        next_sequence,
        origin,
        skills_catalog_present,
    } = session;
    let (mut journal, writer) =
        TranscriptJournal::open_at(config.path(), session_id, next_sequence)?;
    let writer_path = journal.path().to_path_buf();
    let persisted_transcript = journal.persistence_flag();
    journal.defer_start(SessionStarted {
        session_id: session_id.to_owned(),
        parent_session_id: parent_session_id.map(str::to_owned),
        parent_sequence,
        model: settings.model.to_string(),
        effort: settings.effort,
        reasoning_mode: settings.reasoning_mode,
        speed: settings.speed,
        workspace: config.agent().workspace().to_path_buf(),
        application_version: env!("CARGO_PKG_VERSION").to_owned(),
    });

    let updates = writer_updates.clone();
    let completion_session_id = session_id.to_owned();
    tokio::spawn(async move {
        let result = writer
            .into_task()
            .await
            .map_err(TranscriptError::WriterTask)
            .and_then(|result| result);
        drop(updates.send(WriterCompletion {
            pane,
            session_id: completion_session_id,
            generation,
            result,
        }));
    });

    Ok(PaneRuntime {
        session_id: session_id.to_owned(),
        instructions,
        skills_catalog_present,
        origin,
        journal: Some(journal),
        writer_path,
        persisted_transcript,
        agent: AgentState::Running,
        next_turn: 1,
        next_shell: 1,
        pending_shell_context: Vec::new(),
        pending_submission: None,
        settings,
        active_shells: 0,
        generation,
        subagent_control,
        _lock: lock,
    })
}

fn close_journals(
    panes: &mut HashMap<PaneId, PaneRuntime>,
    worker_error: Option<&nanocodex::NanocodexError>,
) -> Result<()> {
    let outcome = if worker_error.is_some() {
        SessionOutcome::Failed
    } else {
        SessionOutcome::Cancelled
    };
    for runtime in panes.values_mut() {
        close_pane_journal(runtime, outcome, worker_error.map(ToString::to_string))?;
    }
    Ok(())
}

fn schedule_subagent_shutdown(runtime: &PaneRuntime, tasks: &mut JoinSet<()>) {
    let control = runtime.subagent_control.clone();
    let root_session_id = runtime.session_id.clone();
    tasks.spawn(async move {
        control.close_all(&root_session_id).await;
    });
}

fn close_pane_journal(
    runtime: &mut PaneRuntime,
    outcome: SessionOutcome,
    error: Option<String>,
) -> Result<()> {
    let Some(mut journal) = runtime.journal.take() else {
        return Ok(());
    };
    if journal.is_empty() {
        return Ok(());
    }
    journal.append_local(LocalEvent::SessionEnded(SessionEnded { outcome, error }))?;
    drop(journal);
    Ok(())
}

fn merge_recent_prompts(
    mut persisted: Vec<RecentPrompt>,
    current: Vec<RecentPromptDraft>,
    session_id: &str,
    workspace: &Path,
) -> Vec<RecentPrompt> {
    persisted.retain(|prompt| prompt.session_id != session_id);
    let mut prompts = current
        .into_iter()
        .rev()
        .map(|prompt| RecentPrompt {
            text: prompt.text,
            recorded_at_unix_ms: prompt.recorded_at_unix_ms,
            session_id: session_id.to_owned(),
            workspace: workspace.to_path_buf(),
        })
        .collect::<Vec<_>>();
    prompts.extend(persisted);
    prompts.sort_by_key(|prompt| std::cmp::Reverse(prompt.recorded_at_unix_ms));
    prompts
}

fn remember_recent_prompt(cache: &mut Option<Vec<RecentPrompt>>, prompt: RecentPrompt) {
    let Some(cache) = cache else {
        return;
    };
    let index = cache
        .partition_point(|existing| existing.recorded_at_unix_ms >= prompt.recorded_at_unix_ms);
    cache.insert(index, prompt);
    cache.truncate(session::MAX_RECENT_PROMPTS);
}

fn recent_prompts_loaded_event(
    persisted: Vec<RecentPrompt>,
    request: RecentPromptRequest,
) -> AppEvent {
    let prompts = merge_recent_prompts(
        persisted,
        request.current_prompts,
        &request.session_id,
        &request.workspace,
    );
    AppEvent::RecentPromptsLoaded {
        pane: request.pane,
        session_id: request.session_id,
        prompts,
    }
}

struct EffectContext<'a> {
    app: &'a mut AppNode,
    commands: &'a tokio::sync::mpsc::UnboundedSender<WorkerCommand>,
    workspace: &'a Path,
    config: &'a mut Config,
    shutdown: &'a CancellationToken,
    input: &'a mut Option<EventStream>,
    editor_task: &'a mut Option<EditorTask>,
    effort_task: &'a mut Option<EffortUpdateTask>,
    speed_task: &'a mut Option<SpeedUpdateTask>,
    new_session_task: &'a mut Option<NewSessionTask>,
    session_list_task: &'a mut Option<SessionListTask>,
    recent_prompt_task: &'a mut Option<RecentPromptTask>,
    recent_prompt_cache: &'a mut Option<Vec<RecentPrompt>>,
    recent_prompt_request: &'a mut Option<RecentPromptRequest>,
    handoff_controller: &'a mut HandoffController,
    web_status: &'a watch::Receiver<WebStatus>,
    open_tasks: &'a mut JoinSet<OpenTask>,
    resume_session_task: &'a mut Option<ResumeSessionTask>,
    terminal: &'a mut TerminalSession,
    scheduler: &'a mut RenderScheduler,
    panes: &'a mut HashMap<PaneId, PaneRuntime>,
    shell_tasks: &'a mut JoinSet<(PaneId, ShellExecution)>,
    web_tasks: &'a mut JoinSet<WebTaskCompletion>,
    memory_store: &'a mut Option<SelectedMemoryStore>,
    memory_tasks: &'a mut JoinSet<MemoryCompletion>,
    memory_generations: &'a mut MemoryGenerations,
    subagent_shutdowns: &'a mut JoinSet<()>,
}

async fn apply_update(
    update: ComponentUpdate<AppEffect>,
    mut context: EffectContext<'_>,
) -> Result<()> {
    for effect in update.effects {
        match effect {
            AppEffect::OpenFork { pane, parent } => {
                request_fork(context.panes, context.commands, pane, parent).await?;
            }
            AppEffect::StartSession { pane, model } => {
                let settings = fresh_settings(context.config, model);
                context
                    .open_tasks
                    .spawn(configure_fresh(context.config.clone(), pane, settings));
            }
            AppEffect::ClosePane(pane) => {
                if let Some(runtime) = context.panes.get_mut(&pane) {
                    schedule_subagent_shutdown(runtime, context.subagent_shutdowns);
                    if runtime.agent == AgentState::Running {
                        runtime.agent = AgentState::Closing;
                    }
                }
                context
                    .commands
                    .send(WorkerCommand::ClosePane(pane))
                    .map_err(|_| RuntimeError::AgentWorkerStopped)?;
            }
            AppEffect::SetTheme(mode) => context.config.persist_theme_mode(mode)?,
            AppEffect::Shutdown => context.shutdown.cancel(),
            AppEffect::Pane { pane, effect } => {
                apply_pane_effect(pane, effect, &mut context)?;
            }
        }
    }
    request_render(update.render, context.scheduler);
    Ok(())
}

/// Reloads the configuration and applies what can change in-process. The outcome is shown in the
/// terminal as a notification on `pane` and returned to a web caller.
fn reload_config(
    context: &mut EffectContext<'_>,
    pane: PaneId,
) -> std::result::Result<(), CommandError> {
    fn refuse(
        context: &mut EffectContext<'_>,
        pane: PaneId,
        error: String,
    ) -> std::result::Result<(), CommandError> {
        schedule(
            context.app.update(AppEvent::ConfigReloadFailed {
                pane,
                error: error.clone(),
            }),
            context.scheduler,
        );
        Err(CommandError::Failed(error))
    }

    let reload = match context.config.reload() {
        Ok(reload) => reload,
        Err(error) => return refuse(context, pane, format!("Could not reload config: {error}")),
    };
    let (config, workspace_changed) = reload.into_parts();
    if let Err(error) = context
        .panes
        .values()
        .try_for_each(|runtime| config.claude().ensure_model_enabled(runtime.settings.model))
    {
        return refuse(
            context,
            pane,
            format!("Could not reload config while a Claude session is open: {error}"),
        );
    }
    let selected_memory_store =
        match crate::core::configured_memory_store(&config, context.workspace) {
            Ok(store) => store,
            Err(error) => {
                return refuse(
                    context,
                    pane,
                    format!("Could not apply memory configuration: {error}"),
                );
            }
        };
    let theme = config.theme().clone();
    let tui = *config.tui();
    let preferred_reasoning_mode = config.agent().reasoning_mode();
    let memory_enabled = config.memory().enabled();
    context.memory_generations.invalidate_all();
    *context.memory_store = selected_memory_store;
    context
        .app
        .set_max_subagents(config.agent().max_subagents());
    context.app.set_claude_enabled(config.claude().enabled());
    for runtime in context.panes.values() {
        apply_subagent_config(&runtime.subagent_control, &config);
    }
    *context.config = config;
    let message = if workspace_changed {
        "Reloaded config · theme, UI, memory browser, and subagent limits applied · agent/auth/tool settings apply to new sessions · workspace requires restart"
    } else {
        "Reloaded config · theme, UI, memory browser, and subagent limits applied · agent/auth/tool settings apply to new sessions"
    };
    schedule(
        context.app.update(AppEvent::ConfigReloaded {
            pane,
            theme,
            tui,
            preferred_reasoning_mode,
            memory_enabled,
            message: message.to_owned(),
        }),
        context.scheduler,
    );
    Ok(())
}

fn apply_pane_effect(
    pane: PaneId,
    effect: components::RootEffect,
    context: &mut EffectContext<'_>,
) -> Result<()> {
    let workspace = context
        .app
        .root(pane)
        .expect("effect pane exists")
        .workspace()
        .to_owned();
    match effect {
        components::RootEffect::Submit(prompt) => {
            let runtime = context
                .panes
                .get_mut(&pane)
                .expect("UI pane must have a runtime");
            let id = TurnId::new(runtime.next_turn);
            runtime.next_turn = runtime.next_turn.saturating_add(1);
            let record = runtime
                .journal_mut()?
                .append_local(LocalEvent::UserSubmitted {
                    id,
                    text: prompt.display_text().to_owned(),
                })?;
            remember_recent_prompt(
                context.recent_prompt_cache,
                RecentPrompt {
                    text: prompt.display_text().to_owned(),
                    recorded_at_unix_ms: record.recorded_at_unix_ms(),
                    session_id: runtime.session_id.clone(),
                    workspace: workspace.clone(),
                },
            );
            schedule(
                context.app.update(AppEvent::Transcript { pane, record }),
                context.scheduler,
            );
            let submission = PendingSubmission { id, prompt };
            if runtime.active_shells == 0 {
                send_submission(
                    context.commands,
                    pane,
                    &mut runtime.pending_shell_context,
                    submission,
                )?;
            } else {
                debug_assert!(runtime.pending_submission.is_none());
                runtime.pending_submission = Some(submission);
            }
        }
        components::RootEffect::ContinueSubagent(prompt) => {
            let runtime = context
                .panes
                .get_mut(&pane)
                .expect("UI pane must have a runtime");
            let id = TurnId::new(runtime.next_turn);
            runtime.next_turn = runtime.next_turn.saturating_add(1);
            let submission = PendingSubmission { id, prompt };
            if runtime.active_shells == 0 {
                send_submission(
                    context.commands,
                    pane,
                    &mut runtime.pending_shell_context,
                    submission,
                )?;
            } else {
                debug_assert!(runtime.pending_submission.is_none());
                runtime.pending_submission = Some(submission);
            }
        }
        components::RootEffect::Compact => {
            let runtime = context
                .panes
                .get_mut(&pane)
                .expect("UI pane must have a runtime");
            let record = runtime
                .journal_mut()?
                .append_local(LocalEvent::CompactionStarted)?;
            schedule(
                context.app.update(AppEvent::Transcript { pane, record }),
                context.scheduler,
            );
            context
                .commands
                .send(WorkerCommand::Compact(pane))
                .map_err(|_| RuntimeError::AgentWorkerStopped)?;
        }
        components::RootEffect::Reflect(instructions) => {
            let runtime = context
                .panes
                .get_mut(&pane)
                .expect("UI pane must have a runtime");
            debug_assert_eq!(runtime.active_shells, 0);
            let id = TurnId::new(runtime.next_turn);
            runtime.next_turn = runtime.next_turn.saturating_add(1);
            let record = runtime
                .journal_mut()?
                .append_local(LocalEvent::ReflectionStarted { id })?;
            schedule(
                context.app.update(AppEvent::Transcript { pane, record }),
                context.scheduler,
            );
            context
                .commands
                .send(WorkerCommand::Reflect {
                    pane,
                    id,
                    instructions,
                    context: ReflectionContext::new(context.config.path(), &workspace),
                })
                .map_err(|_| RuntimeError::AgentWorkerStopped)?;
        }
        components::RootEffect::RunShell(command) => {
            let runtime = context
                .panes
                .get_mut(&pane)
                .expect("UI pane must have a runtime");
            let id = ShellId::new(runtime.next_shell);
            runtime.next_shell = runtime.next_shell.saturating_add(1);
            runtime.active_shells = runtime.active_shells.saturating_add(1);
            let record = runtime
                .journal_mut()?
                .append_local(LocalEvent::ShellStarted {
                    id,
                    command: command.clone(),
                    workspace: workspace.clone(),
                })?;
            schedule(
                context.app.update(AppEvent::Transcript { pane, record }),
                context.scheduler,
            );
            let workspace = workspace.clone();
            context
                .shell_tasks
                .spawn(async move { (pane, shell::execute(id, command, workspace).await) });
        }
        components::RootEffect::OpenLink(destination) if is_web_link(&destination) => {
            context.web_tasks.spawn(async move {
                WebTaskCompletion {
                    pane,
                    result: crate::app::browser::open(&destination)
                        .await
                        .map(|()| None)
                        .map_err(|error| format!("Could not open link: {error}")),
                }
            });
        }
        editor_effect @ (components::RootEffect::OpenDraftEditor
        | components::RootEffect::OpenConfigEditor
        | components::RootEffect::OpenLink(_)) => {
            context.terminal.suspend().map_err(RuntimeError::Terminal)?;
            *context.input = None;
            let target = match editor_effect {
                components::RootEffect::OpenDraftEditor => EditorTarget::Draft {
                    pane,
                    text: context
                        .app
                        .root(pane)
                        .expect("editor pane must exist")
                        .composer()
                        .draft()
                        .to_owned(),
                },
                components::RootEffect::OpenConfigEditor => {
                    EditorTarget::Config(context.config.path().to_path_buf())
                }
                components::RootEffect::OpenLink(destination) => {
                    EditorTarget::File(local_link_path(&destination, &workspace))
                }
                _ => unreachable!("editor effect pattern is exhaustive"),
            };
            let workspace = workspace.clone();
            *context.editor_task = Some(tokio::spawn(async move {
                match target {
                    EditorTarget::Draft { pane, text } => {
                        let outcome = editor::edit(&text, &workspace).await?;
                        Ok(EditorCompletion::Draft { pane, outcome })
                    }
                    EditorTarget::Config(path) => editor::edit_config(&path, &workspace)
                        .await
                        .map(|()| EditorCompletion::Config),
                    EditorTarget::File(path) => editor::open_file(&path, &workspace)
                        .await
                        .map(|()| EditorCompletion::File),
                }
            }));
        }
        components::RootEffect::SetEffort {
            effort,
            reasoning_mode,
        } => {
            *context.input = None;
            let config = context.config.clone();
            let is_main = context.app.main_pane() == Some(pane);
            *context.effort_task = Some(tokio::task::spawn_blocking(move || {
                if is_main {
                    config.persist_thinking(effort)?;
                }
                config.persist_reasoning_mode(reasoning_mode)?;
                Ok(EffortUpdate {
                    pane,
                    to: effort,
                    preferred_reasoning_mode: reasoning_mode,
                })
            }));
        }
        components::RootEffect::SetModel(model) => {
            *context.input = None;
            let root = context.app.root(pane).expect("model pane must exist");
            let effort = root.composer().effort();
            let reasoning_mode = supported_reasoning_mode(model, root.preferred_reasoning_mode());
            let speed = context.config.agent().speed();
            let config = context.config.with_workspace(workspace.clone());
            *context.new_session_task = Some(tokio::task::spawn_blocking(move || {
                let configured =
                    ConfiguredAgent::from_config_with_model(&config, effort, reasoning_mode, model);
                (
                    pane,
                    effort,
                    reasoning_mode,
                    speed,
                    model,
                    components::DraftReset::Preserve,
                    configured,
                )
            }));
        }
        components::RootEffect::SetSpeed(speed) => {
            *context.input = None;
            let config = (context.app.main_pane() == Some(pane)).then(|| context.config.clone());
            *context.speed_task = Some(tokio::task::spawn_blocking(move || {
                if let Some(config) = config {
                    config.persist_speed(speed)?;
                }
                Ok(SpeedUpdate { pane, speed })
            }));
        }
        components::RootEffect::SetMaxSubagents(limit) => {
            context.config.persist_max_subagents(limit)?;
            context.config.set_max_subagents(limit);
            context.app.set_max_subagents(limit);
            for runtime in context.panes.values() {
                runtime.subagent_control.set_max_concurrency(limit);
            }
        }
        components::RootEffect::LoadMemories => {
            let Some(store) = context.memory_store.clone() else {
                schedule(
                    context.app.update(AppEvent::MemoryLoadFailed {
                        pane,
                        source: MemorySource::Local,
                        access: None,
                        error: "Memory is disabled. Enable it with memory.enabled = true."
                            .to_owned(),
                    }),
                    context.scheduler,
                );
                return Ok(());
            };
            let generation = context.memory_generations.next(pane);
            context.memory_tasks.spawn(async move {
                run_memory_operation(pane, generation, &store, MemoryOperation::List).await
            });
        }
        components::RootEffect::DeleteMemory(key) => {
            let Some(store) = context.memory_store.clone() else {
                schedule(
                    context.app.update(AppEvent::MemoryDeleteFailed {
                        pane,
                        error: "Memory was disabled before the deletion completed.".to_owned(),
                        conflict: false,
                    }),
                    context.scheduler,
                );
                return Ok(());
            };
            let generation = context.memory_generations.next(pane);
            context.memory_tasks.spawn(async move {
                run_memory_operation(pane, generation, &store, MemoryOperation::Delete(key)).await
            });
        }
        components::RootEffect::ReloadConfig => {
            // The terminal reports the outcome through the notification reload_config schedules.
            let _ = reload_config(context, pane);
        }
        components::RootEffect::NewSession(model) => {
            *context.input = None;
            let PaneSettings {
                effort,
                reasoning_mode,
                speed,
                model,
            } = fresh_settings(context.config, model);
            let config = context.config.with_workspace(workspace.clone());
            *context.new_session_task = Some(tokio::task::spawn_blocking(move || {
                let configured = ConfiguredAgent::from_config_with_session(
                    &config,
                    effort,
                    reasoning_mode,
                    model,
                    None,
                    None,
                );
                (
                    pane,
                    effort,
                    reasoning_mode,
                    speed,
                    model,
                    components::DraftReset::Clear,
                    configured,
                )
            }));
        }
        components::RootEffect::LoadSessions(kind) => {
            *context.input = None;
            let config_path = context.config.path().to_path_buf();
            let workspace = workspace.clone();
            let active_session_id = context
                .panes
                .get(&pane)
                .expect("session-list pane must exist")
                .session_id
                .clone();
            *context.session_list_task = Some(tokio::spawn(async move {
                let resumable_only = matches!(kind, components::SessionListKind::Resume);
                let sessions = session::list_async(config_path, workspace, resumable_only)
                    .await
                    .map(|mut sessions| {
                        sessions.retain(|session| session.session_id != active_session_id);
                        sessions
                    });
                (pane, sessions.map_err(Into::into))
            }));
        }
        components::RootEffect::LoadRecentPrompts(current_prompts) => {
            let session_id = context
                .panes
                .get(&pane)
                .expect("recent-prompt pane must exist")
                .session_id
                .clone();
            let request = RecentPromptRequest {
                pane,
                session_id,
                workspace: workspace.clone(),
                current_prompts,
            };
            if let Some(prompts) = context.recent_prompt_cache.clone() {
                schedule(
                    context
                        .app
                        .update(recent_prompts_loaded_event(prompts, request)),
                    context.scheduler,
                );
                return Ok(());
            }

            *context.input = None;
            *context.recent_prompt_request = Some(request);
            if context.recent_prompt_task.is_none() {
                let config_path = context.config.path().to_path_buf();
                *context.recent_prompt_task = Some(tokio::spawn(async move {
                    session::load_recent_prompts_async(config_path)
                        .await
                        .map_err(Into::into)
                }));
            }
        }
        components::RootEffect::Handoff => start_handoff(context, pane),
        components::RootEffect::OpenWebInterface { install } => {
            let home = context
                .config
                .path()
                .parent()
                .unwrap_or(Path::new("."))
                .to_owned();
            let download = match crate::web::WebAssets::locate(&home) {
                Ok(crate::web::Located::Ready(_)) => false,
                Ok(crate::web::Located::Absent)
                    if !crate::app::installation::current().is_development() && !install =>
                {
                    schedule(
                        context.app.update(AppEvent::ConfirmWebInstall { pane }),
                        context.scheduler,
                    );
                    return Ok(());
                }
                Ok(crate::web::Located::Absent)
                    if !crate::app::installation::current().is_development() =>
                {
                    true
                }
                Ok(crate::web::Located::Absent) => {
                    let error = "You are running a development build of Tact, which cannot download the web interface automatically. Run `cd web && bun install --frozen-lockfile && just install-dev`, or set TACT_WEB_ASSETS to the absolute `web/dist` path."
                        .to_owned();
                    schedule(
                        context.app.update(AppEvent::NotifyError { pane, error }),
                        context.scheduler,
                    );
                    return Ok(());
                }
                Err(error) => {
                    schedule(
                        context.app.update(AppEvent::NotifyError {
                            pane,
                            error: format!("Could not load the web interface: {error}"),
                        }),
                        context.scheduler,
                    );
                    return Ok(());
                }
            };
            let status = context.web_status.clone();
            let enabled = context.config.web().enabled();
            context.web_tasks.spawn(async move {
                let result = async {
                    if download {
                        crate::web::WebAssets::download(&home)
                            .await
                            .map_err(|error| {
                                format!("Could not install the web interface: {error}")
                            })?;
                    }
                    let url = web_link(&status, enabled)?;
                    crate::app::browser::open(&url).await.map_err(|error| {
                        format!("Could not open the browser: {error}. Use Copy web link instead.")
                    })?;
                    Ok(None)
                }
                .await;
                WebTaskCompletion { pane, result }
            });
        }
        components::RootEffect::CopyWebLink => {
            let copied = web_link(context.web_status, context.config.web().enabled())
                .and_then(|url| copy_selection(context.terminal, &url));
            let event = match copied {
                Ok(()) => AppEvent::NotifySuccess {
                    pane,
                    message: "Copied the web link. It signs in to Tact; share it carefully."
                        .to_owned(),
                },
                Err(error) => AppEvent::NotifyError { pane, error },
            };
            schedule(context.app.update(event), context.scheduler);
        }
        components::RootEffect::ShowWebQr => {
            let status = context.web_status.clone();
            let enabled = context.config.web().enabled();
            context.web_tasks.spawn(async move {
                let result = async {
                    let link = web_link(&status, enabled)?;
                    phone_link(link, &status).await.map(Some)
                }
                .await;
                WebTaskCompletion { pane, result }
            });
        }
        components::RootEffect::ResumeSession(session_id) => {
            if let Some(live) = context.app.pane_for_session(&session_id) {
                schedule(
                    context.app.update(AppEvent::SessionLoadFailed {
                        pane,
                        error: "That session is already open here.".to_owned(),
                    }),
                    context.scheduler,
                );
                context.app.activate(live);
                return Ok(());
            }
            let lock = match SessionLock::acquire(context.config.path(), &session_id) {
                Ok(lock) => lock,
                Err(error) => {
                    schedule(
                        context.app.update(AppEvent::SessionLoadFailed {
                            pane,
                            error: format!("Could not resume session: {error}"),
                        }),
                        context.scheduler,
                    );
                    return Ok(());
                }
            };
            *context.input = None;
            let effort = context.config.agent().thinking();
            let preferred_reasoning_mode = context.config.agent().reasoning_mode();
            let speed = context.config.agent().speed();
            let config = context.config.clone();
            *context.resume_session_task = Some(tokio::spawn(async move {
                let restored = restore_session(config, session_id, effort, lock).await;
                (pane, effort, preferred_reasoning_mode, speed, restored)
            }));
        }
        components::RootEffect::Copy(text) => match copy_selection(context.terminal, &text) {
            Ok(()) => schedule(
                context.app.update(AppEvent::NotifySuccess {
                    pane,
                    message: "Copied to clipboard.".to_owned(),
                }),
                context.scheduler,
            ),
            Err(error) => schedule(
                context.app.update(AppEvent::NotifyError { pane, error }),
                context.scheduler,
            ),
        },
        components::RootEffect::Steer { id, prompt } => {
            let runtime = context.panes.get_mut(&pane).expect("steer pane must exist");
            let fallback_id = TurnId::new(runtime.next_turn);
            runtime.next_turn = runtime.next_turn.saturating_add(1);
            context
                .commands
                .send(WorkerCommand::Steer {
                    pane,
                    queue_id: id,
                    fallback_id,
                    prompt,
                })
                .map_err(|_| RuntimeError::AgentWorkerStopped)?;
        }
        components::RootEffect::PersistSteer(text) => {
            let runtime = context.panes.get_mut(&pane).expect("steer pane must exist");
            let record = runtime
                .journal_mut()?
                .append_local(LocalEvent::UserSteered { text: text.clone() })?;
            remember_recent_prompt(
                context.recent_prompt_cache,
                RecentPrompt {
                    text,
                    recorded_at_unix_ms: record.recorded_at_unix_ms(),
                    session_id: runtime.session_id.clone(),
                    workspace: workspace.clone(),
                },
            );
            schedule(
                context.app.update(AppEvent::Transcript { pane, record }),
                context.scheduler,
            );
        }
        components::RootEffect::CancelTurns => {
            let runtime = context.panes.get(&pane).expect("cancelled pane must exist");
            let subagents = runtime.subagent_control.clone();
            let root_session_id = runtime.session_id.clone();
            tokio::spawn(async move { subagents.cancel_all(&root_session_id).await });
            context
                .commands
                .send(WorkerCommand::CancelAll(pane))
                .map_err(|_| RuntimeError::AgentWorkerStopped)?;
        }
        components::RootEffect::CancelHandoff => {
            context.handoff_controller.cancel();
        }
        components::RootEffect::Fork
        | components::RootEffect::OpenSessions
        | components::RootEffect::SetTheme(_)
        | components::RootEffect::Shutdown => {
            unreachable!("application effects are handled before pane dispatch")
        }
    }
    Ok(())
}

/// Copies text through the clipboard channels available to Tact.
///
/// On non-macOS and remote macOS sessions, Tact first tries the tmux server that
/// directly contains it. `load-buffer -w` asks that server to forward the selection
/// to its terminal when supported. Local macOS retains its native pasteboard-first path.
fn copy_selection(terminal: &mut TerminalSession, text: &str) -> std::result::Result<(), String> {
    let use_tmux = std::env::var_os("TMUX").is_some();
    #[cfg(target_os = "macos")]
    let use_tmux = use_tmux && is_remote_session();

    copy_selection_with(
        use_tmux,
        || clipboard::copy_to_tmux(text).map_err(|error| error.to_string()),
        || copy_platform_selection(terminal, text),
    )
}

fn copy_selection_with(
    use_tmux: bool,
    tmux_copy: impl FnOnce() -> std::result::Result<(), String>,
    platform_copy: impl FnOnce() -> std::result::Result<(), String>,
) -> std::result::Result<(), String> {
    if use_tmux {
        match tmux_copy() {
            Ok(()) => return Ok(()),
            Err(tmux_error) => {
                return platform_copy().map_err(|platform_error| {
                    format!(
                        "Could not copy selection to tmux: {tmux_error}; \
                     platform fallback failed: {platform_error}"
                    )
                });
            }
        }
    }

    platform_copy()
}

#[cfg(target_os = "macos")]
fn is_remote_session() -> bool {
    std::env::var_os("SSH_TTY").is_some() || std::env::var_os("SSH_CONNECTION").is_some()
}

#[cfg(not(target_os = "macos"))]
fn copy_platform_selection(
    terminal: &mut TerminalSession,
    text: &str,
) -> std::result::Result<(), String> {
    match terminal.copy_to_clipboard(text) {
        Ok(()) => Ok(()),
        Err(terminal_error) => clipboard::copy_text(text).map_err(|native_error| {
            format!(
                "Could not copy selection: terminal copy failed: {terminal_error}; \
                 native fallback failed: {native_error}"
            )
        }),
    }
}

#[cfg(target_os = "macos")]
fn copy_platform_selection(
    terminal: &mut TerminalSession,
    text: &str,
) -> std::result::Result<(), String> {
    match clipboard::copy_text(text) {
        Ok(()) => Ok(()),
        Err(native_error) => terminal.copy_to_clipboard(text).map_err(|terminal_error| {
            format!(
                "Could not copy selection: {native_error}; \
                 terminal fallback failed: {terminal_error}"
            )
        }),
    }
}

fn start_handoff(context: &mut EffectContext<'_>, pane: PaneId) {
    let Some(runtime) = context.panes.get_mut(&pane) else {
        schedule(
            context.app.update(AppEvent::HandoffFailed {
                pane,
                error: "Could not prepare handoff: session pane is no longer available".to_owned(),
            }),
            context.scheduler,
        );
        return;
    };
    let pane_generation = runtime.generation;
    let id = TurnId::new(runtime.next_turn);
    runtime.next_turn = runtime.next_turn.saturating_add(1);
    let model = runtime.settings.model;
    let commands = context.commands.clone();
    let config = context.config.with_workspace(
        context
            .app
            .root(pane)
            .expect("handoff pane exists")
            .workspace()
            .to_owned(),
    );
    let started =
        context
            .handoff_controller
            .start(pane, pane_generation, move |identity, cancellation| {
                let (completion, result) = tokio::sync::oneshot::channel();
                let sent = commands.send(WorkerCommand::Auxiliary {
                    pane,
                    id,
                    prompt: HANDOFF_PROMPT.to_owned().into(),
                    context: AuxiliaryContext::CurrentConversation,
                    shutdown: cancellation.clone(),
                    completion,
                });
                tokio::spawn(async move {
                    let result = if sent.is_err() {
                        Err(AuxiliaryError::Failed(
                            "agent worker stopped before the handoff could start".to_owned(),
                        ))
                    } else {
                        match result.await {
                            Ok(result) => result,
                            Err(_) if cancellation.is_cancelled() => Err(AuxiliaryError::Cancelled),
                            Err(_) => Err(AuxiliaryError::Failed(
                                "agent worker stopped before the handoff completed".to_owned(),
                            )),
                        }
                    };
                    let result = prepare_handoff(result, config, model, cancellation).await;
                    HandoffCompletion { identity, result }
                })
            });
    if started.is_none() {
        schedule(
            context.app.update(AppEvent::HandoffFailed {
                pane,
                error: "A handoff is already being prepared.".to_owned(),
            }),
            context.scheduler,
        );
    }
}

async fn prepare_handoff(
    result: std::result::Result<String, AuxiliaryError>,
    config: Config,
    model: Model,
    cancellation: CancellationToken,
) -> std::result::Result<PreparedHandoff, AuxiliaryError> {
    let prompt = result?;
    if prompt.trim().is_empty() {
        return Err(AuxiliaryError::Failed(
            "The handoff agent returned an empty continuation prompt.".to_owned(),
        ));
    }
    if cancellation.is_cancelled() {
        return Err(AuxiliaryError::Cancelled);
    }

    let effort = config.agent().thinking();
    let reasoning_mode = supported_reasoning_mode(model, config.agent().reasoning_mode());
    let speed = config.agent().speed();
    let task = tokio::task::spawn_blocking(move || {
        ConfiguredAgent::from_config_with_session(
            &config,
            effort,
            reasoning_mode,
            model,
            None,
            None,
        )
    });
    let configured = tokio::select! {
        result = task => result
            .map_err(|error| AuxiliaryError::Failed(format!("handoff session task failed: {error}")))?
            .map_err(|error| AuxiliaryError::Failed(format!("Could not start handoff session: {error}")))?,
        () = cancellation.cancelled() => return Err(AuxiliaryError::Cancelled),
    };
    if cancellation.is_cancelled() {
        return Err(AuxiliaryError::Cancelled);
    }
    Ok(PreparedHandoff {
        prompt,
        effort,
        reasoning_mode,
        speed,
        model,
        configured,
    })
}

/// The login link of the web interface. It embeds the credential, so it is only opened or
/// copied on the user's request and never displayed or logged.
fn web_link(
    status: &watch::Receiver<WebStatus>,
    enabled: bool,
) -> std::result::Result<String, String> {
    if !enabled {
        return Err("The web interface is disabled; set web.enabled = true.".to_owned());
    }
    match &*status.borrow() {
        WebStatus::Ready { url, .. } => Ok(url.clone()),
        WebStatus::Starting => Err("The web interface is still starting.".to_owned()),
        WebStatus::Unavailable { reason } => {
            Err(format!("The web interface is unavailable: {reason}"))
        }
    }
}

/// A sign-in link that a phone can use.
///
/// With `web.tailscale`, the server is published to the tailnet now, and Tailscale is checked again
/// on every call, so a client that was switched on after an earlier refusal is picked up.
async fn phone_link(
    link: String,
    status: &watch::Receiver<WebStatus>,
) -> std::result::Result<String, String> {
    let tailnet = match &*status.borrow() {
        WebStatus::Ready { tailnet, .. } => tailnet.clone(),
        _ => None,
    };
    let Some(tailnet) = tailnet else {
        return reachable_link(link);
    };
    let origin = tailnet
        .origin()
        .await
        .map_err(|error| format!("Cannot share over Tailscale: {error}"))?;
    let (_, credential) = link
        .split_once('#')
        .ok_or_else(|| "The web link has no sign-in credential.".to_owned())?;
    Ok(format!("{origin}/#{credential}"))
}

/// Refuses a link whose address is this computer's own (the default when neither `web.tailscale`
/// nor `web.public_url` is set) rather than encoding it into a code that cannot work.
fn reachable_link(link: String) -> std::result::Result<String, String> {
    use url::Host;
    let reachable = url::Url::parse(&link)
        .ok()
        .and_then(|url| {
            url.host().map(|host| match host {
                Host::Domain(domain) => domain != "localhost" && !domain.ends_with(".localhost"),
                Host::Ipv4(address) => !address.is_loopback() && !address.is_unspecified(),
                Host::Ipv6(address) => !address.is_loopback() && !address.is_unspecified(),
            })
        })
        .unwrap_or(false);
    if reachable {
        Ok(link)
    } else {
        Err("The web link points at this computer, so a phone cannot use it. Set web.tailscale = true, or web.public_url to your tunnel's address.".to_owned())
    }
}

/// The settings a fresh session starts with: the configured defaults for `model`.
fn fresh_settings(config: &Config, model: Model) -> PaneSettings {
    let agent = config.agent();
    PaneSettings::new(
        agent.thinking(),
        supported_reasoning_mode(model, agent.reasoning_mode()),
        agent.speed(),
        model,
    )
}

async fn configure_fresh(config: Config, pane: PaneId, settings: PaneSettings) -> OpenTask {
    let configured = tokio::task::spawn_blocking(move || {
        ConfiguredAgent::from_config_with_session(
            &config,
            settings.effort,
            settings.reasoning_mode,
            settings.model,
            None,
            None,
        )
    })
    .await
    .map_err(|error| RuntimeError::SessionTask(error).into())
    .and_then(|configured| configured);
    (
        pane,
        configured.map(|configured| OpenedSession::Fresh {
            configured: Box::new(configured),
            settings,
        }),
    )
}

/// Loads a persisted session and configures an agent that continues it.
async fn restore_session(
    config: Config,
    session_id: String,
    effort: ReasoningEffort,
    lock: SessionLock,
) -> Result<RestoredSession> {
    let config_path = config.path().to_path_buf();
    let checkpoint_session_id = session_id.clone();
    let checkpoint = tokio::task::spawn_blocking(move || {
        session::load_checkpoint(&config_path, &checkpoint_session_id)
    });
    let transcript =
        session::load_transcript_async(config.path().to_path_buf(), session_id.clone());
    let (snapshot, records) = tokio::join!(checkpoint, transcript);
    let snapshot = snapshot.map_err(RuntimeError::SessionTask)??;
    let records = records?;
    let config = config.with_workspace(session::workspace(&records)?);
    tokio::task::spawn_blocking(move || -> Result<_> {
        let reasoning_mode = session::reasoning_mode(&records);
        let model = session::model(&records)?;
        let next_sequence = session::next_sequence(&records);
        let projection = RootNode::project_session(effort, records.clone());
        let configured = ConfiguredAgent::from_config_with_session(
            &config,
            effort,
            reasoning_mode,
            model,
            Some(&session_id),
            Some(snapshot),
        )?;
        Ok(RestoredSession {
            configured,
            lock,
            records,
            projection,
            reasoning_mode,
            model,
            next_sequence,
        })
    })
    .await
    .map_err(RuntimeError::SessionTask)?
}

/// Asks the worker to fork `parent` into `pane` at the parent's last persisted record.
async fn request_fork(
    panes: &mut HashMap<PaneId, PaneRuntime>,
    commands: &mpsc::UnboundedSender<WorkerCommand>,
    pane: PaneId,
    parent: PaneId,
) -> Result<()> {
    let journal = panes
        .get_mut(&parent)
        .expect("fork parent pane must have a runtime")
        .journal_mut()?;
    journal.flush().await?;
    let parent_sequence = journal.last_sequence();
    commands
        .send(WorkerCommand::OpenFork {
            pane,
            parent,
            parent_sequence,
        })
        .map_err(|_| RuntimeError::AgentWorkerStopped)?;
    Ok(())
}

enum OpenStarted {
    /// The session is opening in this pane; the reply follows when it is live.
    Pending(PaneId),
    /// The session was already live here and is now active.
    Activated(String),
}

/// Starts opening a session for the web interface with the preconditions of the Sessions action.
async fn open_session(
    spec: OpenSpec,
    app: &mut AppNode,
    config: &Config,
    panes: &mut HashMap<PaneId, PaneRuntime>,
    commands: &mpsc::UnboundedSender<WorkerCommand>,
    open_tasks: &mut JoinSet<OpenTask>,
) -> std::result::Result<OpenStarted, CommandError> {
    match spec {
        OpenSpec::New { model, workspace } => {
            let config = match workspace {
                Some(path) => config.with_workspace(open_workspace(&path)?),
                None => config.clone(),
            };
            let model = match model {
                Some(model) => crate::app::model::parse(&model).map_err(CommandError::Invalid)?,
                None => app
                    .root(app.active_pane())
                    .map(|root| root.composer().model())
                    .ok_or(CommandError::UnknownSession)?,
            };
            let pane = app.begin_open("Starting new session…")?;
            open_tasks.spawn(configure_fresh(
                config.clone(),
                pane,
                fresh_settings(&config, model),
            ));
            Ok(OpenStarted::Pending(pane))
        }
        OpenSpec::Resume { session } => {
            if let Some(pane) = app.pane_for_session(&session) {
                app.activate(pane);
                return Ok(OpenStarted::Activated(session));
            }
            app.can_open()?;
            let lock =
                SessionLock::acquire(config.path(), &session).map_err(|error| match error {
                    session::SessionError::Locked { .. } => CommandError::SessionLocked,
                    error => CommandError::Failed(error.to_string()),
                })?;
            let pane = app.begin_open("Resuming session…")?;
            let agent = config.agent();
            let (effort, speed) = (agent.thinking(), agent.speed());
            let preferred_reasoning_mode = agent.reasoning_mode();
            let config = config.clone();
            open_tasks.spawn(async move {
                let restored = restore_session(config, session, effort, lock).await;
                let opened = restored.map(|restored| OpenedSession::Restored {
                    settings: PaneSettings::new(
                        effort,
                        restored.reasoning_mode,
                        speed,
                        restored.model,
                    ),
                    restored: Box::new(restored),
                    preferred_reasoning_mode,
                });
                (pane, opened)
            });
            Ok(OpenStarted::Pending(pane))
        }
        OpenSpec::Fork { session } => {
            let parent = app
                .pane_for_session(&session)
                .ok_or(CommandError::UnknownSession)?;
            let pane = app.begin_fork(parent)?;
            request_fork(panes, commands, pane, parent)
                .await
                .map_err(|error| CommandError::Failed(error.to_string()))?;
            Ok(OpenStarted::Pending(pane))
        }
    }
}

fn open_workspace(path: &str) -> std::result::Result<PathBuf, CommandError> {
    let directory = Path::new(path);
    if !directory.is_absolute() {
        return Err(CommandError::Invalid(format!(
            "workspace must be an absolute directory: {path}"
        )));
    }
    directory
        .canonicalize()
        .ok()
        .filter(|directory| directory.is_dir())
        .ok_or_else(|| {
            CommandError::Invalid(format!("workspace is not an existing directory: {path}"))
        })
}

fn is_web_link(destination: &str) -> bool {
    destination.starts_with("https://") || destination.starts_with("http://")
}

fn local_link_path(destination: &str, workspace: &Path) -> PathBuf {
    let destination = destination.strip_prefix("file://").unwrap_or(destination);
    let destination = destination
        .rsplit_once("#L")
        .filter(|(_, line)| line.parse::<u32>().is_ok())
        .map_or(destination, |(path, _)| path);
    let destination = destination
        .rsplit_once(':')
        .filter(|(_, line)| line.parse::<u32>().is_ok())
        .map_or(destination, |(path, _)| path);
    let path = Path::new(destination);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace.join(path)
    }
}

fn send_submission(
    commands: &tokio::sync::mpsc::UnboundedSender<WorkerCommand>,
    pane: PaneId,
    shell_context: &mut Vec<String>,
    submission: PendingSubmission,
) -> Result<()> {
    commands
        .send(WorkerCommand::Submit {
            pane,
            id: submission.id,
            prompt: inject_shell_context(shell_context, submission.prompt),
        })
        .map_err(|_| RuntimeError::AgentWorkerStopped.into())
}

fn inject_shell_context(contexts: &mut Vec<String>, prompt: Submission) -> Submission {
    if contexts.is_empty() {
        return prompt;
    }
    let context = contexts.join("\n\n");
    contexts.clear();
    prompt.prepend_text(context)
}

fn is_image_paste(event: &Event) -> bool {
    matches!(
        event,
        Event::Key(key)
            if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
                && key.code == KeyCode::Char('v')
                && key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER)
    )
}

fn apply_subagent_config(subagents: &Subagents, config: &Config) {
    subagents.set_claude_enabled(config.claude().enabled());
    subagents.set_max_concurrency(config.agent().max_subagents());
    subagents.set_max_thinking(config.agent().thinking().into());
}

fn schedule(update: ComponentUpdate<AppEffect>, scheduler: &mut RenderScheduler) {
    debug_assert!(update.effects.is_empty());
    request_render(update.render, scheduler);
}

fn request_render(request: RenderRequest, scheduler: &mut RenderScheduler) {
    let now = Instant::now();
    match request {
        RenderRequest::None => {}
        RenderRequest::Streaming => scheduler.request_streaming(now),
        RenderRequest::Immediate => scheduler.request_immediate(now),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MemoryCompletion, MemoryGenerations, MemoryOperation, PaneGeneration, PaneSession,
        PaneSettings, PendingSubmission, close_pane_journal, copy_selection_with, is_image_paste,
        local_link_path, merge_recent_prompts, open_pane, run_memory_operation, send_submission,
        subagent_pane, supported_reasoning_mode, validate_interactive,
    };
    use crate::{
        app::{
            config::{Config, ConfigOverrides, ReasoningEffort, ReasoningMode, Speed},
            error::{Error, RuntimeError},
        },
        core::{
            configured_memory_store,
            pane::PaneId,
            session::{self, RecentPrompt},
            subagent_updates::ForwardedSubagentUpdate,
            transcript::{LocalEvent, TurnId},
            worker::WorkerCommand,
        },
        tui::components::RecentPromptDraft,
    };
    use nanocodex::{HarnessModel as Model, Model as CodexModel};
    use std::{cell::Cell, collections::HashMap, fs, path::Path, sync::Arc};
    use tact_memory::{MemoryLimits, MemoryStore, SelectedMemoryStore};
    use tact_subagents::{AgentId, AgentStatus, AgentUpdate};
    use tempfile::tempdir;

    #[tokio::test]
    async fn resume_rejects_the_recorded_workspace_when_it_is_missing() {
        use super::restore_session;
        use crate::core::{
            session::{SessionLock, save_checkpoint},
            storage::SessionStorage,
            transcript::{SessionStarted, TranscriptRecord},
        };
        use nanocodex::agent::session::SessionSnapshot;
        use serde_json::json;

        let directory = tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        fs::write(&config_path, "").unwrap();
        let config = Config::load(ConfigOverrides {
            path: Some(config_path.clone()),
            workspace: Some(directory.path().to_owned()),
            ..ConfigOverrides::default()
        })
        .unwrap();
        let missing = directory.path().join("deleted-checkout");
        let start = SessionStarted {
            session_id: "session".to_owned(),
            parent_session_id: None,
            parent_sequence: None,
            model: Model::Codex(CodexModel::Luna).to_string(),
            effort: ReasoningEffort::Medium,
            reasoning_mode: ReasoningMode::Standard,
            speed: Speed::Standard,
            workspace: missing.clone(),
            application_version: "test".to_owned(),
        };
        let record = Arc::new(
            TranscriptRecord::from_local(1, 1, LocalEvent::SessionStarted(start)).unwrap(),
        );
        SessionStorage::open(&config_path)
            .unwrap()
            .append_records("session", &[record])
            .unwrap();
        let snapshot: SessionSnapshot = serde_json::from_value(json!({
            "version": 1,
            "model": nanocodex::oai::MODEL,
            "lineage_id": "resume",
            "prompt_cache_key": "cache-resume",
            "workspace": "/work",
            "request_prefix": [
                {"type": "additional_tools", "role": "developer", "tools": []},
                {"type": "message", "role": "developer", "content": []}
            ],
            "canonical_context": {
                "type": "message", "role": "user",
                "content": [{"type": "input_text", "text": "canonical"}]
            },
            "history": []
        }))
        .unwrap();
        save_checkpoint(&config_path, "session", &snapshot, "instructions", false).unwrap();
        let lock = SessionLock::acquire(&config_path, "session").unwrap();

        let result =
            restore_session(config, "session".to_owned(), ReasoningEffort::Low, lock).await;

        let Err(error) = result else {
            panic!("missing workspace must prevent resume");
        };
        assert!(matches!(
            error,
            Error::Runtime(RuntimeError::ResolveWorkspace { path, .. }) if path == missing
        ));
    }

    #[tokio::test]
    async fn config_reload_updates_claude_admission_in_existing_registry() {
        use nanocodex::{
            NanocodexError, Thinking, Tools,
            tools::{
                contract::{ToolContext, ToolInput},
                runtime::ToolRuntime,
            },
        };
        use std::sync::atomic::{AtomicUsize, Ordering};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(&path, "[claude]\nenabled = true\n").unwrap();
        let mut config = Config::load(ConfigOverrides {
            path: Some(path.clone()),
            auth_file: Some(directory.path().join("codex-auth")),
            ..ConfigOverrides::default()
        })
        .unwrap();
        let (subagents, _updates) = tact_subagents::Subagents::new(1);
        subagents.set_claude_enabled(true);
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        subagents
            .set_agent_factory(Thinking::Max, Speed::Standard, move |_, _, _| {
                observed.fetch_add(1, Ordering::SeqCst);
                Err(NanocodexError::InvalidRequest(
                    "test factory admitted".to_owned(),
                ))
            })
            .unwrap();
        let tools = subagents
            .downgrade()
            .install_tools(Tools::builder())
            .build()
            .unwrap();
        let runtime = ToolRuntime::new_with_tools(directory.path(), None, None, &tools);
        for (enabled, expected_calls) in [(false, 0), (true, 1), (false, 1)] {
            fs::write(&path, format!("[claude]\nenabled = {enabled}\n")).unwrap();
            config = config.reload().unwrap().into_parts().0;
            super::apply_subagent_config(&subagents, &config);
            let input = serde_json::json!({"role":"test","task":"test","model":"opus-5.5","thinking":"low","output_schema":{"type":"object"}});
            let output = runtime
                .execute_tool(
                    "spawn_agent",
                    ToolInput::Function(serde_json::value::to_raw_value(&input).unwrap()),
                    ToolContext::new(
                        Model::Codex(CodexModel::Sol).as_str(),
                        "root",
                        "spawn",
                        &[],
                        128,
                    ),
                )
                .await
                .unwrap();
            assert!(!output.success);
            assert_eq!(
                calls.load(Ordering::SeqCst),
                expected_calls,
                "Claude enabled={enabled}"
            );
        }
    }

    #[test]
    fn astra_uses_standard_reasoning_without_changing_other_models() {
        assert_eq!(
            supported_reasoning_mode(Model::Codex(CodexModel::Astra), ReasoningMode::Pro),
            ReasoningMode::Standard
        );
        assert_eq!(
            supported_reasoning_mode(Model::Codex(CodexModel::Sol), ReasoningMode::Pro),
            ReasoningMode::Pro
        );
    }

    #[test]
    fn a_phone_cannot_use_a_link_to_this_computer() {
        for local in [
            "http://127.0.0.1:7878/#k=t",
            "http://localhost:7878/#k=t",
            "http://[::1]:7878/#k=t",
            "http://0.0.0.0:7878/#k=t",
        ] {
            assert!(super::reachable_link(local.to_owned()).is_err(), "{local}");
        }
        for reachable in [
            "https://laptop.tail1234.ts.net/#k=t",
            "http://100.64.0.7:7878/#k=t",
        ] {
            assert_eq!(
                super::reachable_link(reachable.to_owned()).as_deref(),
                Ok(reachable)
            );
        }
    }

    #[test]
    fn control_or_super_v_requests_an_image_paste() {
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

        assert!(is_image_paste(&Event::Key(KeyEvent::new(
            KeyCode::Char('v'),
            KeyModifiers::CONTROL,
        ))));
        assert!(is_image_paste(&Event::Key(KeyEvent::new(
            KeyCode::Char('v'),
            KeyModifiers::SUPER,
        ))));
        assert!(!is_image_paste(&Event::Key(KeyEvent::new(
            KeyCode::Char('v'),
            KeyModifiers::NONE,
        ))));
    }

    #[test]
    fn successful_tmux_copy_skips_platform_fallback() {
        let tmux_calls = Cell::new(0);
        let platform_calls = Cell::new(0);

        let result = copy_selection_with(
            true,
            || {
                tmux_calls.set(tmux_calls.get() + 1);
                Ok(())
            },
            || {
                platform_calls.set(platform_calls.get() + 1);
                Err("platform failed".to_owned())
            },
        );

        assert_eq!(result, Ok(()));
        assert_eq!(tmux_calls.get(), 1);
        assert_eq!(platform_calls.get(), 0);
    }

    #[test]
    fn tmux_failure_calls_platform_fallback() {
        let tmux_calls = Cell::new(0);
        let platform_calls = Cell::new(0);

        let result = copy_selection_with(
            true,
            || {
                tmux_calls.set(tmux_calls.get() + 1);
                Err("tmux failed".to_owned())
            },
            || {
                platform_calls.set(platform_calls.get() + 1);
                Ok(())
            },
        );

        assert_eq!(result, Ok(()));
        assert_eq!(tmux_calls.get(), 1);
        assert_eq!(platform_calls.get(), 1);
    }

    #[test]
    fn current_session_prompts_replace_the_persisted_snapshot() {
        let persisted = vec![
            RecentPrompt {
                text: "stale current".to_owned(),
                recorded_at_unix_ms: 20,
                session_id: "current".to_owned(),
                workspace: "/work".into(),
            },
            RecentPrompt {
                text: "other".to_owned(),
                recorded_at_unix_ms: 15,
                session_id: "other".to_owned(),
                workspace: "/other".into(),
            },
        ];
        let current = vec![
            RecentPromptDraft {
                text: "first".to_owned(),
                recorded_at_unix_ms: 10,
            },
            RecentPromptDraft {
                text: "just submitted".to_owned(),
                recorded_at_unix_ms: 20,
            },
        ];

        let prompts = merge_recent_prompts(persisted, current, "current", Path::new("/work"));

        assert_eq!(
            prompts
                .iter()
                .map(|prompt| prompt.text.as_str())
                .collect::<Vec<_>>(),
            ["just submitted", "other", "first"]
        );
    }

    fn workspace_config(directory: &Path) -> Config {
        let path = directory.join("config.toml");
        fs::write(&path, "[auth]\nmode = 'api-key'\n[openai]\napi_key = 'workspace-fixture'\n[agent]\nweb_search = false\nimage_generation = false\n[skills]\nenabled = false\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        Config::load(ConfigOverrides {
            path: Some(path),
            auth_file: Some(directory.join("unused-auth.json")),
            workspace: Some(directory.to_owned()),
            ..ConfigOverrides::default()
        })
        .unwrap()
    }

    #[tokio::test]
    async fn web_open_builds_the_agent_and_pane_in_the_selected_workspace() {
        use crate::{
            core::protocol::OpenSpec,
            tui::components::{AppNode, RootNode},
        };
        let directory = tempdir().unwrap();
        let selected = tempdir().unwrap();
        let selected = selected.path().canonicalize().unwrap();
        let config = workspace_config(directory.path());
        let mut app = AppNode::new(
            config.theme().clone(),
            directory.path().to_owned(),
            RootNode::new(directory.path(), ReasoningEffort::Low),
        );
        let (commands, _) = tokio::sync::mpsc::unbounded_channel();
        let mut tasks = tokio::task::JoinSet::new();
        let opened = super::open_session(
            OpenSpec::New {
                model: Some("sol".to_owned()),
                workspace: Some(selected.to_string_lossy().into_owned()),
            },
            &mut app,
            &config,
            &mut HashMap::new(),
            &commands,
            &mut tasks,
        )
        .await
        .unwrap();
        let super::OpenStarted::Pending(pane) = opened else {
            panic!("expected pending session")
        };
        let (_, opened) = tasks.join_next().await.unwrap().unwrap();
        let super::OpenedSession::Fresh { configured, .. } = opened.unwrap() else {
            panic!("expected fresh session")
        };
        assert_eq!(configured.workspace, selected);
        app.set_pane_workspace(pane, configured.workspace.clone());
        assert_eq!(app.root(pane).unwrap().workspace(), selected);
        assert_eq!(
            super::local_link_path("file.txt", app.root(pane).unwrap().workspace()),
            selected.join("file.txt")
        );
        assert_eq!(config.agent().workspace(), directory.path());
        assert_eq!(
            config.with_workspace(selected).memory_workspace(),
            directory.path()
        );
        configured.agent.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn web_open_rejects_a_workspace_that_is_a_file() {
        use crate::{
            core::protocol::{CommandError, OpenSpec},
            tui::components::{AppNode, RootNode},
        };
        let directory = tempdir().unwrap();
        let file = directory.path().join("file");
        fs::write(&file, "text").unwrap();
        let config = workspace_config(directory.path());
        let mut app = AppNode::new(
            config.theme().clone(),
            directory.path().to_owned(),
            RootNode::new(directory.path(), ReasoningEffort::Low),
        );
        let (commands, _) = tokio::sync::mpsc::unbounded_channel();
        let result = super::open_session(
            OpenSpec::New {
                model: None,
                workspace: Some(file.to_string_lossy().into_owned()),
            },
            &mut app,
            &config,
            &mut HashMap::new(),
            &commands,
            &mut tokio::task::JoinSet::new(),
        )
        .await;
        assert!(
            matches!(result, Err(CommandError::Invalid(message)) if message.contains(file.to_string_lossy().as_ref()))
        );
        assert!(super::open_workspace("relative").is_err());
    }

    #[test]
    fn local_links_resolve_against_the_workspace_and_ignore_line_suffixes() {
        let workspace = Path::new("/work/project");

        assert_eq!(
            local_link_path("src/main.rs:42", workspace),
            workspace.join("src/main.rs")
        );
        assert_eq!(
            local_link_path("file:///tmp/example.rs#L7", workspace),
            Path::new("/tmp/example.rs")
        );
    }

    #[test]
    fn bare_non_tty_invocation_points_to_headless_run() {
        let error = validate_interactive(false, true).unwrap_err();

        assert!(matches!(
            error,
            Error::Runtime(RuntimeError::InteractiveTerminal)
        ));
    }

    #[test]
    fn submission_consumes_pending_shell_context_before_reaching_the_worker() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut context = vec!["<local_shell_result>done</local_shell_result>".to_owned()];

        send_submission(
            &sender,
            PaneId::Main,
            &mut context,
            PendingSubmission {
                id: TurnId::new(3),
                prompt: "explain it".to_owned().into(),
            },
        )
        .unwrap();

        assert!(context.is_empty());
        assert!(matches!(
            receiver.try_recv(),
            Ok(WorkerCommand::Submit { pane: PaneId::Main, id, prompt })
                if id == TurnId::new(3)
                    && prompt.display_text()
                        == "<local_shell_result>done</local_shell_result>\n\nexplain it"
        ));
    }

    #[test]
    fn disabled_memory_does_not_construct_or_open_the_database() {
        let directory = tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        fs::write(&config_path, "").unwrap();
        let config = Config::load(ConfigOverrides {
            path: Some(config_path),
            workspace: Some(directory.path().to_path_buf()),
            ..ConfigOverrides::default()
        })
        .unwrap();
        let memory_path = config.memory_path();

        assert!(
            configured_memory_store(&config, config.agent().workspace())
                .unwrap()
                .is_none()
        );
        assert!(!memory_path.exists());
    }

    #[test]
    fn enabled_memory_constructs_the_global_store_without_eagerly_opening_it() {
        let directory = tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        fs::write(&config_path, "[memory]\nenabled = true\n").unwrap();
        let config = Config::load(ConfigOverrides {
            path: Some(config_path),
            workspace: Some(directory.path().to_path_buf()),
            ..ConfigOverrides::default()
        })
        .unwrap();
        let memory_path = config.memory_path();

        assert!(
            configured_memory_store(&config, config.agent().workspace())
                .unwrap()
                .is_some()
        );
        assert!(!memory_path.exists());
    }

    #[tokio::test]
    async fn memory_list_inspection_does_not_change_use_telemetry() {
        let directory = tempdir().unwrap();
        let store = SelectedMemoryStore::local(
            directory.path().join("memory.sqlite3"),
            MemoryLimits::PRODUCTION,
        );
        store.put("inspect without using", None).await.unwrap();

        let MemoryCompletion::Listed {
            pane: PaneId::Fork(4),
            result: Ok((access, records)),
            ..
        } = run_memory_operation(PaneId::Fork(4), 1, &store, MemoryOperation::List).await
        else {
            panic!("list should complete for the originating pane");
        };

        assert_eq!(access, tact_memory::MemoryAccess::Local);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].scan_count, 0);
        assert_eq!(records[0].last_scanned_at_ms, None);
        assert_eq!(records[0].use_count, 0);
        assert_eq!(records[0].last_used_at_ms, None);
    }

    #[test]
    fn newer_memory_operations_supersede_older_pane_completions() {
        let mut generations = MemoryGenerations::default();

        let superseded = generations.next(PaneId::Main);
        let fork = generations.next(PaneId::Fork(1));
        let latest = generations.next(PaneId::Main);
        assert!(!generations.is_current(PaneId::Main, superseded));
        assert!(generations.is_current(PaneId::Main, latest));
        assert!(generations.is_current(PaneId::Fork(1), fork));

        generations.invalidate_all();
        assert!(!generations.is_current(PaneId::Main, latest));
        assert!(!generations.is_current(PaneId::Fork(1), fork));
        let renewed = generations.next(PaneId::Main);
        assert!(generations.is_current(PaneId::Main, renewed));
    }

    #[tokio::test]
    async fn stale_human_delete_is_reported_to_the_originating_pane() {
        let directory = tempdir().unwrap();
        let store = SelectedMemoryStore::local(
            directory.path().join("memory.sqlite3"),
            MemoryLimits::PRODUCTION,
        );
        let original = store.put("old value", None).await.unwrap();
        store
            .put("new value", Some(original.key.clone()))
            .await
            .unwrap();

        let completion = run_memory_operation(
            PaneId::Fork(9),
            1,
            &store,
            MemoryOperation::Delete(original.key.clone()),
        )
        .await;

        assert!(matches!(
            completion,
            MemoryCompletion::Deleted {
                pane: PaneId::Fork(9),
                key,
                conflict: true,
                result: Err(error),
                ..
            } if key == original.key && error.contains("changed since it was read")
        ));
    }

    #[tokio::test]
    async fn fork_pane_has_an_independent_session_and_persisted_transcript() {
        let directory = tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        fs::write(&config_path, "").unwrap();
        let config = Config::load(ConfigOverrides {
            path: Some(config_path),
            workspace: Some(directory.path().to_path_buf()),
            ..ConfigOverrides::default()
        })
        .unwrap();
        let (sender, mut completions) = tokio::sync::mpsc::unbounded_channel();
        let (subagent_control, _updates) = tact_subagents::Subagents::new(32);
        let main = open_pane(
            PaneGeneration {
                pane: PaneId::Main,
                generation: 0,
            },
            PaneSession::new("main-session", None, None, 1, false),
            &config,
            PaneSettings::new(
                ReasoningEffort::Low,
                ReasoningMode::Standard,
                Speed::Standard,
                Model::Codex(CodexModel::Luna),
            ),
            Arc::from("instructions"),
            subagent_control.clone(),
            &sender,
            super::SessionLock::acquire(config.path(), "main-session").unwrap(),
        )
        .unwrap();
        let fork = open_pane(
            PaneGeneration {
                pane: PaneId::Fork(1),
                generation: 0,
            },
            PaneSession::new("fork-session", Some("main-session"), Some(0), 1, false),
            &config,
            PaneSettings::new(
                ReasoningEffort::Low,
                ReasoningMode::Standard,
                Speed::Standard,
                Model::Codex(CodexModel::Luna),
            ),
            Arc::from("instructions"),
            subagent_control.clone(),
            &sender,
            super::SessionLock::acquire(config.path(), "fork-session").unwrap(),
        )
        .unwrap();
        let mut panes = HashMap::from([(PaneId::Main, main), (PaneId::Fork(1), fork)]);
        let fork_update = ForwardedSubagentUpdate {
            runtime_id: subagent_control.runtime_id(),
            root_session_id: "fork-session".to_owned(),
            update: AgentUpdate::Status {
                id: AgentId::new(1),
                status: AgentStatus::Closed,
            },
        };

        assert_eq!(subagent_pane(&panes, &fork_update), Some(PaneId::Fork(1)));

        let (other_control, _other_updates) = tact_subagents::Subagents::new(32);
        let stale_update = ForwardedSubagentUpdate {
            runtime_id: other_control.runtime_id(),
            root_session_id: "fork-session".to_owned(),
            update: AgentUpdate::Status {
                id: AgentId::new(1),
                status: AgentStatus::Closed,
            },
        };
        assert_eq!(subagent_pane(&panes, &stale_update), None);

        let mut main = panes.remove(&PaneId::Main).unwrap();
        let mut fork = panes.remove(&PaneId::Fork(1)).unwrap();
        let main_path = main.writer_path.clone();
        fork.journal_mut()
            .unwrap()
            .append_local(LocalEvent::UserSubmitted {
                id: TurnId::new(1),
                text: "fork-only prompt".to_owned(),
            })
            .unwrap();

        assert_eq!(main.session_id, "main-session");
        assert_eq!(fork.session_id, "fork-session");
        assert_eq!(main.writer_path, fork.writer_path);

        drop(main.journal.take());
        drop(fork.journal.take());
        for _ in 0..2 {
            completions.recv().await.unwrap().result.unwrap();
        }
        assert!(main_path.exists());
        assert!(main.exit_session_id().is_none());
        assert_eq!(fork.exit_session_id().as_deref(), Some("fork-session"));
        let records = session::load_transcript(config.path(), "fork-session").unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].kind(), "session.started");
        let started = records[0]
            .decode_payload::<crate::core::transcript::SessionStarted>()
            .unwrap();
        assert_eq!(started.parent_session_id.as_deref(), Some("main-session"));
        assert_eq!(started.model, Model::Codex(CodexModel::Luna).to_string());
        assert_eq!(records[1].kind(), "user.submitted");
    }

    #[tokio::test]
    async fn replacing_a_pane_does_not_persist_the_new_session_until_it_has_transcript_items() {
        let directory = tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        fs::write(&config_path, "").unwrap();
        let config = Config::load(ConfigOverrides {
            path: Some(config_path),
            workspace: Some(directory.path().to_path_buf()),
            ..ConfigOverrides::default()
        })
        .unwrap();
        let (sender, mut completions) = tokio::sync::mpsc::unbounded_channel();
        let (subagent_control, _updates) = tact_subagents::Subagents::new(32);
        let mut old = open_pane(
            PaneGeneration {
                pane: PaneId::Main,
                generation: 0,
            },
            PaneSession::new("old-session", None, None, 1, false),
            &config,
            PaneSettings::new(
                ReasoningEffort::Medium,
                ReasoningMode::Standard,
                Speed::Standard,
                Model::Codex(CodexModel::Sol),
            ),
            Arc::from("instructions"),
            subagent_control.clone(),
            &sender,
            super::SessionLock::acquire(config.path(), "old-session").unwrap(),
        )
        .unwrap();
        old.journal_mut()
            .unwrap()
            .append_local(LocalEvent::UserSubmitted {
                id: TurnId::new(1),
                text: "old prompt".to_owned(),
            })
            .unwrap();

        close_pane_journal(&mut old, super::SessionOutcome::Closed, None).unwrap();
        let mut new = open_pane(
            PaneGeneration {
                pane: PaneId::Main,
                generation: 1,
            },
            PaneSession::new("new-session", None, None, 1, false),
            &config,
            PaneSettings::new(
                ReasoningEffort::Medium,
                ReasoningMode::Standard,
                Speed::Standard,
                Model::Codex(CodexModel::Sol),
            ),
            Arc::from("instructions"),
            subagent_control,
            &sender,
            super::SessionLock::acquire(config.path(), "new-session").unwrap(),
        )
        .unwrap();
        drop(new.journal.take());

        for _ in 0..2 {
            completions.recv().await.unwrap().result.unwrap();
        }
        let old_records = session::load_transcript(config.path(), "old-session").unwrap();

        assert_eq!(old_records.last().unwrap().kind(), "session.ended");
        let ended = old_records
            .last()
            .unwrap()
            .decode_payload::<crate::core::transcript::SessionEnded>()
            .unwrap();
        assert_eq!(ended.outcome, super::SessionOutcome::Closed);
        assert!(
            session::load_transcript(config.path(), "new-session")
                .unwrap()
                .is_empty()
        );
        assert!(new.exit_session_id().is_none());
    }
}
