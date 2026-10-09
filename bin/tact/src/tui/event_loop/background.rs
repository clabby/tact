//! Single-slot background tasks awaited by the event loop.
//!
//! Each [`TaskKind`] has at most one task in flight. Spawning a task of a kind that is already
//! running aborts the earlier task, whose result is never delivered, so a completion always
//! answers the newest request of its kind.

use super::{
    EventLoop,
    sessions::{NewSession, ResumedSession},
    settings::{EffortUpdate, SpeedUpdate},
};
use crate::{
    app::{
        error::{ExternalEditorError, Result, RuntimeError},
        update::UpdateError,
    },
    core::{
        pane::PaneId,
        session::{RecentPrompt, SessionSummary},
    },
    tui::{
        components::AppEvent,
        editor::{EditorCompletion, EditorOutcome},
    },
};
use std::{collections::HashMap, future::Future, time::Instant};
use tokio::task::{AbortHandle, Id, JoinError, JoinSet};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum TaskKind {
    /// An external editor that owns the terminal.
    Editor,
    /// Persisting a new effort before the worker applies it.
    Effort,
    /// Persisting a new speed before the worker applies it.
    Speed,
    /// Configuring a fresh agent to replace a pane's session.
    NewSession,
    /// Listing stored sessions for a picker.
    SessionList,
    /// Loading the recent prompts of every stored session.
    RecentPrompts,
    /// Restoring a stored session into a pane.
    ResumeSession,
    /// Checking for a newer release.
    UpdateCheck,
}

impl TaskKind {
    /// Whether web commands wait while a task of this kind runs. These tasks change a pane's agent
    /// or settings when they finish, so a command applied meanwhile could act on stale state.
    fn defers_web_commands(self) -> bool {
        matches!(
            self,
            Self::Effort | Self::Speed | Self::NewSession | Self::ResumeSession
        )
    }

    /// The error a failed task of this kind ends the loop with; an update check may fail silently.
    fn join_error(self, error: JoinError) -> Option<RuntimeError> {
        match self {
            Self::Editor => Some(RuntimeError::ExternalEditorTask(error)),
            Self::Effort => Some(RuntimeError::EffortUpdateTask(error)),
            Self::Speed => Some(RuntimeError::SpeedUpdateTask(error)),
            Self::NewSession => Some(RuntimeError::NewSessionTask(error)),
            Self::SessionList | Self::RecentPrompts | Self::ResumeSession => {
                Some(RuntimeError::SessionTask(error))
            }
            Self::UpdateCheck => None,
        }
    }
}

/// The result of a background task, one variant per [`TaskKind`].
pub(super) enum TaskOutput {
    Editor(std::result::Result<EditorCompletion, ExternalEditorError>),
    Effort(Result<EffortUpdate>),
    Speed(Result<SpeedUpdate>),
    NewSession(NewSession),
    SessionList {
        pane: PaneId,
        sessions: Result<Vec<SessionSummary>>,
    },
    RecentPrompts(Result<Vec<RecentPrompt>>),
    ResumeSession(Box<ResumedSession>),
    UpdateCheck(std::result::Result<Option<semver::Version>, UpdateError>),
}

/// The set of in-flight background tasks, at most one per kind.
#[derive(Default)]
pub(super) struct BackgroundTasks {
    set: JoinSet<TaskOutput>,
    slots: HashMap<TaskKind, AbortHandle>,
}

impl BackgroundTasks {
    /// Spawns `task` as the task of its kind, aborting the task it replaces.
    pub(super) fn spawn(
        &mut self,
        kind: TaskKind,
        task: impl Future<Output = TaskOutput> + Send + 'static,
    ) {
        let handle = self.set.spawn(task);
        self.occupy(kind, handle);
    }

    /// Spawns blocking `task` as the task of its kind. A replaced blocking task runs to completion,
    /// but its result is discarded.
    pub(super) fn spawn_blocking(
        &mut self,
        kind: TaskKind,
        task: impl FnOnce() -> TaskOutput + Send + 'static,
    ) {
        let handle = self.set.spawn_blocking(task);
        self.occupy(kind, handle);
    }

    fn occupy(&mut self, kind: TaskKind, handle: AbortHandle) {
        if let Some(replaced) = self.slots.insert(kind, handle) {
            replaced.abort();
        }
    }

    pub(super) fn is_active(&self, kind: TaskKind) -> bool {
        self.slots.contains_key(&kind)
    }

    pub(super) fn is_empty(&self) -> bool {
        self.set.is_empty()
    }

    pub(super) fn defers_web_commands(&self) -> bool {
        self.slots.keys().any(|kind| kind.defers_web_commands())
    }

    /// Waits for the next task that was not replaced or aborted. Returns `None` once no task
    /// remains. Cancel safe: a result is only taken when it is returned.
    pub(super) async fn join_next(&mut self) -> Option<Result<TaskOutput>> {
        loop {
            let (id, result) = match self.set.join_next_with_id().await? {
                Ok((id, output)) => (id, Ok(output)),
                Err(error) => (error.id(), Err(error)),
            };
            let Some(kind) = self.release(id) else {
                continue;
            };
            match result {
                Ok(output) => return Some(Ok(output)),
                Err(error) => {
                    if let Some(error) = kind.join_error(error) {
                        return Some(Err(error.into()));
                    }
                }
            }
        }
    }

    fn release(&mut self, id: Id) -> Option<TaskKind> {
        let kind = self
            .slots
            .iter()
            .find_map(|(&kind, handle)| (handle.id() == id).then_some(kind))?;
        self.slots.remove(&kind);
        Some(kind)
    }

    /// Aborts every task; none of their results are delivered.
    pub(super) fn abort_all(&mut self) {
        self.slots.clear();
        self.set.abort_all();
    }

    /// Aborts every task and waits until each has stopped. Blocking tasks cannot be interrupted,
    /// so this also waits for them to finish.
    pub(super) async fn shutdown(&mut self) {
        self.slots.clear();
        self.set.shutdown().await;
    }
}

impl EventLoop {
    pub(super) async fn on_task_finished(&mut self, output: TaskOutput) -> Result<()> {
        match output {
            TaskOutput::Editor(completion) => self.on_editor_closed(completion)?,
            TaskOutput::Effort(update) => self.on_effort_persisted(update?)?,
            TaskOutput::Speed(update) => self.on_speed_persisted(update?)?,
            TaskOutput::NewSession(session) => self.on_new_session_configured(session)?,
            TaskOutput::SessionList { pane, sessions } => self.on_sessions_listed(pane, sessions),
            TaskOutput::RecentPrompts(prompts) => self.on_recent_prompts_loaded(prompts),
            TaskOutput::ResumeSession(session) => self.on_session_restored(*session)?,
            TaskOutput::UpdateCheck(Ok(Some(version))) => {
                self.show(AppEvent::UpdateAvailable(version));
            }
            TaskOutput::UpdateCheck(_) => {}
        }
        Ok(())
    }

    /// Takes the terminal back from the external editor and applies what the editor produced.
    fn on_editor_closed(
        &mut self,
        completion: std::result::Result<EditorCompletion, ExternalEditorError>,
    ) -> Result<()> {
        // Only a terminal runs the editor, so a headless loop never gets here.
        if let Ok(session) = self.frontend.session() {
            session.resume().map_err(RuntimeError::Terminal)?;
        }
        self.rereport_active_workspace()?;
        self.app.refresh_terminal_images();
        self.frontend.attach_input();
        if let EditorCompletion::Draft {
            pane,
            outcome: EditorOutcome::Updated(draft),
        } = completion?
        {
            self.show(AppEvent::EditorDraft { pane, draft });
        }
        self.scheduler.request_immediate(Instant::now());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{BackgroundTasks, TaskKind, TaskOutput};
    use crate::core::pane::PaneId;
    use tokio::sync::oneshot;

    fn session_list(pane: PaneId) -> TaskOutput {
        TaskOutput::SessionList {
            pane,
            sessions: Ok(Vec::new()),
        }
    }

    #[tokio::test]
    async fn a_replaced_task_is_aborted_and_only_the_newest_result_is_delivered() {
        let mut tasks = BackgroundTasks::default();
        let (release, released) = oneshot::channel::<()>();
        tasks.spawn(TaskKind::SessionList, async move {
            drop(released.await);
            session_list(PaneId::Main)
        });
        tasks.spawn(TaskKind::SessionList, async {
            session_list(PaneId::Fork(1))
        });

        let Some(Ok(TaskOutput::SessionList { pane, .. })) = tasks.join_next().await else {
            panic!("the replacing task should complete");
        };
        assert_eq!(pane, PaneId::Fork(1));
        assert!(!tasks.is_active(TaskKind::SessionList));
        assert!(tasks.join_next().await.is_none());
        assert!(release.is_closed());
    }

    #[tokio::test]
    async fn only_tasks_that_change_a_pane_defer_web_commands() {
        let mut tasks = BackgroundTasks::default();
        tasks.spawn(TaskKind::SessionList, std::future::pending());
        tasks.spawn(TaskKind::Editor, std::future::pending());
        assert!(!tasks.defers_web_commands());

        tasks.spawn(TaskKind::NewSession, std::future::pending());
        assert!(tasks.defers_web_commands());

        tasks.abort_all();
        assert!(!tasks.defers_web_commands());
        assert!(tasks.join_next().await.is_none());
    }
}
