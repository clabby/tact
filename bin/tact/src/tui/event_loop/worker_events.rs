//! The loop's link to the turn worker and its handling of worker events.

use super::EventLoop;
use crate::{
    app::{
        error::{Result, RuntimeError},
        herdr, hook,
    },
    core::{
        context::ContextBudget,
        pane::PaneId,
        prompt::{QueueId, Submission},
        session::AgentSnapshot,
        transcript::{LocalEvent, TerminalStopReason, TurnId},
        worker::{WorkerCommand, WorkerError, WorkerEvent},
    },
    tui::components::AppEvent,
};
use nanocodex::NanocodexError;
use std::collections::HashSet;
use tokio::sync::mpsc;

/// The sending end of the worker's command channel and what the loop knows of the worker's state.
pub(super) struct WorkerLink {
    commands: mpsc::UnboundedSender<WorkerCommand>,
    state: WorkerState,
}

enum WorkerState {
    Running,
    /// The worker reported that it stopped, with the error that stopped it, or its event channel
    /// closed.
    Stopped(Option<NanocodexError>),
}

impl WorkerLink {
    pub(super) fn new(commands: mpsc::UnboundedSender<WorkerCommand>) -> Self {
        Self {
            commands,
            state: WorkerState::Running,
        }
    }

    pub(super) fn send(&self, command: WorkerCommand) -> Result<()> {
        self.commands
            .send(command)
            .map_err(|_| RuntimeError::AgentWorkerStopped.into())
    }

    /// A sender for tasks that issue worker commands on their own.
    pub(super) fn commands(&self) -> mpsc::UnboundedSender<WorkerCommand> {
        self.commands.clone()
    }

    pub(super) fn is_running(&self) -> bool {
        matches!(self.state, WorkerState::Running)
    }

    pub(super) fn error(&self) -> Option<&NanocodexError> {
        match &self.state {
            WorkerState::Stopped(error) => error.as_ref(),
            WorkerState::Running => None,
        }
    }

    pub(super) fn take_error(&mut self) -> Option<NanocodexError> {
        match &mut self.state {
            WorkerState::Stopped(error) => error.take(),
            WorkerState::Running => None,
        }
    }
}

/// Reports to herdr whether any turn is running, on behalf of the main pane's session.
pub(super) struct BusyTurns {
    reporter: herdr::Reporter,
    turns: HashSet<(PaneId, TurnId)>,
}

impl BusyTurns {
    pub(super) fn new(reporter: herdr::Reporter) -> Self {
        Self {
            reporter,
            turns: HashSet::new(),
        }
    }

    fn accepted(&mut self, pane: PaneId, id: TurnId, main_session_id: Option<&str>) {
        if self.turns.insert((pane, id)) && self.turns.len() == 1 {
            self.reporter.working(main_session_id);
        }
    }

    fn finished(&mut self, pane: PaneId, id: TurnId, main_session_id: Option<&str>) {
        if self.turns.remove(&(pane, id)) && self.turns.is_empty() {
            self.reporter.idle(main_session_id);
        }
    }
}

impl EventLoop {
    pub(super) async fn on_worker_event(&mut self, event: Option<WorkerEvent>) -> Result<()> {
        let Some(event) = event else {
            self.worker.state = WorkerState::Stopped(None);
            return Ok(());
        };
        match event {
            WorkerEvent::Stopped { error } => self.on_worker_stopped(error)?,
            WorkerEvent::ContextBudget {
                pane,
                session_id,
                budget,
            } => self.on_context_budget(pane, &session_id, budget)?,
            WorkerEvent::TurnAccepted { pane, id } => self.on_turn_accepted(pane, id)?,
            WorkerEvent::CompactionFinished {
                pane,
                result,
                terminal_stop,
                duration_ns,
            } => {
                self.on_compaction_finished(pane, result, terminal_stop, duration_ns)
                    .await?;
            }
            WorkerEvent::TurnFinished {
                pane,
                id,
                error,
                terminal_stop,
                snapshot,
                terminal_expected,
            } => {
                let turn = FinishedTurn {
                    id,
                    error,
                    terminal_stop,
                    snapshot,
                    terminal_expected,
                };
                self.on_turn_finished(pane, turn).await?;
            }
            WorkerEvent::SteerAdmitted { pane, queue_id } => {
                self.apply(AppEvent::SteerAdmitted { pane, id: queue_id })
                    .await?;
            }
            WorkerEvent::SteerPromoted {
                pane,
                queue_id,
                id,
                prompt,
            } => self.on_steer_promoted(pane, queue_id, id, &prompt)?,
            WorkerEvent::SteerFailed {
                pane,
                queue_id,
                error,
            } => self.on_steer_failed(pane, queue_id, &error).await?,
            WorkerEvent::TurnsCancelled { pane, count, error } => {
                self.on_turns_cancelled(pane, count, error)?;
            }
            WorkerEvent::ForkOpened {
                pane,
                parent,
                parent_sequence,
                events,
            } => {
                self.on_fork_opened(pane, parent, parent_sequence, events)
                    .await?
            }
            WorkerEvent::ForkFailed { pane, error } => {
                self.on_fork_failed(pane, error.to_string()).await?;
            }
            WorkerEvent::ThinkingUpdated {
                pane,
                effort,
                result,
            } => self.on_effort_updated(pane, effort, result).await?,
            WorkerEvent::SpeedUpdated {
                pane,
                speed,
                result,
            } => {
                result?;
                self.on_speed_updated(pane, speed)?;
            }
        }
        Ok(())
    }

    /// Records the worker's stop in every pane that has a transcript.
    fn on_worker_stopped(&mut self, error: Option<NanocodexError>) -> Result<()> {
        let panes = self
            .panes
            .runtimes()
            .map(|runtime| runtime.pane)
            .collect::<Vec<_>>();
        for pane in panes {
            let runtime = self.panes.runtime(pane)?;
            if runtime.journal_mut()?.is_empty() {
                continue;
            }
            let record = runtime.record(LocalEvent::WorkerStopped {
                error: error.as_ref().map(ToString::to_string),
            })?;
            self.show(AppEvent::Transcript { pane, record });
        }
        self.worker.state = WorkerState::Stopped(error);
        Ok(())
    }

    /// Shows a pane's context budget; once the transcript has records, the budget is journaled so
    /// a resumed session can show it too.
    fn on_context_budget(
        &mut self,
        pane: PaneId,
        session_id: &str,
        budget: ContextBudget,
    ) -> Result<()> {
        let Some(runtime) = self
            .panes
            .get_mut(pane)
            .filter(|runtime| runtime.session_id == session_id)
        else {
            return Ok(());
        };
        let event = if runtime.journal_mut()?.is_empty() {
            AppEvent::ContextBudget { pane, budget }
        } else {
            let record = runtime.record(LocalEvent::ContextBudget(budget))?;
            AppEvent::Transcript { pane, record }
        };
        self.show(event);
        Ok(())
    }

    fn on_turn_accepted(&mut self, pane: PaneId, id: TurnId) -> Result<()> {
        let main_session_id = self
            .app
            .main_pane()
            .and_then(|main| self.panes.session_id(main));
        self.busy_turns.accepted(pane, id, main_session_id);
        let Some(runtime) = self.panes.get_mut(pane) else {
            return Ok(());
        };
        let record = runtime.record(LocalEvent::WorkerTurnAccepted { id })?;
        self.show(AppEvent::Transcript { pane, record });
        Ok(())
    }

    async fn on_compaction_finished(
        &mut self,
        pane: PaneId,
        result: std::result::Result<Box<AgentSnapshot>, WorkerError>,
        terminal_stop: Option<TerminalStopReason>,
        duration_ns: u64,
    ) -> Result<()> {
        let Some(runtime) = self.panes.get_mut(pane) else {
            return Ok(());
        };
        let (snapshot, error) = match result {
            Ok(snapshot) => (Some(snapshot), None),
            Err(error) => (None, Some(error.to_string())),
        };
        let event = LocalEvent::CompactionFinished {
            error,
            duration_ns,
            terminal_stop,
        };
        let record = match snapshot.as_deref() {
            Some(snapshot) => runtime.record_checkpoint(event, snapshot)?,
            None => runtime.record(event)?,
        };
        runtime.journal_mut()?.flush().await?;
        self.show(AppEvent::Transcript { pane, record });
        if let Some(budget) = snapshot.and_then(|snapshot| snapshot.context_budget()) {
            let record = self
                .panes
                .runtime(pane)?
                .record(LocalEvent::ContextBudget(budget))?;
            self.show(AppEvent::Transcript { pane, record });
        }
        self.apply(AppEvent::CompactionFinished(pane)).await
    }

    async fn on_turn_finished(&mut self, pane: PaneId, turn: FinishedTurn) -> Result<()> {
        let main_session_id = self
            .app
            .main_pane()
            .and_then(|main| self.panes.session_id(main));
        self.busy_turns.finished(pane, turn.id, main_session_id);
        let Some(runtime) = self.panes.get_mut(pane) else {
            return Ok(());
        };
        let event = LocalEvent::WorkerTurnFinished {
            id: turn.id,
            error: turn.error.map(|error| error.to_string()),
            terminal_stop: turn.terminal_stop,
        };
        let record = match turn.snapshot.as_deref() {
            Some(snapshot) => runtime.record_checkpoint(event, snapshot)?,
            None => runtime.record(event)?,
        };
        if turn.terminal_stop.is_some() {
            // Resume reads must observe the stop before the pane admits another command.
            runtime.journal_mut()?.flush().await?;
        }
        self.show(AppEvent::Transcript { pane, record });
        if let Some(command) = self.config.agent().completion_hook() {
            let command = command.to_owned();
            let workspace = self
                .app
                .root(pane)
                .expect("hook pane exists")
                .workspace()
                .to_owned();
            tokio::spawn(async move {
                drop(hook::execute(&command, &workspace).await);
            });
        }
        self.apply(AppEvent::WorkerTurnFinished {
            pane,
            terminal_expected: turn.terminal_expected,
        })
        .await
    }

    fn on_steer_promoted(
        &mut self,
        pane: PaneId,
        queue_id: QueueId,
        id: TurnId,
        prompt: &Submission,
    ) -> Result<()> {
        let Some(runtime) = self.panes.get_mut(pane) else {
            return Ok(());
        };
        let record = runtime.record(LocalEvent::UserSubmitted {
            id,
            text: prompt.display_text().to_owned(),
        })?;
        self.show(AppEvent::Transcript { pane, record });
        self.show(AppEvent::SteerPromoted { pane, id: queue_id });
        Ok(())
    }

    async fn on_steer_failed(
        &mut self,
        pane: PaneId,
        queue_id: QueueId,
        error: &WorkerError,
    ) -> Result<()> {
        let Some(runtime) = self.panes.get_mut(pane) else {
            return Ok(());
        };
        let record = runtime.record(LocalEvent::WorkerSteerFailed {
            error: error.to_string(),
        })?;
        self.show(AppEvent::Transcript { pane, record });
        self.apply(AppEvent::SteerFailed { pane, id: queue_id })
            .await
    }

    fn on_turns_cancelled(
        &mut self,
        pane: PaneId,
        count: usize,
        error: Option<NanocodexError>,
    ) -> Result<()> {
        let Some(runtime) = self.panes.get_mut(pane) else {
            return Ok(());
        };
        if count > 0 || error.is_some() {
            let record = runtime.record(LocalEvent::WorkerTurnsInterrupted {
                count,
                error: error.map(|error| error.to_string()),
            })?;
            self.show(AppEvent::Transcript { pane, record });
        }
        self.show(AppEvent::TurnsCancelled(pane));
        Ok(())
    }
}

/// A finished turn as the worker reported it.
struct FinishedTurn {
    id: TurnId,
    error: Option<WorkerError>,
    terminal_stop: Option<TerminalStopReason>,
    snapshot: Option<Box<AgentSnapshot>>,
    /// Whether the turn was accepted earlier, so the front-end is tracking it as running.
    terminal_expected: bool,
}
