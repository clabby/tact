//! The event loop's owned state and its select loop.
//!
//! [`EventLoop`] owns everything the loop mutates: the application tree, the terminal, the pane
//! runtimes, the link to the turn worker, and every task whose completion the loop awaits.
//! [`Inbox`] holds the receiving ends of the channels it selects over. Each select arm hands its
//! event to one handler method, which runs to completion before the next event is taken, so
//! handlers mutate state without locks. Handlers live in the submodule that owns their state:
//!
//! - [`panes`] and [`pane_events`]: pane runtimes, agent and subagent events, shells, and journal
//!   writers.
//! - [`worker_events`]: worker events and the link to the worker.
//! - [`effects`]: effects requested by the application tree.
//! - [`sessions`] and [`opens`]: replacing, resuming, opening, and forking sessions.
//! - [`settings`]: effort, speed, subagent limits, and configuration reloads.
//! - [`handoff`], [`memory`], and [`recent_prompts`]: the features of the same names.
//! - [`web`] and [`remote`]: web requests, queries, auxiliary requests, and the web interface
//!   actions; [`links`] resolves the links they and the transcript open.
//! - [`background`]: single-slot tasks such as the external editor and the update check.
//! - [`lifecycle`]: shutdown ordering.
//! - [`frontend`]: the terminal, or its absence under `tact serve`.

mod background;
mod effects;
mod frontend;
mod handoff;
mod lifecycle;
mod links;
mod memory;
mod opens;
mod pane_events;
mod panes;
mod recent_prompts;
mod remote;
#[cfg(test)]
mod serve_tests;
mod sessions;
mod settings;
mod web;
mod worker_events;

use self::{
    background::{BackgroundTasks, TaskKind, TaskOutput},
    frontend::Frontend,
    handoff::HandoffController,
    lifecycle::Lifecycle,
    memory::Memory,
    opens::PendingOpens,
    panes::{PaneAgent, PaneReports, PaneSession, PaneSettings, Panes},
    recent_prompts::RecentPrompts,
    sessions::RestoredSession,
    web::{WebServer, WebTaskCompletion},
    worker_events::{BusyTurns, WorkerLink},
};
use super::{
    StartupMode, clipboard,
    components::{AppEvent, AppNode, RenderRequest, RootNode},
    scheduler::{RenderScheduler, STREAM_FRAME_INTERVAL},
    system_scheme,
};
use crate::{
    app::{
        config::Config,
        error::{Result, RuntimeError},
        herdr,
        theme::ColorScheme,
    },
    core::{
        ConfiguredAgent,
        pane::PaneId,
        protocol::{AuxiliaryRequest, Origin, QueryRequest, Request},
        session::{SessionLock, SessionStore},
        shell::ShellExecution,
        supported_reasoning_mode,
        transcript::TranscriptError,
        worker::{self, WorkerEvent},
    },
    web::bridge::{self, WebStatus},
};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use std::{
    future, io,
    path::{Path, PathBuf},
    time::Instant,
};
use tokio::{
    sync::{mpsc, watch},
    task::JoinSet,
    time::sleep_until,
};
use tokio_util::sync::CancellationToken;

/// Which front-end the loop serves beside the web interface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Interface {
    /// The interactive terminal. The web server is optional and its failure is not fatal.
    Terminal,
    /// `tact serve`: the web server is the only front-end, so failing to start it is fatal.
    Headless,
}

/// Serves `interface` and the web front-end until shutdown, returning the main session's ID when
/// it can be resumed.
pub(super) async fn run(
    config: Config,
    startup: StartupMode,
    shutdown: CancellationToken,
    interface: Interface,
) -> Result<Option<String>> {
    let (mut event_loop, mut inbox, web_server) =
        EventLoop::start(config, startup, shutdown, interface).await?;
    event_loop.serve(&mut inbox).await?;
    let outcome = event_loop.outcome();
    let EventLoop { frontend, .. } = event_loop;
    drop(frontend);
    web_server.stop().await;
    outcome
}

/// The receiving ends of the channels the loop selects over.
struct Inbox {
    worker: mpsc::UnboundedReceiver<WorkerEvent>,
    panes: PaneReports,
    web_requests: mpsc::UnboundedReceiver<Request>,
    web_queries: mpsc::UnboundedReceiver<QueryRequest>,
    auxiliary_requests: mpsc::UnboundedReceiver<AuxiliaryRequest>,
    system_scheme: mpsc::UnboundedReceiver<ColorScheme>,
}

/// Everything the event loop owns and mutates.
struct EventLoop {
    config: Config,
    /// The workspace Tact started in, used when no pane's root reports one.
    workspace: PathBuf,
    shutdown: CancellationToken,
    lifecycle: Lifecycle,
    app: AppNode,
    frontend: Frontend,
    scheduler: RenderScheduler,
    worker: WorkerLink,
    busy_turns: BusyTurns,
    panes: Panes,
    tasks: BackgroundTasks,
    opens: PendingOpens,
    shells: JoinSet<(PaneId, ShellExecution)>,
    web_status: watch::Receiver<WebStatus>,
    web_tasks: JoinSet<WebTaskCompletion>,
    memory: Memory,
    recent_prompts: RecentPrompts,
    handoff: HandoffController,
    /// The first journal writer failure; it ends the loop with an error.
    writer_error: Option<TranscriptError>,
}

impl EventLoop {
    /// Configures the main pane's agent, enters the terminal when there is one, and starts the
    /// worker, the web server, and the startup background tasks.
    async fn start(
        config: Config,
        startup: StartupMode,
        shutdown: CancellationToken,
        interface: Interface,
    ) -> Result<(Self, Inbox, WebServer)> {
        let initial_effort = config.agent().thinking();
        let initial_speed = config.agent().speed();
        let preferred_reasoning_mode = config.agent().reasoning_mode();
        let open_resume_selector = matches!(&startup, StartupMode::ResumeSelector(_));
        let (configured, restored, reasoning_mode, model, history) = match startup {
            StartupMode::ResumeSession(session_id) => {
                let lock = SessionLock::acquire(config.path(), &session_id)?;
                let RestoredSession {
                    configured,
                    lock,
                    records,
                    projection,
                    reasoning_mode,
                    model,
                    next_sequence,
                } = sessions::restore_session(config.clone(), session_id, initial_effort, lock)
                    .await?;
                (
                    configured,
                    Some((projection, records)),
                    reasoning_mode,
                    model,
                    Some((next_sequence, lock)),
                )
            }
            StartupMode::NewSession(model) | StartupMode::ResumeSelector(model) => {
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
                    None,
                )
            }
        };
        let workspace = config.agent().workspace().to_path_buf();
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
        let frontend = match interface {
            Interface::Terminal => {
                Frontend::terminal(&initial_workspace).map_err(RuntimeError::Terminal)?
            }
            Interface::Headless => Frontend::Headless,
        };
        let main_session_id = agent.session_id().to_string();
        let (session, lock, memory_review) = match history {
            Some((next_sequence, lock)) => (
                PaneSession::resumed(&main_session_id, next_sequence),
                lock,
                worker::MemoryReviewState::restored(memory_enabled),
            ),
            None => (
                PaneSession::fresh(&main_session_id),
                SessionLock::acquire(config.path(), &main_session_id)?,
                worker::MemoryReviewState::fresh(memory_enabled),
            ),
        };
        let (mut panes, pane_reports) = Panes::new();
        panes.open(
            PaneId::Main,
            0,
            session,
            PaneAgent {
                settings: PaneSettings::new(initial_effort, reasoning_mode, initial_speed, model),
                instructions,
                skills_catalog_present: !skills.is_empty(),
                subagent_control: subagent_control.clone(),
            },
            &config.with_workspace(initial_workspace.clone()),
            lock,
        )?;
        let busy_turns = BusyTurns::new(herdr::Reporter::from_env(&main_session_id));
        let (commands, worker_events) =
            worker::spawn(agent, context, memory_review, shutdown.clone());
        panes.forward_agent_events(PaneId::Main, 0, events);
        panes.forward_subagent_updates(&subagent_control, subagent_updates);

        let mut root = RootNode::new(&initial_workspace, initial_effort);
        root.set_tui_config(*config.tui());
        root.set_claude_enabled(config.claude().enabled());
        root.set_reasoning_modes(reasoning_mode, preferred_reasoning_mode);
        root.set_speed(initial_speed);
        root.set_max_subagents(config.agent().max_subagents());
        let memory = Memory::new(crate::core::configured_memory_store(&config, &workspace)?);
        root.set_memory_enabled(memory.is_enabled());
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
            requests: web_requests,
            queries: web_queries,
            auxiliary: auxiliary_requests,
            status: web_status,
        } = loop_end;
        app.attach_publisher(publisher);
        app.session_opened(
            PaneId::Main,
            main_session_id,
            restored_records.unwrap_or_default(),
        );
        let web_server = match interface {
            Interface::Terminal => WebServer::spawn(&config, &workspace, web_end, &shutdown),
            Interface::Headless => WebServer::serve(&config, &workspace, web_end, &shutdown)
                .await
                .inspect_err(|_| shutdown.cancel())?,
        };
        let mut tasks = BackgroundTasks::default();
        tasks.spawn(
            TaskKind::RecentPrompts,
            recent_prompts::load(SessionStore::new(config.path())),
        );
        if !crate::app::installation::current().is_development() {
            tasks.spawn(TaskKind::UpdateCheck, async {
                TaskOutput::UpdateCheck(crate::app::update::check_for_update().await)
            });
        }
        let (system_scheme_sender, system_scheme_updates) = mpsc::unbounded_channel();
        system_scheme::watch_system_scheme(system_scheme_sender, shutdown.clone());

        let inbox = Inbox {
            worker: worker_events,
            panes: pane_reports,
            web_requests,
            web_queries,
            auxiliary_requests,
            system_scheme: system_scheme_updates,
        };
        let mut event_loop = Self {
            config,
            workspace,
            shutdown,
            lifecycle: Lifecycle::Running,
            app,
            frontend,
            scheduler: RenderScheduler::new(STREAM_FRAME_INTERVAL, Instant::now()),
            worker: WorkerLink::new(commands),
            busy_turns,
            panes,
            tasks,
            opens: PendingOpens::default(),
            shells: JoinSet::new(),
            web_status,
            web_tasks: JoinSet::new(),
            memory,
            recent_prompts: RecentPrompts::default(),
            handoff: HandoffController::new(),
            writer_error: None,
        };
        if open_resume_selector {
            let update = event_loop.app.open_resume_selector();
            event_loop.apply_update(update).await?;
        }
        Ok((event_loop, inbox, web_server))
    }

    /// Runs the select loop until shutdown has drained every journal writer.
    async fn serve(&mut self, inbox: &mut Inbox) -> Result<()> {
        loop {
            self.report_active_workspace()?;
            if self.advance_shutdown()?.is_break() {
                return Ok(());
            }
            self.present()?;

            let running = self.lifecycle.is_running();
            let rendering = running
                && matches!(self.frontend, Frontend::Terminal { .. })
                && !self.tasks.is_active(TaskKind::Editor);
            let render_deadline = self.scheduler.deadline();
            let animation_deadline = self.app.animation_deadline();
            tokio::select! {
                () = self.shutdown.cancelled(), if running => self.begin_shutdown().await,
                event = self.frontend.next_input(), if self.frontend.has_input() && running => {
                    self.on_terminal_event(event).await?;
                }
                Some(scheme) = inbox.system_scheme.recv(), if running => {
                    self.show(AppEvent::SystemThemeChanged(scheme));
                }
                event = inbox.panes.agent_events.recv(), if self.panes.has_live_agents() => {
                    self.on_agent_event(event).await?;
                }
                Some(update) = inbox.panes.subagent_updates.recv(), if running => {
                    self.on_subagent_update(update).await?;
                }
                Some(request) = inbox.web_requests.recv(),
                    if running && !self.tasks.defers_web_commands() =>
                {
                    self.on_web_request(request).await?;
                }
                Some(request) = inbox.web_queries.recv(), if running => self.on_web_query(request),
                Some(request) = inbox.auxiliary_requests.recv(), if running => {
                    self.on_auxiliary_request(request)?;
                }
                Some(result) = self.opens.join_next(), if running => {
                    self.on_open_finished(result.map_err(RuntimeError::SessionTask)?).await?;
                }
                event = inbox.worker.recv(), if self.worker.is_running() => {
                    self.on_worker_event(event).await?;
                }
                Some(result) = self.shells.join_next(), if !self.shells.is_empty() => {
                    if let Ok((pane, execution)) = result {
                        self.on_shell_finished(pane, execution).await?;
                    }
                }
                Some(result) = self.web_tasks.join_next(), if !self.web_tasks.is_empty() => {
                    if let Ok(completion) = result {
                        self.on_web_task_finished(completion);
                    }
                }
                Some(result) = self.memory.join_next(), if running => {
                    if let Ok(Some(event)) = result {
                        self.show(event);
                    }
                }
                Some(_) = self.panes.subagent_shutdowns.join_next(),
                    if !self.panes.subagent_shutdowns.is_empty() => {}
                result = self.handoff.finished(), if self.handoff.is_active() && running => {
                    self.on_handoff_finished(result.map_err(RuntimeError::HandoffTask)?)?;
                }
                Some(output) = self.tasks.join_next(), if !self.tasks.is_empty() && running => {
                    self.on_task_finished(output?).await?;
                }
                completion = inbox.panes.writer_completions.recv(), if self.panes.has_open_writers() => {
                    self.on_writer_finished(completion).await;
                }
                () = wait_until(animation_deadline), if animation_deadline.is_some() && rendering => {
                    self.show(AppEvent::AnimationFrame(Instant::now()));
                }
                () = wait_until(render_deadline), if render_deadline.is_some() && rendering => {}
            }
        }
    }

    /// Publishes changed state to web clients and draws the terminal, if any, when a frame is due.
    fn present(&mut self) -> Result<()> {
        let render = self.app.publish_changes(Origin::Terminal);
        self.scheduler.request(render, Instant::now());
        let Frontend::Terminal { session, .. } = &mut self.frontend else {
            return Ok(());
        };
        if self.lifecycle.is_running()
            && !self.tasks.is_active(TaskKind::Editor)
            && self.scheduler.is_due(Instant::now())
        {
            session
                .draw(|frame| self.app.render(frame))
                .map_err(RuntimeError::Terminal)?;
            self.scheduler.presented(Instant::now());
        }
        Ok(())
    }

    /// The workspace of the active pane, which the terminal reports as its working directory.
    fn active_workspace(&self) -> &Path {
        self.app
            .root(self.app.active_pane())
            .map_or(&self.workspace, RootNode::workspace)
    }

    /// Reports the active pane's workspace to the terminal when it differs from the last report.
    fn report_active_workspace(&mut self) -> Result<()> {
        let Some(root) = self.app.root(self.app.active_pane()) else {
            return Ok(());
        };
        self.frontend
            .report_workspace(root.workspace(), false)
            .map_err(|error| RuntimeError::Terminal(error).into())
    }

    /// Reports the active pane's workspace to the terminal unconditionally, for when the terminal
    /// may have lost it.
    fn rereport_active_workspace(&mut self) -> Result<()> {
        let workspace = self.active_workspace().to_owned();
        self.frontend
            .report_workspace(&workspace, true)
            .map_err(|error| RuntimeError::Terminal(error).into())
    }

    /// Delivers an event whose handling cannot request effects, scheduling the render it asks for.
    fn show(&mut self, event: AppEvent) {
        let update = self.app.update(event);
        debug_assert!(update.effects.is_empty());
        self.scheduler.request(update.render, Instant::now());
    }

    /// Delivers an event and applies the effects its handling requests.
    async fn apply(&mut self, event: AppEvent) -> Result<()> {
        let update = self.app.update(event);
        self.apply_update(update).await
    }

    async fn on_terminal_event(&mut self, event: Option<io::Result<Event>>) -> Result<()> {
        let event = event
            .transpose()
            .map_err(RuntimeError::Terminal)?
            .ok_or_else(|| {
                RuntimeError::Terminal(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "terminal input closed",
                ))
            })?;
        // Focus changes and clicks can leave the terminal's cursor out of step with the frame.
        let refresh_cursor = matches!(&event, Event::FocusGained)
            || matches!(
                &event,
                Event::Mouse(mouse) if matches!(mouse.kind, MouseEventKind::Down(_))
            );
        if refresh_cursor && let Ok(session) = self.frontend.session() {
            session.invalidate_cursor_visibility();
        }
        let mut update = if is_image_paste(&event)
            && let Some(data_url) = clipboard::image_data_url()
        {
            self.app.update(AppEvent::PasteImage(data_url))
        } else {
            self.app.update(AppEvent::Terminal(event))
        };
        if refresh_cursor {
            update.render = update.render.max(RenderRequest::Immediate);
        }
        self.apply_update(update).await
    }

    /// Stops accepting new work. Background tasks are aborted and awaited, so an external editor
    /// has released the terminal and settings being persisted are written before the loop drains.
    async fn begin_shutdown(&mut self) {
        self.lifecycle.stop();
        self.frontend.detach_input();
        self.tasks.shutdown().await;
    }

    /// The main session to offer for resumption, or the error that ended the loop.
    fn outcome(&mut self) -> Result<Option<String>> {
        let session_id = self
            .app
            .main_pane()
            .and_then(|pane| self.panes.get(pane))
            .and_then(|runtime| runtime.exit_session_id());
        if let Some(error) = self.writer_error.take() {
            return Err(error.into());
        }
        self.worker
            .take_error()
            .map_or(Ok(session_id), |error| Err(error.into()))
    }
}

async fn wait_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => sleep_until(deadline.into()).await,
        None => future::pending().await,
    }
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

#[cfg(test)]
mod tests {
    use super::is_image_paste;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn control_or_super_v_requests_an_image_paste() {
        let key = |modifiers| Event::Key(KeyEvent::new(KeyCode::Char('v'), modifiers));

        assert!(is_image_paste(&key(KeyModifiers::CONTROL)));
        assert!(is_image_paste(&key(KeyModifiers::SUPER)));
        assert!(!is_image_paste(&key(KeyModifiers::NONE)));
    }
}
