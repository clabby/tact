use super::{TranscriptError, TranscriptRecord};
use crate::tui::{
    context::outbound_context_snapshot,
    storage::{SessionStorage, database_path},
    transcript::{LocalEvent, SessionStarted},
};
use nanocodex::agent::events::AgentEvent;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{sync::mpsc, task::JoinHandle};

pub(crate) struct TranscriptJournal {
    path: PathBuf,
    sender: mpsc::UnboundedSender<PendingWrite>,
    persisted: Arc<AtomicBool>,
    pending_start: Option<SessionStarted>,
    next_sequence: u64,
}

pub(crate) struct TranscriptWriter {
    task: JoinHandle<Result<(), TranscriptError>>,
}

struct PendingWrite {
    record: Arc<TranscriptRecord>,
    resume_state: Option<Vec<u8>>,
}

impl TranscriptJournal {
    pub(crate) fn open(
        config_path: &Path,
        session_id: &str,
    ) -> Result<(Self, TranscriptWriter), TranscriptError> {
        let path = database_path(config_path);
        let (sender, receiver) = mpsc::unbounded_channel();
        let persisted = Arc::new(AtomicBool::new(false));
        let writer_config = config_path.to_path_buf();
        let writer_session = session_id.to_owned();
        let writer_persisted = Arc::clone(&persisted);
        let task = tokio::task::spawn_blocking(move || {
            write_journal(receiver, &writer_config, &writer_session, &writer_persisted)
        });
        Ok((
            Self {
                path,
                sender,
                persisted,
                pending_start: None,
                next_sequence: 1,
            },
            TranscriptWriter { task },
        ))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn persistence_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.persisted)
    }

    pub(crate) fn defer_start(&mut self, started: SessionStarted) {
        self.pending_start = Some(started);
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.next_sequence == 1
    }

    pub(crate) fn set_initial_effort(&mut self, effort: crate::app::config::ReasoningEffort) {
        if let Some(started) = &mut self.pending_start {
            started.effort = effort;
        }
    }

    pub(crate) fn set_initial_fast_mode(&mut self, enabled: bool) {
        if let Some(started) = &mut self.pending_start {
            started.fast_mode = enabled;
        }
    }

    /// Returns the live event record while persisting only its resume-relevant representation.
    ///
    /// Raw API transport events repeat complete requests, response snapshots, and streaming
    /// payloads already represented by normalized agent events. They remain available to the live
    /// diagnostics reducer, but V2 stores only the content-free outbound context facts it needs.
    pub(crate) fn append_agent(
        &mut self,
        event: AgentEvent,
    ) -> Result<Arc<TranscriptRecord>, TranscriptError> {
        self.start_if_needed()?;
        let record = TranscriptRecord::from_agent(self.next_sequence, unix_milliseconds(), event);
        self.next_sequence = self.next_sequence.saturating_add(1);
        if record.kind() == "api.event" {
            if let Some((prompt_cache, previous_response)) = outbound_context_snapshot(&record) {
                self.append_local_record(LocalEvent::ContextObserved {
                    prompt_cache,
                    previous_response,
                })?;
            }
            return Ok(Arc::new(record));
        }
        self.send(record, None)
    }

    pub(crate) fn append_local(
        &mut self,
        event: LocalEvent,
    ) -> Result<Arc<TranscriptRecord>, TranscriptError> {
        self.start_if_needed()?;
        self.append_local_record(event)
    }

    pub(crate) fn append_local_with_resume_state(
        &mut self,
        event: LocalEvent,
        resume_state: Vec<u8>,
    ) -> Result<Arc<TranscriptRecord>, TranscriptError> {
        self.start_if_needed()?;
        let record = self.local_record(event)?;
        self.send(record, Some(resume_state))
    }

    fn start_if_needed(&mut self) -> Result<(), TranscriptError> {
        let Some(started) = self.pending_start.take() else {
            return Ok(());
        };
        self.append_local_record(LocalEvent::SessionStarted(started))?;
        Ok(())
    }

    fn append_local_record(
        &mut self,
        event: LocalEvent,
    ) -> Result<Arc<TranscriptRecord>, TranscriptError> {
        let record = self.local_record(event)?;
        self.send(record, None)
    }

    fn local_record(&mut self, event: LocalEvent) -> Result<TranscriptRecord, TranscriptError> {
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        TranscriptRecord::from_local(sequence, unix_milliseconds(), event).map_err(|source| {
            TranscriptError::Encode {
                path: self.path.clone(),
                source,
            }
        })
    }

    fn send(
        &self,
        record: TranscriptRecord,
        resume_state: Option<Vec<u8>>,
    ) -> Result<Arc<TranscriptRecord>, TranscriptError> {
        let record = Arc::new(record);
        self.sender
            .send(PendingWrite {
                record: Arc::clone(&record),
                resume_state,
            })
            .map_err(|_| TranscriptError::WriterStopped(self.path.clone()))?;
        Ok(record)
    }
}

impl TranscriptWriter {
    pub(crate) fn into_task(self) -> JoinHandle<Result<(), TranscriptError>> {
        self.task
    }
}

fn write_journal(
    mut receiver: mpsc::UnboundedReceiver<PendingWrite>,
    config_path: &Path,
    session_id: &str,
    persisted: &AtomicBool,
) -> Result<(), TranscriptError> {
    let Some(first) = receiver.blocking_recv() else {
        return Ok(());
    };
    let mut storage = SessionStorage::open(config_path)?;
    let mut batch = vec![first];
    loop {
        while let Ok(record) = receiver.try_recv() {
            batch.push(record);
        }
        let records = batch
            .iter()
            .map(|pending| Arc::clone(&pending.record))
            .collect::<Vec<_>>();
        let resume_state = batch
            .iter()
            .rev()
            .find_map(|pending| pending.resume_state.as_deref());
        if let Some(resume_state) = resume_state {
            storage.append_records_and_resume_state(session_id, &records, Some(resume_state))?;
        } else {
            storage.append_records(session_id, &records)?;
        }
        persisted.store(true, Ordering::Release);
        batch.clear();
        let Some(record) = receiver.blocking_recv() else {
            return Ok(());
        };
        batch.push(record);
    }
}

fn unix_milliseconds() -> u64 {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::TranscriptJournal;
    use crate::{
        app::config::{ReasoningEffort, ReasoningMode},
        tui::{
            session,
            storage::SessionStorage,
            transcript::{LocalEvent, SessionStarted, TurnId},
        },
    };
    use nanocodex::agent::events::{AgentEvent, AgentEventKind};
    use serde_json::{json, value::to_raw_value};
    use std::sync::Arc;
    use tempfile::tempdir;

    fn started(session_id: &str) -> SessionStarted {
        SessionStarted {
            session_id: session_id.to_owned(),
            parent_session_id: None,
            model: "model".to_owned(),
            effort: ReasoningEffort::Medium,
            reasoning_mode: ReasoningMode::Standard,
            fast_mode: false,
            workspace: "/work".into(),
            application_version: "test".to_owned(),
        }
    }

    #[tokio::test]
    async fn semantic_records_round_trip_in_order() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let (mut journal, writer) = TranscriptJournal::open(&config, "session").unwrap();
        journal.defer_start(started("session"));
        journal
            .append_local(LocalEvent::UserSubmitted {
                id: TurnId::new(1),
                text: "hello".to_owned(),
            })
            .unwrap();
        drop(journal);
        writer.into_task().await.unwrap().unwrap();

        let records = session::load_transcript(&config, "session").unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].kind(), "session.started");
        assert_eq!(records[1].kind(), "user.submitted");
    }

    #[tokio::test]
    async fn raw_api_events_are_not_persisted() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let (mut journal, writer) = TranscriptJournal::open(&config, "session").unwrap();
        journal.defer_start(started("session"));
        let marker = "must-not-be-durable";
        let live = journal
            .append_agent(AgentEvent {
                protocol_version: 1,
                request_id: Arc::from("request"),
                seq: 1,
                kind: AgentEventKind::ApiEvent,
                payload: to_raw_value(&json!({
                    "direction": "outbound", "phase": "generation",
                    "event": {"prompt_cache_key": marker, "previous_response_id": marker}
                }))
                .unwrap()
                .into(),
            })
            .unwrap();
        assert_eq!(live.kind(), "api.event");
        drop(journal);
        writer.into_task().await.unwrap().unwrap();

        let records = SessionStorage::open(&config)
            .unwrap()
            .load_records("session")
            .unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[1].kind(), "context.observed");
        assert!(!format!("{records:?}").contains(marker));
    }

    #[tokio::test]
    async fn completed_assistant_message_replaces_its_persisted_deltas() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let (mut journal, writer) = TranscriptJournal::open(&config, "session").unwrap();
        journal.defer_start(started("session"));
        for (sequence, text) in [(1, "first "), (2, "second")] {
            journal
                .append_agent(AgentEvent {
                    protocol_version: 1,
                    request_id: Arc::from("request"),
                    seq: sequence,
                    kind: AgentEventKind::AssistantDelta,
                    payload: to_raw_value(&json!({
                        "model_call_index": 0,
                        "item_id": "message",
                        "phase": "final_answer",
                        "text": text,
                    }))
                    .unwrap()
                    .into(),
                })
                .unwrap();
        }
        journal
            .append_agent(AgentEvent {
                protocol_version: 1,
                request_id: Arc::from("request"),
                seq: 3,
                kind: AgentEventKind::AssistantMessage,
                payload: to_raw_value(&json!({
                    "model_call_index": 0,
                    "item_id": "message",
                    "phase": "final_answer",
                    "text": "first second",
                }))
                .unwrap()
                .into(),
            })
            .unwrap();
        drop(journal);
        writer.into_task().await.unwrap().unwrap();

        let records = session::load_transcript(&config, "session").unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].kind(), "session.started");
        assert_eq!(records[1].kind(), "assistant.message");
    }

    #[tokio::test]
    async fn completed_turn_does_not_remove_an_earlier_failed_turn_draft() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let (mut journal, writer) = TranscriptJournal::open(&config, "session").unwrap();
        journal.defer_start(started("session"));
        journal
            .append_local(LocalEvent::WorkerTurnAccepted { id: TurnId::new(1) })
            .unwrap();
        append_assistant(
            &mut journal,
            AgentEventKind::AssistantDelta,
            1,
            "failed draft",
        );
        journal
            .append_local(LocalEvent::WorkerTurnFinished {
                id: TurnId::new(1),
                error: Some("failed".to_owned()),
            })
            .unwrap();
        journal
            .append_local(LocalEvent::WorkerTurnAccepted { id: TurnId::new(2) })
            .unwrap();
        append_assistant(&mut journal, AgentEventKind::AssistantDelta, 2, "complete");
        append_assistant(
            &mut journal,
            AgentEventKind::AssistantMessage,
            3,
            "complete",
        );
        drop(journal);
        writer.into_task().await.unwrap().unwrap();

        let records = session::load_transcript(&config, "session").unwrap();
        assert_eq!(
            records
                .iter()
                .filter(|record| record.kind() == "assistant.delta")
                .count(),
            1
        );
        assert_eq!(
            records
                .iter()
                .filter(|record| record.kind() == "assistant.message")
                .count(),
            1
        );
    }

    fn append_assistant(
        journal: &mut TranscriptJournal,
        kind: AgentEventKind,
        sequence: u64,
        text: &str,
    ) {
        journal
            .append_agent(AgentEvent {
                protocol_version: 1,
                request_id: Arc::from("request"),
                seq: sequence,
                kind,
                payload: to_raw_value(&json!({
                    "model_call_index": 0,
                    "item_id": "message",
                    "phase": "final_answer",
                    "text": text,
                }))
                .unwrap()
                .into(),
            })
            .unwrap();
    }

    #[tokio::test]
    async fn an_empty_journal_creates_no_session() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let (journal, writer) = TranscriptJournal::open(&config, "empty").unwrap();
        drop(journal);
        writer.into_task().await.unwrap().unwrap();
        assert!(!directory.path().join("sessions").exists());
    }

    #[tokio::test]
    async fn recent_prompts_are_indexed_in_reverse_order_without_changing_whitespace() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        for (session_id, prompt) in [
            ("first", "  preserve\n  this spacing  "),
            ("second", "newest prompt"),
        ] {
            let (mut journal, writer) = TranscriptJournal::open(&config, session_id).unwrap();
            journal.defer_start(started(session_id));
            journal
                .append_local(LocalEvent::UserSubmitted {
                    id: TurnId::new(1),
                    text: prompt.to_owned(),
                })
                .unwrap();
            drop(journal);
            writer.into_task().await.unwrap().unwrap();
        }

        let prompts = session::load_recent_prompts_async(config).await.unwrap();
        assert_eq!(prompts.len(), 2);
        assert_eq!(prompts[0].text, "newest prompt");
        assert_eq!(prompts[1].text, "  preserve\n  this spacing  ");
        assert_eq!(prompts[1].session_id, "first");
    }
}
