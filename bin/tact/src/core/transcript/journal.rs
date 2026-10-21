//! Ordered, append-only persistence for one live session's transcript.
//!
//! [`TranscriptJournal`] belongs to the session's event loop. It assigns every record its sequence, holds the
//! session's start record back until the first other record (so an untouched session leaves
//! nothing on disk), and hands records to a dedicated blocking writer that commits everything
//! queued since its previous commit through the session's durable owner, in one SQLite
//! transaction. Appends never wait for disk; callers that need durability await
//! [`TranscriptJournal::flush`].
//!
//! Raw API transport events are returned for live diagnostics but never persisted. Only the
//! content-free context facts derived from outbound requests reach storage.
//!
//! The store waits out other connections' write locks. Any other persistence failure stops the
//! writer, and the journal reports that failure from every later append.

use super::{EffortChanged, LocalKind, SpeedChanged, TranscriptError, TranscriptRecord};
use crate::{
    app::{
        config::{ReasoningEffort, Speed},
        model,
    },
    core::{
        context::outbound_context_snapshot,
        storage::{HostBatch, SessionStorage, SqliteStateStore, database_path, transcript_commit},
        transcript::{LocalEvent, SessionStarted},
    },
};
use nanocode::{
    BackendId, Lineage, ModelSelection, Origin,
    agent::events::{AgentEvent, AgentEventKind},
    durability::{
        DurableSession, MetadataWrite, SessionRecord, SessionStore as SessionCatalog, SessionWrite,
    },
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{
    runtime::Handle,
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

/// Metadata keys of the Tact settings recorded on a durable session.
const EFFORT_KEY: &str = "tact.effort";
const REASONING_MODE_KEY: &str = "tact.reasoning_mode";
const SPEED_KEY: &str = "tact.speed";
const APPLICATION_VERSION_KEY: &str = "tact.application_version";

/// The durable store of one session database, shared by every journal in a process.
#[derive(Clone)]
pub(crate) struct DurableTranscripts {
    config_path: Arc<Path>,
    store: SqliteStateStore,
}

impl DurableTranscripts {
    pub(crate) fn new(config_path: &Path) -> Result<Self, TranscriptError> {
        Ok(Self {
            config_path: config_path.into(),
            store: SqliteStateStore::new(config_path)?,
        })
    }
}

pub(crate) struct TranscriptJournal {
    path: PathBuf,
    /// Unbounded so the event loop never blocks on disk; the writer drains the whole queue into
    /// each batch.
    sender: mpsc::UnboundedSender<JournalCommand>,
    /// Set once the writer has committed at least one batch.
    persisted: Arc<AtomicBool>,
    /// The writer's terminal error, kept so later appends can report why it stopped.
    failure: Arc<OnceLock<String>>,
    pending_start: Option<SessionStarted>,
    next_sequence: u64,
    last_sequence: u64,
    empty: bool,
}

/// The writer task, which finishes after every journal handle is dropped and its queue drained.
pub(crate) struct TranscriptWriter {
    task: JoinHandle<Result<(), TranscriptError>>,
}

struct PendingWrite {
    record: Arc<TranscriptRecord>,
    resume_state: Option<Vec<u8>>,
}

enum JournalCommand {
    Write(PendingWrite),
    Flush(oneshot::Sender<()>),
}

impl TranscriptJournal {
    #[cfg(test)]
    pub(crate) fn open(
        config_path: &Path,
        session_id: &str,
    ) -> Result<(Self, TranscriptWriter), TranscriptError> {
        Self::open_at(config_path, session_id, 1)
    }

    #[cfg(test)]
    pub(crate) fn open_at(
        config_path: &Path,
        session_id: &str,
        next_sequence: u64,
    ) -> Result<(Self, TranscriptWriter), TranscriptError> {
        Self::open_in(
            &DurableTranscripts::new(config_path)?,
            session_id,
            next_sequence,
        )
    }

    pub(crate) fn open_in(
        transcripts: &DurableTranscripts,
        session_id: &str,
        next_sequence: u64,
    ) -> Result<(Self, TranscriptWriter), TranscriptError> {
        let path = database_path(&transcripts.config_path);
        let (sender, receiver) = mpsc::unbounded_channel();
        let persisted = Arc::new(AtomicBool::new(false));
        let failure = Arc::new(OnceLock::new());
        let store = transcripts.store.writer_handle();
        let catalog =
            SessionCatalog::new(store.clone()).map_err(|source| TranscriptError::Durability {
                path: path.clone(),
                source,
            })?;
        let mut writer = JournalWriter {
            path: path.clone(),
            config_path: Arc::clone(&transcripts.config_path),
            store,
            catalog,
            runtime: Handle::current(),
            session_id: session_id.to_owned(),
            session: None,
        };
        let writer_persisted = Arc::clone(&persisted);
        let writer_failure = Arc::clone(&failure);
        let task = tokio::task::spawn_blocking(move || {
            writer.run(receiver, &writer_persisted, &writer_failure)
        });
        Ok((
            Self {
                path,
                sender,
                persisted,
                failure,
                pending_start: None,
                next_sequence,
                last_sequence: next_sequence.saturating_sub(1),
                empty: true,
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
        self.empty
    }

    pub(crate) const fn last_sequence(&self) -> u64 {
        self.last_sequence
    }

    pub(crate) async fn flush(&self) -> Result<(), TranscriptError> {
        let (completion, completed) = oneshot::channel();
        self.sender
            .send(JournalCommand::Flush(completion))
            .map_err(|_| self.writer_stopped())?;
        completed.await.map_err(|_| self.writer_stopped())
    }

    pub(crate) async fn persist_start(
        &mut self,
    ) -> Result<Option<Arc<TranscriptRecord>>, TranscriptError> {
        let record = self.start_if_needed()?;
        self.flush().await?;
        Ok(record)
    }

    pub(crate) fn set_initial_effort(&mut self, effort: ReasoningEffort) {
        if let Some(started) = &mut self.pending_start {
            started.effort = effort;
        }
    }

    pub(crate) fn set_initial_speed(&mut self, speed: Speed) {
        if let Some(started) = &mut self.pending_start {
            started.speed = speed;
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
        drop(self.start_if_needed()?);
        let record = TranscriptRecord::from_agent(self.next_sequence, unix_milliseconds(), event);
        self.next_sequence = self.next_sequence.saturating_add(1);
        if record.agent_kind() == Some(AgentEventKind::ApiEvent) {
            if let Some(observed) = outbound_context_snapshot(&record) {
                self.append_local_record(LocalEvent::ContextObserved(observed))?;
            }
            return Ok(Arc::new(record));
        }
        self.send(record, None)
    }

    pub(crate) fn append_local(
        &mut self,
        event: LocalEvent,
    ) -> Result<Arc<TranscriptRecord>, TranscriptError> {
        drop(self.start_if_needed()?);
        self.append_local_record(event)
    }

    pub(crate) fn append_local_with_resume_state(
        &mut self,
        event: LocalEvent,
        resume_state: Vec<u8>,
    ) -> Result<Arc<TranscriptRecord>, TranscriptError> {
        drop(self.start_if_needed()?);
        let record = self.local_record(event)?;
        self.send(record, Some(resume_state))
    }

    fn start_if_needed(&mut self) -> Result<Option<Arc<TranscriptRecord>>, TranscriptError> {
        let Some(started) = self.pending_start.take() else {
            return Ok(None);
        };
        self.append_local_record(LocalEvent::SessionStarted(started))
            .map(Some)
    }

    fn append_local_record(
        &mut self,
        event: LocalEvent,
    ) -> Result<Arc<TranscriptRecord>, TranscriptError> {
        let record = self.local_record(event)?;
        self.send(record, None)
    }

    fn local_record(&mut self, event: LocalEvent) -> Result<TranscriptRecord, TranscriptError> {
        self.empty = false;
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
        &mut self,
        record: TranscriptRecord,
        resume_state: Option<Vec<u8>>,
    ) -> Result<Arc<TranscriptRecord>, TranscriptError> {
        let record = Arc::new(record);
        self.sender
            .send(JournalCommand::Write(PendingWrite {
                record: Arc::clone(&record),
                resume_state,
            }))
            .map_err(|_| self.writer_stopped())?;
        self.last_sequence = record.sequence();
        Ok(record)
    }

    fn writer_stopped(&self) -> TranscriptError {
        self.failure.get().map_or_else(
            || TranscriptError::WriterStopped(self.path.clone()),
            |reason| TranscriptError::WriterFailed {
                path: self.path.clone(),
                reason: reason.clone(),
            },
        )
    }
}

impl TranscriptWriter {
    pub(crate) fn into_task(self) -> JoinHandle<Result<(), TranscriptError>> {
        self.task
    }
}

/// The blocking side of a journal: it batches queued records and commits each batch through
/// the session's durable owner, which it acquires with the first batch.
struct JournalWriter {
    path: PathBuf,
    config_path: Arc<Path>,
    /// This writer's own store handle, through which it stages its host batches.
    store: SqliteStateStore,
    catalog: SessionCatalog,
    /// The runtime that runs the durable session's driver; the writer blocks on it from its own
    /// blocking thread.
    runtime: Handle,
    session_id: String,
    session: Option<DurableSession>,
}

impl JournalWriter {
    fn run(
        &mut self,
        mut receiver: mpsc::UnboundedReceiver<JournalCommand>,
        persisted: &AtomicBool,
        failure: &OnceLock<String>,
    ) -> Result<(), TranscriptError> {
        while let Some(command) = receiver.blocking_recv() {
            let mut batch = Vec::new();
            let mut completions = Vec::new();
            for command in
                std::iter::once(command).chain(std::iter::from_fn(|| receiver.try_recv().ok()))
            {
                match command {
                    JournalCommand::Write(record) => batch.push(record),
                    JournalCommand::Flush(completion) => completions.push(completion),
                }
            }
            let wrote = !batch.is_empty();
            if let Err(error) = self.write_batch(batch) {
                self.store.discard(&self.session_id);
                let _ = failure.set(error.to_string());
                return Err(error);
            }
            if wrote {
                persisted.store(true, Ordering::Release);
            }
            for completion in completions {
                let _ = completion.send(());
            }
        }
        Ok(())
    }

    fn write_batch(&mut self, batch: Vec<PendingWrite>) -> Result<(), TranscriptError> {
        if batch.is_empty() {
            return Ok(());
        }
        let encode = |source| TranscriptError::Encode {
            path: self.path.clone(),
            source,
        };
        let (events, rows) = transcript_commit(batch.iter().map(|pending| pending.record.as_ref()))
            .map_err(encode)?;
        let mut settings = BTreeMap::new();
        for record in batch.iter().map(|pending| pending.record.as_ref()) {
            if let Some((key, value)) = setting_change(record).map_err(encode)? {
                settings.insert(key.to_owned(), Some(value));
            }
        }
        let started = batch
            .iter()
            .find_map(|pending| pending.record.session_started());
        let resume_state = batch
            .into_iter()
            .rev()
            .find_map(|pending| pending.resume_state);
        let session = match self.session.take() {
            Some(session) => session,
            None => self.open_session(started.as_ref())?,
        };
        let session = self.session.insert(session);
        let metadata =
            (!settings.is_empty() && session.described_record().is_some()).then(|| MetadataWrite {
                values: settings,
                ..MetadataWrite::default()
            });
        self.store
            .stage(&self.session_id, HostBatch { rows, resume_state })?;
        self.runtime
            .block_on(session.append(SessionWrite { events, metadata }))
            .map_err(|source| self.durability(source))?;
        Ok(())
    }

    /// Acquires the session. A journal that begins with a session start describes the session
    /// from it; one that continues a stored session without a new start keeps its stored record.
    fn open_session(
        &self,
        started: Option<&SessionStarted>,
    ) -> Result<DurableSession, TranscriptError> {
        let opened = match started {
            Some(started) => {
                let record = self.session_record(started)?;
                self.runtime.block_on(self.catalog.session(record))
            }
            None => self.runtime.block_on(DurableSession::open(
                self.store.clone(),
                self.session_id.clone(),
            )),
        };
        opened.map_err(|source| self.durability(source))
    }

    fn session_record(&self, started: &SessionStarted) -> Result<SessionRecord, TranscriptError> {
        let model = model::parse(&started.model).map_or_else(
            |_| ModelSelection {
                backend: BackendId::new("unknown"),
                model: started.model.clone(),
            },
            |model| model.selection(),
        );
        let lineage = match &started.parent_session_id {
            None => Lineage::root(&started.session_id),
            Some(parent) => {
                let (root, depth) = match SessionStorage::open_read_only(&self.config_path)? {
                    Some(storage) => storage.lineage_root(parent)?,
                    None => (parent.clone(), 0),
                };
                Lineage::new(
                    root,
                    Some(parent.clone()),
                    Origin::Fork,
                    depth.saturating_add(1),
                )
            }
        };
        let encode = |source| TranscriptError::Encode {
            path: self.path.clone(),
            source,
        };
        let mut record =
            SessionRecord::root(&started.session_id, model, Some(started.workspace.clone()))
                .with_lineage(lineage);
        record.metadata = BTreeMap::from([
            (
                EFFORT_KEY.to_owned(),
                serde_json::to_value(started.effort).map_err(encode)?,
            ),
            (
                REASONING_MODE_KEY.to_owned(),
                serde_json::to_value(started.reasoning_mode).map_err(encode)?,
            ),
            (
                SPEED_KEY.to_owned(),
                serde_json::to_value(started.speed).map_err(encode)?,
            ),
            (
                APPLICATION_VERSION_KEY.to_owned(),
                started.application_version.clone().into(),
            ),
        ]);
        Ok(record)
    }

    fn durability(&self, source: nanocode::durability::Error) -> TranscriptError {
        TranscriptError::Durability {
            path: self.path.clone(),
            source,
        }
    }
}

/// The session setting a record changes, as the metadata key and value it records.
fn setting_change(
    record: &TranscriptRecord,
) -> Result<Option<(&'static str, serde_json::Value)>, serde_json::Error> {
    Ok(match record.local_kind() {
        Some(LocalKind::EffortChanged) => Some((
            EFFORT_KEY,
            serde_json::to_value(record.decode_payload::<EffortChanged>()?.to)?,
        )),
        Some(LocalKind::SpeedChanged) => Some((
            SPEED_KEY,
            serde_json::to_value(record.decode_payload::<SpeedChanged>()?.to)?,
        )),
        _ => None,
    })
}

fn unix_milliseconds() -> u64 {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::{DurableTranscripts, TranscriptJournal};
    use crate::{
        app::config::{ReasoningEffort, ReasoningMode, Speed},
        core::{
            session::SessionStore,
            storage::{SessionStorage, SqliteStateReader, database_path},
            transcript::{
                EffortChanged, LocalEvent, LocalKind, SessionStarted, SpeedChanged,
                TranscriptError, TranscriptRecord, TurnId, UserSubmitted, WorkerTurnAccepted,
                WorkerTurnFinished,
            },
        },
    };
    use nanocode::{
        agent::events::{AgentEvent, AgentEventKind},
        durability::{Error as DurabilityError, StateQuery, StateReader, StoreError},
    };
    use rusqlite::Connection;
    use serde_json::{Value, json, value::to_raw_value};
    use std::{path::Path, sync::Arc};
    use tempfile::tempdir;

    fn started(session_id: &str) -> SessionStarted {
        SessionStarted {
            session_id: session_id.to_owned(),
            parent_session_id: None,
            parent_sequence: None,
            model: "model".to_owned(),
            effort: ReasoningEffort::Medium,
            reasoning_mode: ReasoningMode::Standard,
            speed: Speed::Standard,
            workspace: "/work".into(),
            application_version: "test".to_owned(),
        }
    }

    #[tokio::test]
    async fn initial_speed_and_changes_round_trip_without_overwriting_the_start() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let (mut journal, writer) = TranscriptJournal::open(&config, "session").unwrap();
        journal.defer_start(started("session"));
        journal.set_initial_speed(Speed::Ultrafast);
        journal
            .append_local(LocalEvent::UserSubmitted(UserSubmitted {
                id: TurnId::new(1),
                text: "hello".to_owned(),
            }))
            .unwrap();
        journal.set_initial_speed(Speed::Fast);
        journal
            .append_local(LocalEvent::SpeedChanged(SpeedChanged {
                from: Speed::Ultrafast,
                to: Speed::Fast,
            }))
            .unwrap();
        drop(journal);
        writer.into_task().await.unwrap().unwrap();

        let records = SessionStore::new(&config)
            .load_transcript("session")
            .unwrap()
            .into_records();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].local_kind(), Some(LocalKind::SessionStarted));
        assert_eq!(
            records[0].decode_payload::<SessionStarted>().unwrap().speed,
            Speed::Ultrafast
        );
        assert_eq!(records[1].local_kind(), Some(LocalKind::UserSubmitted));
        assert_eq!(records[2].local_kind(), Some(LocalKind::SpeedChanged));
        assert_eq!(
            records[2].decode_payload::<Value>().unwrap(),
            json!({"from": "ultrafast", "to": "fast"})
        );
    }

    #[tokio::test]
    async fn persist_start_makes_an_untouched_fork_available_to_descendants() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let (mut journal, writer) = TranscriptJournal::open(&config, "fork").unwrap();
        let mut fork = started("fork");
        fork.parent_session_id = Some("parent".to_owned());
        fork.parent_sequence = Some(0);
        journal.defer_start(fork);

        let record = journal.persist_start().await.unwrap().unwrap();

        assert_eq!(record.sequence(), 1);
        assert_eq!(journal.last_sequence(), 1);
        let records = SessionStorage::open(&config)
            .unwrap()
            .load_records("fork")
            .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].local_kind(), Some(LocalKind::SessionStarted));
        drop(journal);
        writer.into_task().await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn untouched_nested_forks_restore_every_lineage_edge() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let (mut parent, parent_writer) = TranscriptJournal::open(&config, "parent").unwrap();
        parent.defer_start(started("parent"));
        parent
            .append_local(LocalEvent::UserSubmitted(UserSubmitted {
                id: TurnId::new(1),
                text: "parent prompt".to_owned(),
            }))
            .unwrap();
        parent.flush().await.unwrap();

        let (mut fork, fork_writer) = TranscriptJournal::open(&config, "fork").unwrap();
        let mut fork_start = started("fork");
        fork_start.parent_session_id = Some("parent".to_owned());
        fork_start.parent_sequence = Some(parent.last_sequence());
        fork.defer_start(fork_start);
        fork.persist_start().await.unwrap();

        let (mut grandchild, grandchild_writer) =
            TranscriptJournal::open(&config, "grandchild").unwrap();
        let mut grandchild_start = started("grandchild");
        grandchild_start.parent_session_id = Some("fork".to_owned());
        grandchild_start.parent_sequence = Some(fork.last_sequence());
        grandchild.defer_start(grandchild_start);
        grandchild
            .append_local(LocalEvent::UserSubmitted(UserSubmitted {
                id: TurnId::new(1),
                text: "grandchild prompt".to_owned(),
            }))
            .unwrap();
        grandchild.flush().await.unwrap();

        let records = SessionStore::new(&config)
            .load_transcript("grandchild")
            .unwrap()
            .into_records();
        assert_eq!(
            records
                .iter()
                .filter(|record| record.local_kind() == Some(LocalKind::SessionStarted))
                .count(),
            3
        );
        assert_eq!(
            records
                .iter()
                .filter(|record| record.local_kind() == Some(LocalKind::UserSubmitted))
                .count(),
            2
        );

        drop(parent);
        drop(fork);
        drop(grandchild);
        parent_writer.into_task().await.unwrap().unwrap();
        fork_writer.into_task().await.unwrap().unwrap();
        grandchild_writer.into_task().await.unwrap().unwrap();
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
        assert_eq!(live.agent_kind(), Some(AgentEventKind::ApiEvent));
        drop(journal);
        writer.into_task().await.unwrap().unwrap();

        let records = SessionStorage::open(&config)
            .unwrap()
            .load_records("session")
            .unwrap();
        let kinds = records
            .iter()
            .map(|record| record.local_kind())
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            [
                Some(LocalKind::SessionStarted),
                Some(LocalKind::ContextObserved)
            ]
        );
        assert_eq!(
            records[1].decode_payload::<Value>().unwrap(),
            json!({"prompt_cache": true, "previous_response": true})
        );
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

        let records = SessionStore::new(&config)
            .load_transcript("session")
            .unwrap()
            .into_records();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].local_kind(), Some(LocalKind::SessionStarted));
        assert_eq!(
            records[1].agent_kind(),
            Some(AgentEventKind::AssistantMessage)
        );
    }

    #[tokio::test]
    async fn completed_turn_does_not_remove_an_earlier_failed_turn_draft() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let (mut journal, writer) = TranscriptJournal::open(&config, "session").unwrap();
        journal.defer_start(started("session"));
        journal
            .append_local(LocalEvent::WorkerTurnAccepted(WorkerTurnAccepted {
                id: TurnId::new(1),
            }))
            .unwrap();
        append_assistant(
            &mut journal,
            AgentEventKind::AssistantDelta,
            1,
            "failed draft",
        );
        journal
            .append_local(LocalEvent::WorkerTurnFinished(WorkerTurnFinished {
                id: TurnId::new(1),
                error: Some("failed".to_owned()),
                terminal_stop: None,
            }))
            .unwrap();
        journal
            .append_local(LocalEvent::WorkerTurnAccepted(WorkerTurnAccepted {
                id: TurnId::new(2),
            }))
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

        let records = SessionStore::new(&config)
            .load_transcript("session")
            .unwrap()
            .into_records();
        assert_eq!(
            records
                .iter()
                .filter(|record| record.agent_kind() == Some(AgentEventKind::AssistantDelta))
                .count(),
            1
        );
        assert_eq!(
            records
                .iter()
                .filter(|record| record.agent_kind() == Some(AgentEventKind::AssistantMessage))
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
    async fn stopped_writer_reports_its_persistence_error_to_later_appends() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        drop(SessionStorage::open(&config).unwrap());
        let connection = Connection::open(database_path(&config)).unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER reject_sessions BEFORE INSERT ON sessions BEGIN
                     SELECT RAISE(ABORT, 'forced persistence failure');
                 END;",
            )
            .unwrap();

        let (mut journal, writer) = TranscriptJournal::open(&config, "session").unwrap();
        journal.defer_start(started("session"));
        // Appending sends the deferred session start and then the prompt. The writer can reject
        // the start before the prompt is sent, so this append may already report the failure.
        let _ = journal.append_local(LocalEvent::UserSubmitted(UserSubmitted {
            id: TurnId::new(1),
            text: "first".to_owned(),
        }));
        let writer_error = writer.into_task().await.unwrap().unwrap_err();
        assert!(matches!(
            &writer_error,
            TranscriptError::Durability { source, .. }
                if source.to_string().contains("forced persistence failure")
        ));

        let append_error = journal
            .append_local(LocalEvent::UserSubmitted(UserSubmitted {
                id: TurnId::new(2),
                text: "second".to_owned(),
            }))
            .unwrap_err();

        assert!(matches!(
            append_error,
            TranscriptError::WriterFailed { reason, .. } if reason == writer_error.to_string()
        ));
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
                .append_local(LocalEvent::UserSubmitted(UserSubmitted {
                    id: TurnId::new(1),
                    text: prompt.to_owned(),
                }))
                .unwrap();
            drop(journal);
            writer.into_task().await.unwrap().unwrap();
        }

        let prompts = SessionStore::new(&config)
            .run_blocking(SessionStore::recent_prompts)
            .await
            .unwrap();
        assert_eq!(prompts.len(), 2);
        assert_eq!(prompts[0].text, "newest prompt");
        assert_eq!(prompts[1].text, "  preserve\n  this spacing  ");
        assert_eq!(prompts[1].session_id, "first");
    }

    fn prompt(id: u64, text: &str) -> LocalEvent {
        LocalEvent::UserSubmitted(UserSubmitted {
            id: TurnId::new(id),
            text: text.to_owned(),
        })
    }

    fn prompts(config: &Path, session_id: &str) -> Vec<String> {
        SessionStore::new(config)
            .load_transcript(session_id)
            .unwrap()
            .into_records()
            .iter()
            .filter_map(|record| record.prompt_text().unwrap())
            .collect()
    }

    #[tokio::test]
    async fn a_second_owner_fences_the_first_writer() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let (mut first, first_writer) = TranscriptJournal::open(&config, "session").unwrap();
        first.defer_start(started("session"));
        first.append_local(prompt(1, "first")).unwrap();
        first.flush().await.unwrap();

        let (mut second, second_writer) = TranscriptJournal::open(&config, "session").unwrap();
        second.defer_start(started("session"));
        second.append_local(prompt(2, "second")).unwrap();
        second.flush().await.unwrap();
        first.append_local(prompt(3, "stale")).unwrap();
        drop(first);

        let error = first_writer.into_task().await.unwrap().unwrap_err();
        assert!(matches!(
            error,
            TranscriptError::Durability {
                source: DurabilityError::Store(StoreError::Fenced),
                ..
            }
        ));
        drop(second);
        second_writer.into_task().await.unwrap().unwrap();
        assert_eq!(prompts(&config, "session"), ["first", "second"]);
    }

    #[tokio::test]
    async fn a_journaled_session_is_listed_in_the_contract_index() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let (mut journal, writer) = TranscriptJournal::open(&config, "session").unwrap();
        journal.defer_start(started("session"));
        journal.append_local(prompt(1, "index me")).unwrap();
        journal.flush().await.unwrap();
        drop(journal);
        writer.into_task().await.unwrap().unwrap();

        let mut reader = SqliteStateReader::new(&config);
        let states = reader
            .query_states(&StateQuery {
                limit: 10,
                ..StateQuery::default()
            })
            .await
            .unwrap();
        let [state] = states.states.as_slice() else {
            panic!("expected the journaled session, found {states:?}");
        };
        assert_eq!(state.state_id, "session");
        assert_eq!(state.summary.preview.as_deref(), Some("index me"));
    }

    #[tokio::test]
    async fn resuming_a_session_stored_before_the_contract_continues_its_event_sequence() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let stored = [
            LocalEvent::SessionStarted(started("session")),
            prompt(1, "before"),
        ]
        .into_iter()
        .zip(1..)
        .map(|(event, sequence)| {
            Arc::new(TranscriptRecord::from_local(sequence, 100 + sequence, event).unwrap())
        })
        .collect::<Vec<_>>();
        SessionStorage::open(&config)
            .unwrap()
            .append_records("session", &stored)
            .unwrap();

        let (mut journal, writer) = TranscriptJournal::open_at(&config, "session", 3).unwrap();
        journal.defer_start(started("session"));
        journal
            .append_local(LocalEvent::EffortChanged(EffortChanged {
                from: ReasoningEffort::Medium,
                to: ReasoningEffort::High,
            }))
            .unwrap();
        drop(journal);
        writer.into_task().await.unwrap().unwrap();

        let connection = Connection::open(database_path(&config)).unwrap();
        let sequences = connection
            .prepare("SELECT seq, revision > 0 FROM events ORDER BY event_id")
            .unwrap()
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, bool>(1)?))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(sequences, [(1, false), (2, false), (3, true), (4, true)]);
        let payload: String = connection
            .query_row(
                "SELECT payload FROM durable_states WHERE state_id = 'session'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let metadata =
            &serde_json::from_str::<Value>(&payload).unwrap()["nanocodex_session"]["metadata"];
        assert_eq!(metadata["tact.effort"], json!("high"));
        assert_eq!(metadata["tact.application_version"], json!("test"));
    }

    #[tokio::test]
    async fn a_stopped_writer_discards_its_staged_host_batch() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        drop(SessionStorage::open(&config).unwrap());
        Connection::open(database_path(&config))
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER reject_events BEFORE INSERT ON events BEGIN
                     SELECT RAISE(ABORT, 'forced persistence failure');
                 END;",
            )
            .unwrap();
        let transcripts = DurableTranscripts::new(&config).unwrap();
        let (mut journal, writer) = TranscriptJournal::open_in(&transcripts, "session", 1).unwrap();
        journal.defer_start(started("session"));
        let _ = journal.append_local(prompt(1, "rejected"));

        assert!(writer.into_task().await.unwrap().is_err());
        assert_eq!(transcripts.store.staged_batches(), 0);
    }
}
