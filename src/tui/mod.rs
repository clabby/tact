//! Interactive terminal runtime.

mod agent_events;
mod clipboard;
mod components;
mod context;
mod editor;
mod format;
mod pane;
mod prompt;
mod scheduler;
pub(crate) mod session;
mod shell;
mod spinner;
mod subagent_updates;
mod terminal;
pub(crate) mod theme;
pub(crate) mod transcript;
mod worker;

use crate::{
    config::Config,
    core::ConfiguredAgent,
    error::{Result, RuntimeError},
    subagents::SubagentControl,
    tui::{
        agent_events::ForwardedAgentEvent,
        components::{AppEffect, AppEvent, AppNode, ComponentUpdate, RenderRequest, RootNode},
        editor::EditorOutcome,
        pane::PaneId,
        prompt::Submission,
        scheduler::{RenderScheduler, STREAM_FRAME_INTERVAL},
        session::SessionSummary,
        shell::ShellExecution,
        subagent_updates::ForwardedSubagentUpdate,
        terminal::TerminalSession,
        transcript::{
            LocalEvent, SessionEnded, SessionOutcome, SessionStarted, ShellId, TranscriptError,
            TranscriptJournal, TurnId,
        },
        worker::{WorkerCommand, WorkerEvent},
    },
};
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use std::{
    collections::HashMap,
    io::{self, IsTerminal},
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};
use tokio::{
    sync::mpsc,
    task::{JoinHandle, JoinSet},
    time::sleep_until,
};
use tokio_util::sync::CancellationToken;

type EditorTask =
    JoinHandle<std::result::Result<EditorCompletion, crate::error::ExternalEditorError>>;
type EffortUpdateTask = JoinHandle<Result<EffortUpdate>>;
type FastModeUpdateTask = JoinHandle<Result<FastModeUpdate>>;
type NewSessionTask = JoinHandle<(
    PaneId,
    crate::config::ReasoningEffort,
    bool,
    Result<ConfiguredAgent>,
)>;
type SessionListTask = JoinHandle<(PaneId, Result<Vec<SessionSummary>>)>;
type ResumeSessionTask = JoinHandle<(
    PaneId,
    crate::config::ReasoningEffort,
    bool,
    Result<RestoredSession>,
)>;
type UpdateCheckTask =
    JoinHandle<std::result::Result<Option<semver::Version>, crate::update::UpdateError>>;

fn update_checks_enabled() -> bool {
    crate::update::is_official_release_build()
}

fn spawn_update_check(config_path: &Path) -> Option<UpdateCheckTask> {
    if !update_checks_enabled() {
        return None;
    }
    let config_path = config_path.to_path_buf();
    Some(tokio::spawn(async move {
        crate::update::check_for_update(&config_path).await
    }))
}

struct RestoredSession {
    configured: ConfiguredAgent,
    records: Vec<std::sync::Arc<transcript::TranscriptRecord>>,
}

enum EditorTarget {
    Draft {
        pane: PaneId,
        text: String,
    },
    Queue {
        pane: PaneId,
        index: usize,
        text: String,
    },
    Config(PathBuf),
    File(PathBuf),
}

enum EditorCompletion {
    Draft {
        pane: PaneId,
        outcome: EditorOutcome,
    },
    Queue {
        pane: PaneId,
        index: usize,
        original: String,
        outcome: EditorOutcome,
    },
    Config,
    File,
}

struct EffortUpdate {
    pane: PaneId,
    to: crate::config::ReasoningEffort,
}

struct FastModeUpdate {
    pane: PaneId,
    enabled: bool,
}

struct PendingSubmission {
    id: TurnId,
    prompt: Submission,
}

#[derive(Clone, Copy)]
struct PaneGeneration {
    pane: PaneId,
    generation: u64,
}

struct PaneSession<'a> {
    id: &'a str,
    parent_id: Option<&'a str>,
    previously_persisted: bool,
}

#[derive(Clone, Copy)]
struct PaneSettings {
    effort: crate::config::ReasoningEffort,
    fast_mode: bool,
}

impl PaneSettings {
    const fn new(effort: crate::config::ReasoningEffort, fast_mode: bool) -> Self {
        Self { effort, fast_mode }
    }
}

impl<'a> PaneSession<'a> {
    const fn new(id: &'a str, parent_id: Option<&'a str>) -> Self {
        Self {
            id,
            parent_id,
            previously_persisted: false,
        }
    }

    const fn persisted(id: &'a str) -> Self {
        Self {
            id,
            parent_id: None,
            previously_persisted: true,
        }
    }
}

struct PaneRuntime {
    session_id: String,
    instructions: Arc<str>,
    previously_persisted: bool,
    journal: Option<TranscriptJournal>,
    writer_path: PathBuf,
    event_streams_open: usize,
    next_turn: u64,
    next_shell: u64,
    pending_shell_context: Vec<String>,
    pending_submission: Option<PendingSubmission>,
    current_effort: crate::config::ReasoningEffort,
    current_fast_mode: bool,
    active_shells: usize,
    generation: u64,
    subagent_control: SubagentControl,
}

struct WriterCompletion {
    pane: PaneId,
    session_id: String,
    generation: u64,
    result: std::result::Result<(), TranscriptError>,
}

impl PaneRuntime {
    fn journal_mut(&mut self) -> Result<&mut TranscriptJournal> {
        self.journal
            .as_mut()
            .ok_or_else(|| TranscriptError::WriterStopped(self.writer_path.clone()).into())
    }

    fn exit_session_id(&self) -> Option<String> {
        (self.previously_persisted || self.writer_path.is_file()).then(|| self.session_id.clone())
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
    resume_session_id: Option<String>,
    shutdown: CancellationToken,
) -> Result<Option<String>> {
    ensure_interactive()?;

    let restored_records;
    let configured = if let Some(session_id) = resume_session_id.as_deref() {
        let snapshot = session::load_checkpoint(config.path(), session_id)?;
        restored_records = session::load_transcript(config.path(), session_id)?;
        ConfiguredAgent::from_config_with_session(
            &config,
            config.agent().thinking(),
            Some(session_id),
            Some(snapshot),
        )?
    } else {
        restored_records = Vec::new();
        ConfiguredAgent::from_config(&config)?
    };
    let initial_effort = config.agent().thinking();
    let initial_fast_mode = config.agent().fast_mode();
    let mut terminal = TerminalSession::enter().map_err(RuntimeError::Terminal)?;
    let ConfiguredAgent {
        agent,
        events,
        instructions,
        subagent_updates,
        subagent_control,
    } = configured;
    let main_session_id = events.request_id().to_owned();
    let (writer_sender, mut writer_updates) = mpsc::unbounded_channel();
    let mut panes = HashMap::new();
    panes.insert(
        PaneId::Main,
        open_pane(
            PaneGeneration {
                pane: PaneId::Main,
                generation: 0,
            },
            if resume_session_id.is_some() {
                PaneSession::persisted(&main_session_id)
            } else {
                PaneSession::new(&main_session_id, None)
            },
            &config,
            PaneSettings::new(initial_effort, initial_fast_mode),
            instructions,
            subagent_control.clone(),
            &writer_sender,
        )?,
    );
    let (commands, mut worker_updates) = worker::spawn(agent, shutdown.clone());
    let workspace = config.agent().workspace().to_path_buf();
    let (agent_event_sender, mut agent_events) = mpsc::unbounded_channel();
    agent_events::forward(PaneId::Main, 0, events, agent_event_sender.clone());
    let (subagent_sender, mut subagent_events) = mpsc::unbounded_channel();
    subagent_updates::forward(
        subagent_control.runtime_id(),
        subagent_updates,
        subagent_sender.clone(),
    );
    let mut root = RootNode::new(&workspace, initial_effort);
    root.set_fast_mode(initial_fast_mode);
    if !restored_records.is_empty() {
        root.restore_session(
            &workspace,
            initial_effort,
            initial_fast_mode,
            restored_records,
        );
    }
    let mut theme = config.theme().clone();
    if let Some(scheme) = theme::detect_system_scheme() {
        theme.set_system_scheme(scheme);
    }
    let mut app = AppNode::new(theme, workspace.clone(), root);
    let mut update_check_task = spawn_update_check(config.path());
    let (system_theme_sender, mut system_theme_updates) = mpsc::unbounded_channel();
    theme::watch_system_scheme(system_theme_sender, shutdown.clone());
    let mut input = Some(EventStream::new());
    let mut editor_task = None::<EditorTask>;
    let mut effort_task = None::<EffortUpdateTask>;
    let mut fast_mode_task = None::<FastModeUpdateTask>;
    let mut new_session_task = None::<NewSessionTask>;
    let mut session_list_task = None::<SessionListTask>;
    let mut resume_session_task = None::<ResumeSessionTask>;
    let mut scheduler = RenderScheduler::new(STREAM_FRAME_INTERVAL, Instant::now());
    let mut stopping = false;
    let mut worker_stopped = false;
    let mut worker_error = None::<nanocodex::NanocodexError>;
    let mut writer_error = None::<TranscriptError>;
    let mut writers_open = 1_usize;
    let mut shell_tasks = JoinSet::<(PaneId, ShellExecution)>::new();
    let mut subagent_shutdowns = JoinSet::<()>::new();
    let mut subagents_stopping = false;

    macro_rules! apply_app_update {
        ($update:expr) => {
            apply_update(
                $update,
                EffectContext {
                    app: &mut app,
                    commands: &commands,
                    workspace: &workspace,
                    config: &mut config,
                    shutdown: &shutdown,
                    input: &mut input,
                    editor_task: &mut editor_task,
                    effort_task: &mut effort_task,
                    fast_mode_task: &mut fast_mode_task,
                    new_session_task: &mut new_session_task,
                    session_list_task: &mut session_list_task,
                    resume_session_task: &mut resume_session_task,
                    terminal: &mut terminal,
                    scheduler: &mut scheduler,
                    panes: &mut panes,
                    shell_tasks: &mut shell_tasks,
                    subagent_shutdowns: &mut subagent_shutdowns,
                },
            )?;
        };
    }

    loop {
        if stopping && let Some(task) = update_check_task.take() {
            task.abort();
        }
        if stopping {
            shell_tasks.abort_all();
        }
        if stopping && !subagents_stopping {
            for runtime in panes.values() {
                schedule_subagent_shutdown(runtime, &mut subagent_shutdowns);
            }
            subagents_stopping = true;
        }
        if stopping
            && worker_stopped
            && panes.values().all(|pane| pane.event_streams_open == 0)
            && shell_tasks.is_empty()
            && subagent_shutdowns.is_empty()
        {
            close_journals(&mut panes, worker_error.as_ref())?;
            if writers_open == 0 {
                break;
            }
        }

        if editor_task.is_none() && !stopping && scheduler.is_due(Instant::now()) {
            terminal
                .draw(|frame| app.render(frame))
                .map_err(RuntimeError::Terminal)?;
            scheduler.presented(Instant::now());
        }

        let render_deadline = scheduler.deadline();
        let animation_deadline = app.animation_deadline();
        tokio::select! {
            () = shutdown.cancelled(), if !stopping => {
                stopping = true;
                input = None;
                if let Some(task) = editor_task.take() {
                    task.abort();
                    drop(task.await);
                }
                if let Some(task) = effort_task.take() {
                    task.abort();
                    drop(task.await);
                }
                if let Some(task) = fast_mode_task.take() {
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
            }, if input.is_some() && !stopping => {
                let event = event
                    .transpose()
                    .map_err(RuntimeError::Terminal)?
                    .ok_or_else(|| RuntimeError::Terminal(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "terminal input closed",
                    )))?;
                let update = if is_image_paste(&event)
                    && let Some(data_url) = clipboard::image_data_url()
                {
                    app.update(AppEvent::PasteImage(data_url))
                } else {
                    app.update(AppEvent::Terminal(event))
                };
                apply_app_update!(update);
            }
            Some(scheme) = system_theme_updates.recv(), if !stopping => {
                schedule(app.update(AppEvent::SystemThemeChanged(scheme)), &mut scheduler);
            }
            result = async {
                update_check_task
                    .as_mut()
                    .expect("update-check branch is disabled without a task")
                    .await
            }, if update_check_task.is_some() && !stopping => {
                update_check_task = None;
                if let Ok(Ok(Some(version))) = result {
                    schedule(app.update(AppEvent::UpdateAvailable(version)), &mut scheduler);
                }
            }
            event = agent_events.recv(), if panes.values().any(|pane| pane.event_streams_open > 0) => {
                let Some(event) = event else {
                    for (&pane, runtime) in &mut panes {
                        if runtime.event_streams_open > 0 {
                            runtime.event_streams_open = 0;
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
                        apply_app_update!(app.update(AppEvent::Transcript { pane, record }));
                    }
                    ForwardedAgentEvent::Closed { pane, session_id, generation } => {
                        let mut stream_closed = false;
                        if let Some(runtime) = panes.get_mut(&pane)
                            && runtime.session_id == session_id
                            && runtime.generation == generation
                        {
                            runtime.event_streams_open = runtime.event_streams_open.saturating_sub(1);
                            stream_closed = runtime.event_streams_open == 0;
                        }
                        if stream_closed {
                            schedule(app.update(AppEvent::AgentStreamClosed(pane)), &mut scheduler);
                        }
                    }
                }
            }
            Some(event) = subagent_events.recv(), if !stopping => {
                if let Some(pane) = subagent_pane(&panes, &event) {
                    schedule(
                        app.update(AppEvent::Subagent {
                            pane,
                            update: event.update,
                        }),
                        &mut scheduler,
                    );
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
                    WorkerEvent::TurnAccepted { pane, id } => {
                        let record = panes.get_mut(&pane).expect("worker pane must exist")
                            .journal_mut()?.append_local(LocalEvent::WorkerTurnAccepted { id })?;
                        schedule(app.update(AppEvent::Transcript { pane, record }), &mut scheduler);
                    }
                    WorkerEvent::TurnFinished { pane, id, error, snapshot } => {
                        let Some(runtime) = panes.get_mut(&pane) else {
                            continue;
                        };
                        if let Some(snapshot) = snapshot {
                            session::save_checkpoint(
                                config.path(),
                                &runtime.session_id,
                                &snapshot,
                                &runtime.instructions,
                            )?;
                        }
                        let record = runtime.journal_mut()?.append_local(LocalEvent::WorkerTurnFinished {
                            id,
                            error,
                        })?;
                        schedule(app.update(AppEvent::Transcript { pane, record }), &mut scheduler);
                        apply_app_update!(app.update(AppEvent::WorkerTurnFinished(pane)));
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
                            error,
                        })?;
                        schedule(app.update(AppEvent::Transcript { pane, record }), &mut scheduler);
                        apply_app_update!(app.update(AppEvent::SteerFailed { pane, id: queue_id }));
                    }
                    WorkerEvent::TurnsCancelled { pane, count, error } => {
                        let Some(runtime) = panes.get_mut(&pane) else {
                            continue;
                        };
                        let record = runtime.journal_mut()?.append_local(LocalEvent::WorkerTurnsInterrupted {
                            count,
                            error,
                        })?;
                        schedule(app.update(AppEvent::Transcript { pane, record }), &mut scheduler);
                        schedule(app.update(AppEvent::TurnsCancelled(pane)), &mut scheduler);
                    }
                    WorkerEvent::ForkOpened { pane, events } => {
                        let session_id = events.request_id().to_owned();
                        let parent_session_id = panes
                            .get(&PaneId::Main)
                            .map(|runtime| runtime.session_id.clone());
                        let effort = app
                            .root(pane)
                            .map(|root| root.composer().effort())
                            .unwrap_or_else(|| config.agent().thinking());
                        let fast_mode = panes
                            .get(&PaneId::Main)
                            .expect("main pane must exist")
                            .current_fast_mode;
                        let subagent_control = panes
                            .get(&PaneId::Main)
                            .expect("main pane must exist")
                            .subagent_control
                            .clone();
                        let instructions = Arc::clone(
                            &panes
                                .get(&PaneId::Main)
                                .expect("main pane must exist")
                                .instructions,
                        );
                        panes.insert(
                            pane,
                            open_pane(
                                PaneGeneration {
                                    pane,
                                    generation: 0,
                                },
                                PaneSession::new(&session_id, parent_session_id.as_deref()),
                                &config,
                                PaneSettings::new(effort, fast_mode),
                                instructions,
                                subagent_control.clone(),
                                &writer_sender,
                            )?,
                        );
                        writers_open = writers_open.saturating_add(1);
                        agent_events::forward(pane, 0, events, agent_event_sender.clone());
                        apply_app_update!(app.update(AppEvent::ForkReady(pane)));
                    }
                    WorkerEvent::ForkFailed { pane, error } => {
                        apply_app_update!(app.update(AppEvent::ForkFailed { pane, error }));
                    }
                    WorkerEvent::ThinkingUpdated { pane, effort, result } => {
                        result?;
                        let runtime = panes.get_mut(&pane).expect("effort pane must exist");
                        let previous_effort = runtime.current_effort;
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
                        runtime.current_effort = effort;
                        if pane == PaneId::Main {
                            config.set_thinking(effort);
                        }
                        input = Some(EventStream::new());
                        scheduler.request_immediate(Instant::now());
                    }
                    WorkerEvent::FastModeUpdated { pane, enabled, result } => {
                        result?;
                        let runtime = panes.get_mut(&pane).expect("fast-mode pane must exist");
                        let previous = runtime.current_fast_mode;
                        let journal = runtime.journal_mut()?;
                        if journal.is_empty() {
                            journal.set_initial_fast_mode(enabled);
                        } else {
                            let record = journal.append_local(LocalEvent::FastModeChanged {
                                from: previous,
                                to: enabled,
                            })?;
                            schedule(app.update(AppEvent::Transcript { pane, record }), &mut scheduler);
                        }
                        runtime.current_fast_mode = enabled;
                        if pane == PaneId::Main {
                            config.set_fast_mode(enabled);
                        }
                        input = Some(EventStream::new());
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
            result = subagent_shutdowns.join_next(), if !subagent_shutdowns.is_empty() => {
                drop(result);
            }
            result = async {
                editor_task
                    .as_mut()
                    .expect("editor branch is disabled without an editor task")
                    .await
            }, if editor_task.is_some() && !stopping => {
                editor_task = None;
                terminal.resume().map_err(RuntimeError::Terminal)?;
                input = Some(EventStream::new());
                match result.map_err(RuntimeError::ExternalEditorTask)?? {
                    EditorCompletion::Draft { pane, outcome: EditorOutcome::Updated(draft) } => {
                        schedule(app.update(AppEvent::EditorDraft { pane, draft }), &mut scheduler);
                    }
                    EditorCompletion::Queue {
                        pane,
                        index,
                        original,
                        outcome,
                    } => {
                        let text = match outcome {
                            EditorOutcome::Updated(text) => text,
                            EditorOutcome::Unchanged => original,
                        };
                        schedule(
                            app.update(AppEvent::QueueEditorFinished { pane, index, text }),
                            &mut scheduler,
                        );
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
            }, if effort_task.is_some() && !stopping => {
                effort_task = None;
                let update = result.map_err(RuntimeError::EffortUpdateTask)??;
                commands
                    .send(WorkerCommand::SetThinking {
                        pane: update.pane,
                        effort: update.to,
                    })
                    .map_err(|_| RuntimeError::AgentWorkerStopped)?;
            }
            result = async {
                fast_mode_task
                    .as_mut()
                    .expect("fast-mode branch is disabled without a task")
                    .await
            }, if fast_mode_task.is_some() && !stopping => {
                fast_mode_task = None;
                let update = result.map_err(RuntimeError::FastModeUpdateTask)??;
                commands
                    .send(WorkerCommand::SetFastMode {
                        pane: update.pane,
                        enabled: update.enabled,
                    })
                    .map_err(|_| RuntimeError::AgentWorkerStopped)?;
            }
            result = async {
                new_session_task
                    .as_mut()
                    .expect("new-session branch is disabled without a task")
                    .await
            }, if new_session_task.is_some() && !stopping => {
                new_session_task = None;
                input = Some(EventStream::new());
                let (pane, effort, fast_mode, configured) =
                    result.map_err(RuntimeError::NewSessionTask)?;
                match configured {
                    Ok(configured) => {
                        let ConfiguredAgent {
                            agent,
                            events,
                            instructions,
                            subagent_updates,
                            subagent_control,
                        } = configured;
                        let session_id = events.request_id().to_owned();
                        let generation = panes
                            .get(&pane)
                            .expect("new-session pane must exist")
                            .generation
                            .saturating_add(1);
                        schedule_subagent_shutdown(
                            panes.get(&pane).expect("new-session pane must exist"),
                            &mut subagent_shutdowns,
                        );
                        close_pane_journal(
                            panes.get_mut(&pane).expect("new-session pane must exist"),
                            SessionOutcome::Closed,
                            None,
                        )?;
                        panes.insert(
                            pane,
                            open_pane(
                                PaneGeneration { pane, generation },
                                PaneSession::new(&session_id, None),
                                &config,
                                PaneSettings::new(effort, fast_mode),
                                instructions,
                                subagent_control.clone(),
                                &writer_sender,
                            )?,
                        );
                        writers_open = writers_open.saturating_add(1);
                        agent_events::forward(
                            pane,
                            generation,
                            events,
                            agent_event_sender.clone(),
                        );
                        subagent_updates::forward(
                            subagent_control.runtime_id(),
                            subagent_updates,
                            subagent_sender.clone(),
                        );
                        commands
                            .send(WorkerCommand::ReplaceAgent { pane, agent })
                            .map_err(|_| RuntimeError::AgentWorkerStopped)?;
                        schedule(
                            app.update(AppEvent::NewSessionReady {
                                pane,
                                effort,
                                fast_mode,
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
            }, if session_list_task.is_some() && !stopping => {
                session_list_task = None;
                input = Some(EventStream::new());
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
                resume_session_task
                    .as_mut()
                    .expect("resume-session branch is disabled without a task")
                    .await
            }, if resume_session_task.is_some() && !stopping => {
                resume_session_task = None;
                input = Some(EventStream::new());
                let (pane, effort, fast_mode, restored) =
                    result.map_err(RuntimeError::SessionTask)?;
                match restored {
                    Ok(RestoredSession {
                        configured,
                        records,
                    }) => {
                        let ConfiguredAgent {
                            agent,
                            events,
                            instructions,
                            subagent_updates,
                            subagent_control,
                        } = configured;
                        let session_id = events.request_id().to_owned();
                        let generation = panes
                            .get(&pane)
                            .expect("resumed pane must exist")
                            .generation
                            .saturating_add(1);
                        schedule_subagent_shutdown(
                            panes.get(&pane).expect("resumed pane must exist"),
                            &mut subagent_shutdowns,
                        );
                        close_pane_journal(
                            panes.get_mut(&pane).expect("resumed pane must exist"),
                            SessionOutcome::Closed,
                            None,
                        )?;
                        panes.insert(
                            pane,
                            open_pane(
                                PaneGeneration { pane, generation },
                                PaneSession::persisted(&session_id),
                                &config,
                                PaneSettings::new(effort, fast_mode),
                                instructions,
                                subagent_control.clone(),
                                &writer_sender,
                            )?,
                        );
                        writers_open = writers_open.saturating_add(1);
                        agent_events::forward(
                            pane,
                            generation,
                            events,
                            agent_event_sender.clone(),
                        );
                        subagent_updates::forward(
                            subagent_control.runtime_id(),
                            subagent_updates,
                            subagent_sender.clone(),
                        );
                        commands
                            .send(WorkerCommand::ReplaceAgent { pane, agent })
                            .map_err(|_| RuntimeError::AgentWorkerStopped)?;
                        schedule(
                            app.update(AppEvent::SessionRestored {
                                pane,
                                records,
                                effort,
                                fast_mode,
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
                    stopping = true;
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
            }, if animation_deadline.is_some() && editor_task.is_none() && !stopping => {
                schedule(app.update(AppEvent::AnimationFrame(Instant::now())), &mut scheduler);
            }
            () = async {
                sleep_until(render_deadline.expect("deadline branch is disabled without a deadline").into()).await;
            }, if render_deadline.is_some() && editor_task.is_none() && !stopping => {}
        }
    }

    let session_id = panes
        .get(&PaneId::Main)
        .and_then(PaneRuntime::exit_session_id);
    drop(terminal);
    if let Some(error) = writer_error {
        return Err(error.into());
    }
    worker_error.map_or(Ok(session_id), |error| Err(error.into()))
}

pub(crate) fn ensure_interactive() -> Result<()> {
    validate_interactive(io::stdin().is_terminal(), io::stdout().is_terminal())
}

fn validate_interactive(stdin: bool, stdout: bool) -> Result<()> {
    if stdin && stdout {
        return Ok(());
    }
    Err(RuntimeError::InteractiveTerminal.into())
}

fn open_pane(
    identity: PaneGeneration,
    session: PaneSession<'_>,
    config: &Config,
    settings: PaneSettings,
    instructions: Arc<str>,
    subagent_control: SubagentControl,
    writer_updates: &mpsc::UnboundedSender<WriterCompletion>,
) -> Result<PaneRuntime> {
    let PaneGeneration { pane, generation } = identity;
    let PaneSettings { effort, fast_mode } = settings;
    let PaneSession {
        id: session_id,
        parent_id: parent_session_id,
        previously_persisted,
    } = session;
    if pane == PaneId::Main && generation == 0 {
        session::remove_obsolete_checkpoints(config.path())?;
    }
    let (mut journal, writer) = TranscriptJournal::open(config.path(), session_id)?;
    let writer_path = journal.path().to_path_buf();
    journal.defer_start(SessionStarted {
        session_id: session_id.to_owned(),
        parent_session_id: parent_session_id.map(str::to_owned),
        model: nanocodex::MODEL.to_owned(),
        effort,
        fast_mode,
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
        previously_persisted,
        journal: Some(journal),
        writer_path,
        event_streams_open: 1,
        next_turn: 1,
        next_shell: 1,
        pending_shell_context: Vec::new(),
        pending_submission: None,
        current_effort: effort,
        current_fast_mode: fast_mode,
        active_shells: 0,
        generation,
        subagent_control,
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

struct EffectContext<'a> {
    app: &'a mut AppNode,
    commands: &'a tokio::sync::mpsc::UnboundedSender<WorkerCommand>,
    workspace: &'a Path,
    config: &'a mut Config,
    shutdown: &'a CancellationToken,
    input: &'a mut Option<EventStream>,
    editor_task: &'a mut Option<EditorTask>,
    effort_task: &'a mut Option<EffortUpdateTask>,
    fast_mode_task: &'a mut Option<FastModeUpdateTask>,
    new_session_task: &'a mut Option<NewSessionTask>,
    session_list_task: &'a mut Option<SessionListTask>,
    resume_session_task: &'a mut Option<ResumeSessionTask>,
    terminal: &'a mut TerminalSession,
    scheduler: &'a mut RenderScheduler,
    panes: &'a mut HashMap<PaneId, PaneRuntime>,
    shell_tasks: &'a mut JoinSet<(PaneId, ShellExecution)>,
    subagent_shutdowns: &'a mut JoinSet<()>,
}

fn apply_update(update: ComponentUpdate<AppEffect>, mut context: EffectContext<'_>) -> Result<()> {
    for effect in update.effects {
        match effect {
            AppEffect::OpenFork(pane) => context
                .commands
                .send(WorkerCommand::OpenFork(pane))
                .map_err(|_| RuntimeError::AgentWorkerStopped)?,
            AppEffect::ClosePane(pane) => {
                if let Some(runtime) = context.panes.get(&pane) {
                    schedule_subagent_shutdown(runtime, context.subagent_shutdowns);
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

fn apply_pane_effect(
    pane: PaneId,
    effect: components::RootEffect,
    context: &mut EffectContext<'_>,
) -> Result<()> {
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
                    workspace: context.workspace.to_path_buf(),
                })?;
            schedule(
                context.app.update(AppEvent::Transcript { pane, record }),
                context.scheduler,
            );
            let workspace = context.workspace.to_path_buf();
            context
                .shell_tasks
                .spawn(async move { (pane, shell::execute(id, command, workspace).await) });
        }
        components::RootEffect::OpenLink(destination) if is_web_link(&destination) => {
            if let Err(error) = crate::browser::open(&destination) {
                schedule(
                    context.app.update(AppEvent::NotifyError {
                        pane,
                        error: format!("Could not open link: {error}"),
                    }),
                    context.scheduler,
                );
            }
        }
        editor_effect @ (components::RootEffect::OpenDraftEditor
        | components::RootEffect::OpenQueueEditor { .. }
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
                components::RootEffect::OpenQueueEditor { index, text } => {
                    EditorTarget::Queue { pane, index, text }
                }
                components::RootEffect::OpenConfigEditor => {
                    EditorTarget::Config(context.config.path().to_path_buf())
                }
                components::RootEffect::OpenLink(destination) => {
                    EditorTarget::File(local_link_path(&destination, context.workspace))
                }
                _ => unreachable!("editor effect pattern is exhaustive"),
            };
            let workspace = context.workspace.to_path_buf();
            *context.editor_task = Some(tokio::spawn(async move {
                match target {
                    EditorTarget::Draft { pane, text } => {
                        let outcome = editor::edit(&text, &workspace).await?;
                        Ok(EditorCompletion::Draft { pane, outcome })
                    }
                    EditorTarget::Queue { pane, index, text } => {
                        let outcome = editor::edit(&text, &workspace).await?;
                        Ok(EditorCompletion::Queue {
                            pane,
                            index,
                            original: text,
                            outcome,
                        })
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
        components::RootEffect::SetEffort(effort) => {
            *context.input = None;
            let config = (pane == PaneId::Main).then(|| context.config.clone());
            *context.effort_task = Some(tokio::task::spawn_blocking(move || {
                if let Some(config) = config {
                    config.persist_thinking(effort)?;
                }
                Ok(EffortUpdate { pane, to: effort })
            }));
        }
        components::RootEffect::SetFastMode(enabled) => {
            *context.input = None;
            let config = (pane == PaneId::Main).then(|| context.config.clone());
            *context.fast_mode_task = Some(tokio::task::spawn_blocking(move || {
                if let Some(config) = config {
                    config.persist_fast_mode(enabled)?;
                }
                Ok(FastModeUpdate { pane, enabled })
            }));
        }
        components::RootEffect::ReloadConfig => match context.config.reload() {
            Ok(reload) => {
                let (config, workspace_changed) = reload.into_parts();
                let theme = config.theme().clone();
                *context.config = config;
                let message = if workspace_changed {
                    "Reloaded config · theme applied · agent/auth settings apply to new sessions · workspace requires restart"
                } else {
                    "Reloaded config · theme applied · agent/auth settings apply to new sessions"
                };
                schedule(
                    context.app.update(AppEvent::ConfigReloaded {
                        pane,
                        theme,
                        message: message.to_owned(),
                    }),
                    context.scheduler,
                );
            }
            Err(error) => schedule(
                context.app.update(AppEvent::ConfigReloadFailed {
                    pane,
                    error: format!("Could not reload config: {error}"),
                }),
                context.scheduler,
            ),
        },
        components::RootEffect::NewSession => {
            *context.input = None;
            let effort = context.config.agent().thinking();
            let config = context.config.clone();
            *context.new_session_task = Some(tokio::task::spawn_blocking(move || {
                let fast_mode = config.agent().fast_mode();
                let configured =
                    ConfiguredAgent::from_config_with_session(&config, effort, None, None);
                (pane, effort, fast_mode, configured)
            }));
        }
        components::RootEffect::LoadSessions => {
            *context.input = None;
            let config_path = context.config.path().to_path_buf();
            let workspace = context.workspace.to_path_buf();
            let active_session_id = context
                .panes
                .get(&pane)
                .expect("session-list pane must exist")
                .session_id
                .clone();
            *context.session_list_task = Some(tokio::task::spawn_blocking(move || {
                let sessions = session::list(&config_path, &workspace).map(|mut sessions| {
                    sessions.retain(|session| session.session_id != active_session_id);
                    sessions
                });
                (pane, sessions.map_err(Into::into))
            }));
        }
        components::RootEffect::ResumeSession(session_id) => {
            *context.input = None;
            let effort = context.config.agent().thinking();
            let fast_mode = context.config.agent().fast_mode();
            let config = context.config.clone();
            *context.resume_session_task = Some(tokio::task::spawn_blocking(move || {
                let restored = (|| {
                    let snapshot = session::load_checkpoint(config.path(), &session_id)?;
                    let records = session::load_transcript(config.path(), &session_id)?;
                    let configured = ConfiguredAgent::from_config_with_session(
                        &config,
                        effort,
                        Some(&session_id),
                        Some(snapshot),
                    )?;
                    Ok(RestoredSession {
                        configured,
                        records,
                    })
                })();
                (pane, effort, fast_mode, restored)
            }));
        }
        components::RootEffect::Copy(text) => match clipboard::copy_text(&text) {
            Ok(()) => schedule(
                context.app.update(AppEvent::NotifySuccess {
                    pane,
                    message: "Copied selection to clipboard.".to_owned(),
                }),
                context.scheduler,
            ),
            Err(native_error) => {
                match context.terminal.copy_to_clipboard(&text) {
                    Ok(()) => schedule(
                        context.app.update(AppEvent::NotifySuccess {
                            pane,
                            message: "Sent selection to the terminal clipboard.".to_owned(),
                        }),
                        context.scheduler,
                    ),
                    Err(terminal_error) => schedule(
                        context.app.update(AppEvent::NotifyError {
                            pane,
                            error: format!(
                                "Could not copy selection: {native_error}; terminal fallback failed: {terminal_error}"
                            ),
                        }),
                        context.scheduler,
                    ),
                }
            }
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
                .append_local(LocalEvent::UserSteered { text })?;
            schedule(
                context.app.update(AppEvent::Transcript { pane, record }),
                context.scheduler,
            );
        }
        components::RootEffect::CancelTurns => {
            let runtime = context
                .panes
                .get(&pane)
                .expect("cancelled pane must exist");
            let subagents = runtime.subagent_control.clone();
            let root_session_id = runtime.session_id.clone();
            tokio::spawn(async move { subagents.cancel_all(&root_session_id).await });
            context
                .commands
                .send(WorkerCommand::CancelAll(pane))
                .map_err(|_| RuntimeError::AgentWorkerStopped)?;
        }
        components::RootEffect::Fork
        | components::RootEffect::SetTheme(_)
        | components::RootEffect::Shutdown => {
            unreachable!("application effects are handled before pane dispatch")
        }
    }
    Ok(())
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
        PaneGeneration, PaneSession, PaneSettings, PendingSubmission, close_pane_journal,
        is_image_paste, local_link_path, open_pane, send_submission, subagent_pane,
        update_checks_enabled, validate_interactive,
    };
    use crate::{
        config::{Config, ConfigOverrides, ReasoningEffort},
        error::{Error, RuntimeError},
        subagents::{AgentId, AgentStatus, AgentUpdate},
        tui::{
            pane::PaneId,
            subagent_updates::ForwardedSubagentUpdate,
            transcript::{LocalEvent, TurnId, load},
            worker::WorkerCommand,
        },
    };
    use std::{collections::HashMap, fs, path::Path, sync::Arc};
    use tempfile::tempdir;

    #[test]
    fn development_builds_do_not_enable_update_checks() {
        assert!(!update_checks_enabled());
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
        assert!(error.to_string().contains("tact run <PROMPT>"));
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

    #[tokio::test]
    async fn opening_a_session_removes_obsolete_checkpoints() {
        let directory = tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        fs::write(&config_path, "").unwrap();
        let config = Config::load(ConfigOverrides {
            path: Some(config_path),
            workspace: Some(directory.path().to_path_buf()),
            ..ConfigOverrides::default()
        })
        .unwrap();
        let obsolete = directory.path().join("checkpoints/6161.json.zst");
        fs::create_dir_all(obsolete.parent().unwrap()).unwrap();
        fs::write(&obsolete, b"obsolete checkpoint").unwrap();
        let (sender, mut completions) = tokio::sync::mpsc::unbounded_channel();
        let (_subagents, subagent_control, _updates) = crate::subagents::channel();

        let pane = open_pane(
            PaneGeneration {
                pane: PaneId::Main,
                generation: 0,
            },
            PaneSession::new("main-session", None),
            &config,
            PaneSettings::new(ReasoningEffort::Low, false),
            Arc::from("instructions"),
            subagent_control,
            &sender,
        )
        .unwrap();

        assert!(!obsolete.exists());
        drop(pane);
        completions.recv().await.unwrap().result.unwrap();
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
        let (_subagents, subagent_control, _updates) = crate::subagents::channel();
        let main = open_pane(
            PaneGeneration {
                pane: PaneId::Main,
                generation: 0,
            },
            PaneSession::new("main-session", None),
            &config,
            PaneSettings::new(ReasoningEffort::Low, false),
            Arc::from("instructions"),
            subagent_control.clone(),
            &sender,
        )
        .unwrap();
        let fork = open_pane(
            PaneGeneration {
                pane: PaneId::Fork(1),
                generation: 0,
            },
            PaneSession::new("fork-session", Some("main-session")),
            &config,
            PaneSettings::new(ReasoningEffort::Low, false),
            Arc::from("instructions"),
            subagent_control.clone(),
            &sender,
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

        let (_other_registry, other_control, _other_updates) = crate::subagents::channel();
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
        let fork_path = fork.writer_path.clone();
        fork.journal_mut()
            .unwrap()
            .append_local(LocalEvent::UserSubmitted {
                id: TurnId::new(1),
                text: "fork-only prompt".to_owned(),
            })
            .unwrap();

        assert_eq!(main.session_id, "main-session");
        assert_eq!(fork.session_id, "fork-session");
        assert_ne!(main.writer_path, fork.writer_path);

        drop(main.journal.take());
        drop(fork.journal.take());
        for _ in 0..2 {
            completions.recv().await.unwrap().result.unwrap();
        }
        assert!(!main_path.exists());
        assert!(main.exit_session_id().is_none());
        assert_eq!(fork.exit_session_id().as_deref(), Some("fork-session"));
        let records = load(&fork_path).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].kind(), "session.started");
        let started = records[0]
            .decode_payload::<crate::tui::transcript::SessionStarted>()
            .unwrap();
        assert_eq!(started.parent_session_id.as_deref(), Some("main-session"));
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
        let (_subagents, subagent_control, _updates) = crate::subagents::channel();
        let mut old = open_pane(
            PaneGeneration {
                pane: PaneId::Main,
                generation: 0,
            },
            PaneSession::new("old-session", None),
            &config,
            PaneSettings::new(ReasoningEffort::Medium, false),
            Arc::from("instructions"),
            subagent_control.clone(),
            &sender,
        )
        .unwrap();
        let old_path = old.writer_path.clone();
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
            PaneSession::new("new-session", None),
            &config,
            PaneSettings::new(ReasoningEffort::Medium, false),
            Arc::from("instructions"),
            subagent_control,
            &sender,
        )
        .unwrap();
        let new_path = new.writer_path.clone();
        drop(new.journal.take());

        for _ in 0..2 {
            completions.recv().await.unwrap().result.unwrap();
        }
        let old_records = load(&old_path).unwrap();

        assert_eq!(old_records.last().unwrap().kind(), "session.ended");
        let ended = old_records
            .last()
            .unwrap()
            .decode_payload::<crate::tui::transcript::SessionEnded>()
            .unwrap();
        assert_eq!(ended.outcome, super::SessionOutcome::Closed);
        assert!(!new_path.exists());
        assert!(new.exit_session_id().is_none());
    }
}
