//! Pane runtimes: what the event loop owns for each open pane's session.
//!
//! A [`PaneRuntime`] holds a pane's session lock and transcript journal, the counters that number
//! its turns and shell commands, and the submission held back while its shell commands run.
//! [`Panes`] is the registry of runtimes. It owns the senders through which agent event forwarders
//! and journal writers report back to the loop, counts journal writers whose completion is
//! outstanding, and tracks subagent shutdowns, so shutdown can tell when every pane has drained.
//!
//! A runtime outlives its pane until the agent's event stream ends: closing a pane marks its agent
//! [`AgentState::Closing`], and the end of the stream removes the runtime and closes its journal.

use super::worker_events::WorkerLink;
use crate::{
    app::{
        config::{Config, ReasoningEffort, ReasoningMode, Speed},
        error::{Result, RuntimeError},
    },
    core::{
        ConfiguredAgent,
        agent_events::{self, ForwardedAgentEvent},
        extensions::Skill,
        live_sessions::{LiveSessionMessage, LiveSessionRegistration},
        pane::PaneId,
        prompt::Submission,
        session::{self, AgentSnapshot, SessionLock},
        shell::ShellExecution,
        subagent_updates::{self, ForwardedSubagentUpdate},
        transcript::{
            LocalEvent, SessionEnded, SessionOutcome, SessionStarted, ShellFinished, ShellId,
            ShellStarted, TranscriptError, TranscriptJournal, TranscriptRecord, TurnId,
        },
        worker::{self, WorkerCommand},
    },
};
use nanocodex::{AgentEvents, HarnessModel as Model};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tact_subagents::{ScopedAgentUpdate, Subagents};
use tokio::{sync::mpsc, task::JoinSet};

/// The settings a pane's agent runs with.
#[derive(Clone, Copy)]
pub(super) struct PaneSettings {
    pub(super) effort: ReasoningEffort,
    pub(super) reasoning_mode: ReasoningMode,
    pub(super) speed: Speed,
    pub(super) model: Model,
}

impl PaneSettings {
    pub(super) const fn new(
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

/// Whether a pane's session already had a stored transcript when the pane opened it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionOrigin {
    /// A fresh session or fork, stored only once its transcript receives a record.
    New,
    /// A resumed session that is already listed in storage.
    Resumed,
}

/// The session a pane's runtime journals.
pub(super) struct PaneSession<'a> {
    id: &'a str,
    parent: Option<(&'a str, u64)>,
    next_sequence: u64,
    origin: SessionOrigin,
}

impl<'a> PaneSession<'a> {
    /// A session without stored history.
    pub(super) const fn fresh(id: &'a str) -> Self {
        Self {
            id,
            parent: None,
            next_sequence: 1,
            origin: SessionOrigin::New,
        }
    }

    /// A fork of `parent` that continues from the parent's record at `parent_sequence`.
    pub(super) const fn fork(id: &'a str, parent: &'a str, parent_sequence: u64) -> Self {
        Self {
            id,
            parent: Some((parent, parent_sequence)),
            next_sequence: 1,
            origin: SessionOrigin::New,
        }
    }

    /// A stored session whose next record is numbered `next_sequence`.
    pub(super) const fn resumed(id: &'a str, next_sequence: u64) -> Self {
        Self {
            id,
            parent: None,
            next_sequence,
            origin: SessionOrigin::Resumed,
        }
    }
}

/// What a pane's runtime keeps of the agent that serves its session.
pub(super) struct PaneAgent {
    pub(super) settings: PaneSettings,
    pub(super) instructions: Arc<str>,
    pub(super) skills_catalog_present: bool,
    pub(super) subagent_control: Subagents,
}

/// How a pane's newly configured agent relates to stored history.
pub(super) enum InstalledSession {
    Fresh,
    Restored {
        next_sequence: u64,
        lock: SessionLock,
    },
}

/// What installing an agent into a pane produced.
pub(super) struct InstalledAgent {
    pub(super) session_id: String,
    pub(super) skills: Arc<[Skill]>,
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

/// A prompt numbered for the worker but not yet sent to it.
pub(super) struct PendingSubmission {
    pub(super) id: TurnId,
    pub(super) prompt: Submission,
}

/// Reported by a journal's writer task once it has drained.
pub(super) struct WriterCompletion {
    pane: PaneId,
    session_id: String,
    generation: u64,
    result: std::result::Result<(), TranscriptError>,
}

/// What the end of a pane agent's event stream means for the pane.
#[derive(Debug, Eq, PartialEq)]
pub(super) enum StreamEnd {
    /// The stream belonged to a replaced session or a pane that no longer exists.
    Stale,
    /// The pane had been closed; its runtime is gone and its journal recorded the closure.
    Removed,
    /// The pane stays open without a running agent.
    Stopped,
}

/// Everything the event loop owns for one open pane's session.
pub(super) struct PaneRuntime {
    pub(super) pane: PaneId,
    pub(super) session_id: String,
    pub(super) instructions: Arc<str>,
    pub(super) skills_catalog_present: bool,
    origin: SessionOrigin,
    /// The transcript journal; absent once the pane's journal has been closed.
    journal: Option<TranscriptJournal>,
    writer_path: PathBuf,
    /// Set by the journal writer once a record of this session reaches storage.
    persisted_transcript: Arc<AtomicBool>,
    agent: AgentState,
    next_turn: u64,
    next_shell: u64,
    active_shells: usize,
    /// Results of finished shell commands, prepended to the next submission.
    shell_context: Vec<String>,
    /// A submission that waits for the pane's shell commands to finish.
    pending_submission: Option<PendingSubmission>,
    /// The settings the pane's agent is currently running with.
    pub(super) settings: PaneSettings,
    /// Incremented whenever the pane's agent is replaced, so that events and writer completions
    /// from the replaced session can be recognised and ignored.
    pub(super) generation: u64,
    pub(super) subagent_control: Subagents,
    /// Keeps the session addressable by `message_session` while the pane owns it.
    _live: LiveSessionRegistration,
    _lock: SessionLock,
}

impl PaneRuntime {
    pub(super) fn journal_mut(&mut self) -> Result<&mut TranscriptJournal> {
        self.journal
            .as_mut()
            .ok_or_else(|| TranscriptError::WriterStopped(self.writer_path.clone()).into())
    }

    /// Appends a local event to the pane's transcript.
    pub(super) fn record(&mut self, event: LocalEvent) -> Result<Arc<TranscriptRecord>> {
        Ok(self.journal_mut()?.append_local(event)?)
    }

    /// Appends a local event together with the resume state captured by `snapshot`.
    pub(super) fn record_checkpoint(
        &mut self,
        event: LocalEvent,
        snapshot: &AgentSnapshot,
    ) -> Result<Arc<TranscriptRecord>> {
        let state =
            session::encode_checkpoint(snapshot, &self.instructions, self.skills_catalog_present)?;
        Ok(self
            .journal_mut()?
            .append_local_with_resume_state(event, state)?)
    }

    pub(super) fn next_turn_id(&mut self) -> TurnId {
        let id = TurnId::new(self.next_turn);
        self.next_turn = self.next_turn.saturating_add(1);
        id
    }

    pub(super) fn has_active_shells(&self) -> bool {
        self.active_shells > 0
    }

    /// Whether events tagged with `session_id` and `generation` belong to this runtime.
    fn is_current(&self, session_id: &str, generation: u64) -> bool {
        self.session_id == session_id && self.generation == generation
    }

    /// The session to offer for resumption after exit: one that is listed in storage.
    pub(super) fn exit_session_id(&self) -> Option<String> {
        (self.origin == SessionOrigin::Resumed || self.persisted_transcript.load(Ordering::Acquire))
            .then(|| self.session_id.clone())
    }

    /// Numbers and journals a shell command that is about to run in `workspace`.
    pub(super) fn start_shell(
        &mut self,
        command: String,
        workspace: &Path,
    ) -> Result<(ShellId, Arc<TranscriptRecord>)> {
        let id = ShellId::new(self.next_shell);
        self.next_shell = self.next_shell.saturating_add(1);
        self.active_shells = self.active_shells.saturating_add(1);
        let record = self.record(LocalEvent::ShellStarted(ShellStarted {
            id,
            command,
            workspace: workspace.to_path_buf(),
        }))?;
        Ok((id, record))
    }

    /// Journals a finished shell command and keeps its result for the next submission. Once the
    /// pane's last shell has finished, the submission held back for it is returned to be sent.
    pub(super) fn finish_shell(
        &mut self,
        execution: ShellExecution,
    ) -> Result<(Arc<TranscriptRecord>, Option<PendingSubmission>)> {
        self.active_shells = self.active_shells.saturating_sub(1);
        self.shell_context.push(execution.model_context());
        let record = self.record(LocalEvent::ShellFinished(ShellFinished {
            id: execution.id,
            output: execution.output,
            exit_code: execution.exit_code,
            duration_ns: execution.duration_ns,
            truncated: execution.truncated,
            error: execution.error,
        }))?;
        let submission = if self.active_shells == 0 {
            self.pending_submission.take()
        } else {
            None
        };
        Ok((record, submission))
    }

    /// Sends `submission` to the worker, or holds it until the pane's shell commands finish so that
    /// it carries their results.
    pub(super) fn submit(
        &mut self,
        submission: PendingSubmission,
        worker: &WorkerLink,
    ) -> Result<()> {
        if self.has_active_shells() {
            debug_assert!(self.pending_submission.is_none());
            self.pending_submission = Some(submission);
            return Ok(());
        }
        self.send_submission(submission, worker)
    }

    /// Sends `submission` to the worker, prefixed with the results of shell commands that finished
    /// since the previous submission.
    pub(super) fn send_submission(
        &mut self,
        submission: PendingSubmission,
        worker: &WorkerLink,
    ) -> Result<()> {
        let prompt = if self.shell_context.is_empty() {
            submission.prompt
        } else {
            let context = self.shell_context.join("\n\n");
            self.shell_context.clear();
            submission.prompt.prepend_text(context)
        };
        worker.send(WorkerCommand::Submit {
            pane: self.pane,
            id: submission.id,
            prompt,
        })
    }

    /// Records how the session ended and closes the journal. A journal without records is closed
    /// without one, so an unused session is never stored.
    fn close_journal(&mut self, outcome: SessionOutcome, error: Option<String>) -> Result<()> {
        let Some(mut journal) = self.journal.take() else {
            return Ok(());
        };
        if journal.is_empty() {
            return Ok(());
        }
        journal.append_local(LocalEvent::SessionEnded(SessionEnded { outcome, error }))?;
        Ok(())
    }
}

/// The receiving ends of the channels that report into [`Panes`].
pub(super) struct PaneReports {
    pub(super) agent_events: mpsc::UnboundedReceiver<ForwardedAgentEvent>,
    pub(super) subagent_updates: mpsc::UnboundedReceiver<ForwardedSubagentUpdate>,
    pub(super) writer_completions: mpsc::UnboundedReceiver<WriterCompletion>,
    pub(super) live_messages: mpsc::UnboundedReceiver<LiveSessionMessage>,
}

/// The registry of pane runtimes.
pub(super) struct Panes {
    runtimes: HashMap<PaneId, PaneRuntime>,
    agent_events: mpsc::UnboundedSender<ForwardedAgentEvent>,
    subagent_updates: mpsc::UnboundedSender<ForwardedSubagentUpdate>,
    writer_completions: mpsc::UnboundedSender<WriterCompletion>,
    live_messages: mpsc::UnboundedSender<LiveSessionMessage>,
    /// Journal writers whose completion has not been received.
    writers_open: usize,
    /// Tasks closing the subagents of closed or replaced sessions.
    pub(super) subagent_shutdowns: JoinSet<()>,
}

impl Panes {
    pub(super) fn new() -> (Self, PaneReports) {
        let (agent_events, agent_event_reports) = mpsc::unbounded_channel();
        let (subagent_updates, subagent_update_reports) = mpsc::unbounded_channel();
        let (writer_completions, writer_completion_reports) = mpsc::unbounded_channel();
        let (live_messages, live_message_reports) = mpsc::unbounded_channel();
        let panes = Self {
            runtimes: HashMap::new(),
            agent_events,
            subagent_updates,
            writer_completions,
            live_messages,
            writers_open: 0,
            subagent_shutdowns: JoinSet::new(),
        };
        let reports = PaneReports {
            agent_events: agent_event_reports,
            subagent_updates: subagent_update_reports,
            writer_completions: writer_completion_reports,
            live_messages: live_message_reports,
        };
        (panes, reports)
    }

    pub(super) fn get(&self, pane: PaneId) -> Option<&PaneRuntime> {
        self.runtimes.get(&pane)
    }

    pub(super) fn get_mut(&mut self, pane: PaneId) -> Option<&mut PaneRuntime> {
        self.runtimes.get_mut(&pane)
    }

    /// The runtime of a pane that must exist.
    pub(super) fn runtime(&mut self, pane: PaneId) -> Result<&mut PaneRuntime> {
        Ok(self
            .runtimes
            .get_mut(&pane)
            .ok_or(RuntimeError::PaneUnavailable(pane))?)
    }

    pub(super) fn session_id(&self, pane: PaneId) -> Option<&str> {
        self.get(pane).map(|runtime| runtime.session_id.as_str())
    }

    pub(super) fn runtimes(&self) -> impl Iterator<Item = &PaneRuntime> {
        self.runtimes.values()
    }

    /// Opens `session`'s journal and registers the pane's runtime, replacing any previous one.
    pub(super) fn open(
        &mut self,
        pane: PaneId,
        generation: u64,
        session: PaneSession<'_>,
        agent: PaneAgent,
        config: &Config,
        lock: SessionLock,
    ) -> Result<&mut PaneRuntime> {
        let PaneSession {
            id: session_id,
            parent,
            next_sequence,
            origin,
        } = session;
        let (mut journal, writer) =
            TranscriptJournal::open_at(config.path(), session_id, next_sequence)?;
        let writer_path = journal.path().to_path_buf();
        let persisted_transcript = journal.persistence_flag();
        journal.defer_start(SessionStarted {
            session_id: session_id.to_owned(),
            parent_session_id: parent.map(|(parent, _)| parent.to_owned()),
            parent_sequence: parent.map(|(_, sequence)| sequence),
            model: agent.settings.model.to_string(),
            effort: agent.settings.effort,
            reasoning_mode: agent.settings.reasoning_mode,
            speed: agent.settings.speed,
            workspace: config.agent().workspace().to_path_buf(),
            application_version: env!("CARGO_PKG_VERSION").to_owned(),
        });

        let completions = self.writer_completions.clone();
        let completion_session_id = session_id.to_owned();
        tokio::spawn(async move {
            let result = writer
                .into_task()
                .await
                .map_err(TranscriptError::WriterTask)
                .and_then(|result| result);
            drop(completions.send(WriterCompletion {
                pane,
                session_id: completion_session_id,
                generation,
                result,
            }));
        });
        self.writers_open = self.writers_open.saturating_add(1);

        let runtime = PaneRuntime {
            pane,
            session_id: session_id.to_owned(),
            instructions: agent.instructions,
            skills_catalog_present: agent.skills_catalog_present,
            origin,
            journal: Some(journal),
            writer_path,
            persisted_transcript,
            agent: AgentState::Running,
            next_turn: 1,
            next_shell: 1,
            active_shells: 0,
            shell_context: Vec::new(),
            pending_submission: None,
            settings: agent.settings,
            generation,
            subagent_control: agent.subagent_control,
            _live: LiveSessionRegistration::new(session_id, self.live_messages.clone()),
            _lock: lock,
        };
        self.runtimes.insert(pane, runtime);
        Ok(self
            .runtimes
            .get_mut(&pane)
            .expect("the runtime was just inserted"))
    }

    /// Forwards a pane agent's events into the loop, tagged with the pane's generation.
    pub(super) fn forward_agent_events(&self, pane: PaneId, generation: u64, events: AgentEvents) {
        agent_events::forward(pane, generation, events, self.agent_events.clone());
    }

    /// Forwards updates from the subagents of a session's root agent into the loop.
    pub(super) fn forward_subagent_updates(
        &self,
        control: &Subagents,
        updates: mpsc::UnboundedReceiver<ScopedAgentUpdate>,
    ) {
        subagent_updates::forward(control.runtime_id(), updates, self.subagent_updates.clone());
    }

    /// Installs `configured` as `pane`'s agent, closing the session it replaces, and hands the agent
    /// to the worker.
    pub(super) fn install(
        &mut self,
        pane: PaneId,
        configured: ConfiguredAgent,
        history: InstalledSession,
        settings: PaneSettings,
        config: &Config,
        worker: &WorkerLink,
    ) -> Result<InstalledAgent> {
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
        let replaced = match self.runtimes.get_mut(&pane) {
            Some(runtime) => {
                Self::shut_down_subagents(&mut self.subagent_shutdowns, runtime);
                runtime.close_journal(SessionOutcome::Closed, None)?;
                Some(runtime.generation)
            }
            None => None,
        };
        let generation = replaced.map_or(0, |generation| generation.saturating_add(1));
        let (session, lock, memory_review) = match history {
            InstalledSession::Fresh => (
                PaneSession::fresh(&session_id),
                SessionLock::acquire(config.path(), &session_id)?,
                worker::MemoryReviewState::fresh(memory_enabled),
            ),
            InstalledSession::Restored {
                next_sequence,
                lock,
            } => (
                PaneSession::resumed(&session_id, next_sequence),
                lock,
                worker::MemoryReviewState::restored(memory_enabled),
            ),
        };
        let agent_resources = PaneAgent {
            settings,
            instructions,
            skills_catalog_present: !skills.is_empty(),
            subagent_control: subagent_control.clone(),
        };
        self.open(pane, generation, session, agent_resources, config, lock)?;
        self.forward_agent_events(pane, generation, events);
        self.forward_subagent_updates(&subagent_control, subagent_updates);
        worker.send(if replaced.is_some() {
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
        })?;
        Ok(InstalledAgent { session_id, skills })
    }

    /// Starts closing a pane: its subagents are asked to close, and its runtime is removed once
    /// its agent's event stream ends.
    pub(super) fn close(&mut self, pane: PaneId) {
        let Some(runtime) = self.runtimes.get_mut(&pane) else {
            return;
        };
        Self::shut_down_subagents(&mut self.subagent_shutdowns, runtime);
        if runtime.agent == AgentState::Running {
            runtime.agent = AgentState::Closing;
        }
    }

    /// The runtime that events tagged with `session_id` and `generation` belong to, if it is
    /// still current.
    pub(super) fn current_mut(
        &mut self,
        pane: PaneId,
        session_id: &str,
        generation: u64,
    ) -> Option<&mut PaneRuntime> {
        self.runtimes
            .get_mut(&pane)
            .filter(|runtime| runtime.is_current(session_id, generation))
    }

    /// Applies the end of a pane agent's event stream.
    pub(super) fn agent_stream_ended(
        &mut self,
        pane: PaneId,
        session_id: &str,
        generation: u64,
    ) -> Result<StreamEnd> {
        let Some(runtime) = self.current_mut(pane, session_id, generation) else {
            return Ok(StreamEnd::Stale);
        };
        if std::mem::replace(&mut runtime.agent, AgentState::Stopped) != AgentState::Closing {
            return Ok(StreamEnd::Stopped);
        }
        if let Some(mut runtime) = self.runtimes.remove(&pane) {
            runtime.close_journal(SessionOutcome::Closed, None)?;
        }
        Ok(StreamEnd::Removed)
    }

    /// Marks every agent stopped once the forwarding channel has closed, returning the panes that
    /// had not observed their stream's end.
    pub(super) fn all_agent_streams_ended(&mut self) -> Vec<PaneId> {
        self.runtimes
            .iter_mut()
            .filter(|(_, runtime)| runtime.agent != AgentState::Stopped)
            .map(|(&pane, runtime)| {
                runtime.agent = AgentState::Stopped;
                pane
            })
            .collect()
    }

    /// Whether any pane's agent event stream is still open.
    pub(super) fn has_live_agents(&self) -> bool {
        self.runtimes
            .values()
            .any(|runtime| runtime.agent != AgentState::Stopped)
    }

    /// The pane whose session owns the subagent that sent `update`.
    pub(super) fn subagent_pane(&self, update: &ForwardedSubagentUpdate) -> Option<PaneId> {
        self.runtimes.iter().find_map(|(&pane, runtime)| {
            (runtime.session_id == update.root_session_id
                && runtime.subagent_control.runtime_id() == update.runtime_id)
                .then_some(pane)
        })
    }

    /// Asks every pane's subagents to close.
    pub(super) fn shut_down_all_subagents(&mut self) {
        for runtime in self.runtimes.values() {
            Self::shut_down_subagents(&mut self.subagent_shutdowns, runtime);
        }
    }

    fn shut_down_subagents(tasks: &mut JoinSet<()>, runtime: &PaneRuntime) {
        let control = runtime.subagent_control.clone();
        let root_session_id = runtime.session_id.clone();
        tasks.spawn(async move {
            control.close_all(&root_session_id).await;
        });
    }

    /// Whether every agent stream has ended and every subagent shutdown has finished.
    pub(super) fn are_drained(&self) -> bool {
        !self.has_live_agents() && self.subagent_shutdowns.is_empty()
    }

    /// Records in every journal how its session ended at shutdown and closes the journals.
    pub(super) fn close_journals(
        &mut self,
        worker_error: Option<&nanocodex::NanocodexError>,
    ) -> Result<()> {
        let outcome = if worker_error.is_some() {
            SessionOutcome::Failed
        } else {
            SessionOutcome::Cancelled
        };
        for runtime in self.runtimes.values_mut() {
            runtime.close_journal(outcome, worker_error.map(ToString::to_string))?;
        }
        Ok(())
    }

    pub(super) fn has_open_writers(&self) -> bool {
        self.writers_open > 0
    }

    /// Accounts for a drained journal writer. A writer's failure is returned: the transcript can
    /// no longer be recorded, so the loop must shut down.
    pub(super) fn writer_finished(
        &mut self,
        completion: Option<WriterCompletion>,
    ) -> std::result::Result<(), TranscriptError> {
        let Some(completion) = completion else {
            self.writers_open = 0;
            return Ok(());
        };
        self.writers_open = self.writers_open.saturating_sub(1);
        if let Some(runtime) = self.current_mut(
            completion.pane,
            &completion.session_id,
            completion.generation,
        ) {
            runtime.journal = None;
        }
        completion.result
    }
}

#[cfg(test)]
mod tests;
