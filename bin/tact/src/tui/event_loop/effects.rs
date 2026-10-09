//! Effects requested by the application tree.
//!
//! [`EventLoop::apply_update`] performs an update's effects in order and then schedules the render
//! it asks for. Pane effects act on the pane's runtime and its workspace; each has one method here
//! or beside the state it changes.

use super::{
    EventLoop,
    background::{TaskKind, TaskOutput},
    links,
    panes::PendingSubmission,
    web::WebTaskCompletion,
};
use crate::{
    app::{
        config::Setting,
        error::{Result, RuntimeError},
    },
    core::{
        pane::PaneId,
        prompt::{QueueId, Submission},
        session::RecentPrompt,
        shell,
        transcript::{LocalEvent, ReflectionStarted, UserSteered, UserSubmitted},
        worker::{ReflectionContext, WorkerCommand},
    },
    tui::{
        clipboard,
        components::{AppEffect, AppEvent, ComponentUpdate, RootEffect},
        editor::EditorTarget,
    },
};
use std::{path::Path, time::Instant};

impl EventLoop {
    /// Performs `update`'s effects in order, then schedules the render it requests.
    pub(super) async fn apply_update(&mut self, update: ComponentUpdate<AppEffect>) -> Result<()> {
        for effect in update.effects {
            match effect {
                AppEffect::OpenFork { pane, parent } => self.request_fork(pane, parent).await?,
                AppEffect::StartSession { pane, model } => {
                    self.opens.spawn_fresh(&self.config, pane, model);
                }
                AppEffect::ClosePane(pane) => {
                    self.panes.close(pane);
                    self.worker.send(WorkerCommand::ClosePane(pane))?;
                }
                AppEffect::SetTheme(mode) => self.config.persist(Setting::ThemeMode(mode))?,
                AppEffect::Shutdown => self.shutdown.cancel(),
                AppEffect::Pane { pane, effect } => self.apply_pane_effect(pane, effect)?,
            }
        }
        self.scheduler.request(update.render, Instant::now());
        Ok(())
    }

    /// Performs an effect requested by `pane`'s root, which must still exist.
    pub(super) fn apply_pane_effect(&mut self, pane: PaneId, effect: RootEffect) -> Result<()> {
        let workspace = self
            .app
            .root(pane)
            .ok_or(RuntimeError::PaneUnavailable(pane))?
            .workspace()
            .to_owned();
        let workspace = workspace.as_path();
        match effect {
            RootEffect::Submit(prompt) => self.submit(pane, prompt, workspace)?,
            RootEffect::ContinueSubagent(prompt) => {
                let runtime = self.panes.runtime(pane)?;
                let id = runtime.next_turn_id();
                runtime.submit(PendingSubmission { id, prompt }, &self.worker)?;
            }
            RootEffect::Compact => self.compact(pane)?,
            RootEffect::Reflect(instructions) => self.reflect(pane, instructions, workspace)?,
            RootEffect::RunShell(command) => self.run_shell(pane, command, workspace)?,
            RootEffect::OpenLink(destination) if links::is_web_link(&destination) => {
                self.open_web_link(pane, destination);
            }
            RootEffect::OpenLink(destination) => self.open_editor(
                pane,
                EditorTarget::File(links::local_link_path(&destination, workspace)),
                workspace,
            )?,
            RootEffect::OpenDraftEditor => {
                let text = self
                    .app
                    .root(pane)
                    .expect("editor pane must exist")
                    .composer()
                    .draft()
                    .to_owned();
                self.open_editor(pane, EditorTarget::Draft { pane, text }, workspace)?;
            }
            RootEffect::OpenConfigEditor => {
                let path = self.config.path().to_path_buf();
                self.open_editor(pane, EditorTarget::Config(path), workspace)?;
            }
            RootEffect::SetEffort {
                effort,
                reasoning_mode,
            } => self.set_effort(pane, effort, reasoning_mode),
            RootEffect::SetModel(model) => self.set_model(pane, model, workspace),
            RootEffect::SetSpeed(speed) => self.set_speed(pane, speed),
            RootEffect::SetMaxSubagents(limit) => self.set_max_subagents(limit)?,
            RootEffect::LoadMemories => self.load_memories(pane),
            RootEffect::DeleteMemory(key) => self.delete_memory(pane, key),
            RootEffect::ReloadConfig => {
                // The terminal reports the outcome through the notification the reload shows.
                let _ = self.reload_config(pane);
            }
            RootEffect::NewSession(model) => self.new_session(pane, model, workspace),
            RootEffect::LoadSessions(kind) => self.load_sessions(pane, kind, workspace),
            RootEffect::LoadRecentPrompts(current_prompts) => {
                self.load_recent_prompts(pane, current_prompts, workspace);
            }
            RootEffect::Handoff => self.start_handoff(pane),
            RootEffect::OpenWebInterface { install } => self.open_web_interface(pane, install),
            RootEffect::CopyWebLink => self.copy_web_link(pane),
            RootEffect::ShowWebQr => self.show_web_qr(pane),
            RootEffect::ResumeSession(session_id) => self.resume_session(pane, session_id),
            RootEffect::Copy(text) => self.copy(pane, &text),
            RootEffect::Steer { id, prompt } => self.steer(pane, id, prompt)?,
            RootEffect::PersistSteer(text) => self.persist_steer(pane, text, workspace)?,
            RootEffect::CancelTurns => self.cancel_turns(pane)?,
            RootEffect::CancelHandoff => {
                self.handoff.cancel();
            }
            RootEffect::Fork
            | RootEffect::OpenSessions
            | RootEffect::SetTheme(_)
            | RootEffect::Shutdown => {
                unreachable!("application effects are handled before pane dispatch")
            }
        }
        Ok(())
    }

    /// Journals a prompt and sends it to the worker once the pane's shells have finished.
    fn submit(&mut self, pane: PaneId, prompt: Submission, workspace: &Path) -> Result<()> {
        let runtime = self.panes.runtime(pane)?;
        let id = runtime.next_turn_id();
        let text = prompt.display_text().to_owned();
        let record = runtime.record(LocalEvent::UserSubmitted(UserSubmitted {
            id,
            text: text.clone(),
        }))?;
        self.recent_prompts.remember(RecentPrompt {
            text,
            recorded_at_unix_ms: record.recorded_at_unix_ms(),
            session_id: runtime.session_id.clone(),
            workspace: workspace.to_path_buf(),
        });
        self.show(AppEvent::Transcript { pane, record });
        self.panes
            .runtime(pane)?
            .submit(PendingSubmission { id, prompt }, &self.worker)
    }

    fn compact(&mut self, pane: PaneId) -> Result<()> {
        let record = self
            .panes
            .runtime(pane)?
            .record(LocalEvent::CompactionStarted)?;
        self.show(AppEvent::Transcript { pane, record });
        self.worker.send(WorkerCommand::Compact(pane))
    }

    fn reflect(&mut self, pane: PaneId, instructions: Submission, workspace: &Path) -> Result<()> {
        let runtime = self.panes.runtime(pane)?;
        debug_assert!(!runtime.has_active_shells());
        let id = runtime.next_turn_id();
        let record = runtime.record(LocalEvent::ReflectionStarted(ReflectionStarted { id }))?;
        self.show(AppEvent::Transcript { pane, record });
        self.worker.send(WorkerCommand::Reflect {
            pane,
            id,
            instructions,
            context: ReflectionContext::new(self.config.path(), workspace),
        })
    }

    fn run_shell(&mut self, pane: PaneId, command: String, workspace: &Path) -> Result<()> {
        let (id, record) = self
            .panes
            .runtime(pane)?
            .start_shell(command.clone(), workspace)?;
        self.show(AppEvent::Transcript { pane, record });
        let workspace = workspace.to_path_buf();
        self.shells
            .spawn(async move { (pane, shell::execute(id, command, workspace).await) });
        Ok(())
    }

    fn open_web_link(&mut self, pane: PaneId, destination: String) {
        self.web_tasks.spawn(async move {
            WebTaskCompletion {
                pane,
                result: crate::app::browser::open(&destination)
                    .await
                    .map(|()| None)
                    .map_err(|error| format!("Could not open link: {error}")),
            }
        });
    }

    /// Hands the terminal to the external editor until it exits. Without a terminal, `pane` is
    /// told why nothing opened.
    fn open_editor(&mut self, pane: PaneId, target: EditorTarget, workspace: &Path) -> Result<()> {
        let session = match self.frontend.session() {
            Ok(session) => session,
            Err(error) => {
                let error = error.to_string();
                self.show(AppEvent::NotifyError { pane, error });
                return Ok(());
            }
        };
        session.suspend().map_err(RuntimeError::Terminal)?;
        self.frontend.detach_input();
        let workspace = workspace.to_path_buf();
        self.tasks.spawn(TaskKind::Editor, async move {
            TaskOutput::Editor(target.edit(&workspace).await)
        });
        Ok(())
    }

    fn copy(&mut self, pane: PaneId, text: &str) {
        let copied = self
            .frontend
            .session()
            .map_err(|error| error.to_string())
            .and_then(|session| clipboard::copy_selection(session, text));
        let event = match copied {
            Ok(()) => AppEvent::NotifySuccess {
                pane,
                message: "Copied to clipboard.".to_owned(),
            },
            Err(error) => AppEvent::NotifyError { pane, error },
        };
        self.show(event);
    }

    fn steer(&mut self, pane: PaneId, queue_id: QueueId, prompt: Submission) -> Result<()> {
        let fallback_id = self.panes.runtime(pane)?.next_turn_id();
        self.worker.send(WorkerCommand::Steer {
            pane,
            queue_id,
            fallback_id,
            prompt,
        })
    }

    fn persist_steer(&mut self, pane: PaneId, text: String, workspace: &Path) -> Result<()> {
        let runtime = self.panes.runtime(pane)?;
        let record = runtime.record(LocalEvent::UserSteered(UserSteered { text: text.clone() }))?;
        self.recent_prompts.remember(RecentPrompt {
            text,
            recorded_at_unix_ms: record.recorded_at_unix_ms(),
            session_id: runtime.session_id.clone(),
            workspace: workspace.to_path_buf(),
        });
        self.show(AppEvent::Transcript { pane, record });
        Ok(())
    }

    /// Cancels the pane's turns in the worker and the turns of its subagents.
    fn cancel_turns(&mut self, pane: PaneId) -> Result<()> {
        let runtime = self.panes.runtime(pane)?;
        let subagents = runtime.subagent_control.clone();
        let root_session_id = runtime.session_id.clone();
        tokio::spawn(async move { subagents.cancel_all(&root_session_id).await });
        self.worker.send(WorkerCommand::CancelAll(pane))
    }
}
