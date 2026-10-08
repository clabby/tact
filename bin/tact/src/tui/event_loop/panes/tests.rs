use super::{
    PaneAgent, PaneReports, PaneRuntime, PaneSession, PaneSettings, Panes, PendingSubmission,
    StreamEnd,
};
use crate::{
    app::config::{Config, ConfigOverrides, ReasoningEffort, ReasoningMode, Speed},
    core::{
        pane::PaneId,
        session::{SessionLock, SessionStore},
        shell::ShellExecution,
        subagent_updates::ForwardedSubagentUpdate,
        transcript::{
            LocalEvent, LocalKind, SessionEnded, SessionOutcome, SessionStarted, TranscriptRecord,
            TurnId, UserSubmitted,
        },
        worker::WorkerCommand,
    },
    tui::event_loop::worker_events::WorkerLink,
};
use nanocodex::{HarnessModel as Model, Model as CodexModel};
use std::{fs, sync::Arc};
use tact_subagents::{AgentId, AgentStatus, AgentUpdate, Subagents};
use tempfile::TempDir;
use tokio::sync::mpsc;

/// A pane registry over a temporary config, with a worker whose commands the test can inspect.
struct Harness {
    _directory: TempDir,
    config: Config,
    panes: Panes,
    reports: PaneReports,
    subagents: Subagents,
    worker: WorkerLink,
    commands: mpsc::UnboundedReceiver<WorkerCommand>,
}

impl Harness {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        fs::write(&config_path, "").unwrap();
        let config = Config::load(ConfigOverrides {
            path: Some(config_path),
            workspace: Some(directory.path().to_path_buf()),
            ..ConfigOverrides::default()
        })
        .unwrap();
        let (panes, reports) = Panes::new();
        let (subagents, _updates) = Subagents::new(32);
        let (sender, commands) = mpsc::unbounded_channel();
        Self {
            _directory: directory,
            config,
            panes,
            reports,
            subagents,
            worker: WorkerLink::new(sender),
            commands,
        }
    }

    fn open(
        &mut self,
        pane: PaneId,
        generation: u64,
        session: PaneSession<'_>,
    ) -> &mut PaneRuntime {
        let lock = SessionLock::acquire(self.config.path(), session.id).unwrap();
        let agent = PaneAgent {
            settings: PaneSettings::new(
                ReasoningEffort::Low,
                ReasoningMode::Standard,
                Speed::Standard,
                Model::Codex(CodexModel::Luna),
            ),
            instructions: Arc::from("instructions"),
            skills_catalog_present: false,
            subagent_control: self.subagents.clone(),
        };
        self.panes
            .open(pane, generation, session, agent, &self.config, lock)
            .unwrap()
    }

    /// Waits until `count` journal writers have drained, failing on any writer error.
    async fn drain_writers(&mut self, count: usize) {
        for _ in 0..count {
            let completion = self.reports.writer_completions.recv().await;
            self.panes.writer_finished(completion).unwrap();
        }
    }

    fn transcript_kinds(&self, session_id: &str) -> Vec<Option<LocalKind>> {
        self.transcript(session_id)
            .iter()
            .map(|record| record.local_kind())
            .collect()
    }

    fn transcript(&self, session_id: &str) -> Vec<Arc<TranscriptRecord>> {
        SessionStore::new(self.config.path())
            .load_transcript(session_id)
            .unwrap()
            .into_records()
    }
}

fn submitted(id: u64, text: &str) -> LocalEvent {
    LocalEvent::UserSubmitted(UserSubmitted {
        id: TurnId::new(id),
        text: text.to_owned(),
    })
}

fn closed_subagent_update(control: &Subagents, root_session_id: &str) -> ForwardedSubagentUpdate {
    ForwardedSubagentUpdate {
        runtime_id: control.runtime_id(),
        root_session_id: root_session_id.to_owned(),
        update: AgentUpdate::Status {
            id: AgentId::new(1),
            status: AgentStatus::Closed,
        },
    }
}

#[tokio::test]
async fn fork_pane_has_an_independent_session_and_persisted_transcript() {
    let mut harness = Harness::new();
    harness.open(PaneId::Main, 0, PaneSession::fresh("main-session"));
    harness.open(
        PaneId::Fork(1),
        0,
        PaneSession::fork("fork-session", "main-session", 0),
    );
    let (other_control, _other_updates) = Subagents::new(32);

    assert_eq!(
        harness
            .panes
            .subagent_pane(&closed_subagent_update(&harness.subagents, "fork-session")),
        Some(PaneId::Fork(1))
    );
    assert_eq!(
        harness
            .panes
            .subagent_pane(&closed_subagent_update(&other_control, "fork-session")),
        None
    );

    harness
        .panes
        .runtime(PaneId::Fork(1))
        .unwrap()
        .record(submitted(1, "fork-only prompt"))
        .unwrap();
    harness.panes.close_journals(None).unwrap();
    harness.drain_writers(2).await;

    let main = harness.panes.get(PaneId::Main).unwrap();
    let fork = harness.panes.get(PaneId::Fork(1)).unwrap();
    assert_eq!(main.writer_path, fork.writer_path);
    assert!(main.exit_session_id().is_none());
    assert_eq!(fork.exit_session_id().as_deref(), Some("fork-session"));
    assert!(harness.transcript("main-session").is_empty());
    assert_eq!(
        harness.transcript_kinds("fork-session"),
        [
            Some(LocalKind::SessionStarted),
            Some(LocalKind::UserSubmitted),
            Some(LocalKind::SessionEnded)
        ]
    );
    let started = harness.transcript("fork-session")[0]
        .decode_payload::<SessionStarted>()
        .unwrap();
    assert_eq!(started.parent_session_id.as_deref(), Some("main-session"));
    assert_eq!(started.parent_sequence, Some(0));
    assert_eq!(started.model, Model::Codex(CodexModel::Luna).to_string());
}

#[tokio::test]
async fn a_closed_pane_records_its_closure_and_its_successor_is_stored_only_once_used() {
    let mut harness = Harness::new();
    harness
        .open(PaneId::Main, 0, PaneSession::fresh("old-session"))
        .record(submitted(1, "old prompt"))
        .unwrap();

    harness.panes.close(PaneId::Main);
    assert!(harness.panes.has_live_agents());
    assert_eq!(
        harness
            .panes
            .agent_stream_ended(PaneId::Main, "old-session", 0)
            .unwrap(),
        StreamEnd::Removed
    );
    let new = harness.open(PaneId::Main, 1, PaneSession::fresh("new-session"));
    assert!(new.exit_session_id().is_none());
    harness.panes.close_journals(None).unwrap();
    harness.drain_writers(2).await;

    let ended = harness
        .transcript("old-session")
        .last()
        .unwrap()
        .decode_payload::<SessionEnded>()
        .unwrap();
    assert_eq!(ended.outcome, SessionOutcome::Closed);
    assert!(harness.transcript("new-session").is_empty());
    assert!(!harness.panes.has_open_writers());
}

#[tokio::test]
async fn closing_a_pane_while_a_shell_runs_removes_it_once_its_agent_stream_ends() {
    let mut harness = Harness::new();
    let workspace = harness.config.agent().workspace().to_path_buf();
    harness
        .open(PaneId::Main, 0, PaneSession::fresh("session"))
        .start_shell("make".to_owned(), &workspace)
        .unwrap();

    harness.panes.close(PaneId::Main);
    assert!(harness.panes.get(PaneId::Main).is_some());
    let stream_end = harness
        .panes
        .agent_stream_ended(PaneId::Main, "session", 0)
        .unwrap();

    assert_eq!(stream_end, StreamEnd::Removed);
    assert!(harness.panes.get(PaneId::Main).is_none());
    assert!(!harness.panes.are_drained());
    while harness.panes.subagent_shutdowns.join_next().await.is_some() {}
    assert!(harness.panes.are_drained());
    harness.drain_writers(1).await;
    assert_eq!(
        harness.transcript_kinds("session"),
        [
            Some(LocalKind::SessionStarted),
            Some(LocalKind::ShellStarted),
            Some(LocalKind::SessionEnded)
        ]
    );
}

#[tokio::test]
async fn a_stale_stream_end_leaves_the_current_session_running() {
    let mut harness = Harness::new();
    harness.open(PaneId::Main, 1, PaneSession::fresh("session"));

    let replaced = harness.panes.agent_stream_ended(PaneId::Main, "session", 0);
    let other = harness.panes.agent_stream_ended(PaneId::Main, "other", 1);

    assert_eq!(replaced.unwrap(), StreamEnd::Stale);
    assert_eq!(other.unwrap(), StreamEnd::Stale);
    assert!(harness.panes.has_live_agents());
    assert_eq!(
        harness
            .panes
            .agent_stream_ended(PaneId::Main, "session", 1)
            .unwrap(),
        StreamEnd::Stopped
    );
    assert!(harness.panes.get(PaneId::Main).is_some());
    assert!(!harness.panes.has_live_agents());
}

#[tokio::test]
async fn a_submission_waits_for_running_shells_and_carries_their_results() {
    let mut harness = Harness::new();
    let workspace = harness.config.agent().workspace().to_path_buf();
    harness.open(PaneId::Main, 0, PaneSession::fresh("session"));
    let runtime = harness.panes.runtime(PaneId::Main).unwrap();
    let (shell, _) = runtime.start_shell("make".to_owned(), &workspace).unwrap();
    let id = runtime.next_turn_id();
    let submission = PendingSubmission {
        id,
        prompt: "explain it".to_owned().into(),
    };

    runtime.submit(submission, &harness.worker).unwrap();
    assert!(harness.commands.try_recv().is_err());

    let execution = ShellExecution {
        id: shell,
        command: "make".to_owned(),
        output: "done".to_owned(),
        exit_code: Some(0),
        duration_ns: 1,
        truncated: false,
        error: None,
    };
    let context = execution.model_context();
    let runtime = harness.panes.runtime(PaneId::Main).unwrap();
    let (_, held) = runtime.finish_shell(execution).unwrap();
    let held = held.expect("the last shell releases the held submission");
    runtime.send_submission(held, &harness.worker).unwrap();

    let Ok(WorkerCommand::Submit {
        pane,
        id: sent,
        prompt,
    }) = harness.commands.try_recv()
    else {
        panic!("the held submission should reach the worker");
    };
    assert_eq!(pane, PaneId::Main);
    assert_eq!(sent, id);
    assert_eq!(prompt.display_text(), format!("{context}\n\nexplain it"));

    let runtime = harness.panes.runtime(PaneId::Main).unwrap();
    let next = PendingSubmission {
        id: runtime.next_turn_id(),
        prompt: "again".to_owned().into(),
    };
    runtime.submit(next, &harness.worker).unwrap();
    let Ok(WorkerCommand::Submit { prompt, .. }) = harness.commands.try_recv() else {
        panic!("a submission without running shells is sent at once");
    };
    assert_eq!(prompt.display_text(), "again");
}
