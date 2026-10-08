use crate::{
    app::config::{ReasoningEffort, ReasoningMode, Speed},
    core::context::ContextBudget,
};
use nanocodex::agent::events::{AgentEvent, AgentEventKind};
use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{self, Visitor, value::StrDeserializer},
};
use serde_json::value::{RawValue, to_raw_value};
use std::{fmt, path::PathBuf, sync::Arc};

pub(crate) const SCHEMA_VERSION: u32 = 2;

/// The component that produced a transcript record.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum RecordSource {
    /// The agent runtime's event stream.
    Agent,
    /// Tact itself: prompts, settings, shells, and worker lifecycle.
    Tact,
}

/// Local record kinds Tact writes, plus historical kinds it still reads.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum LocalKind {
    SessionStarted,
    UserSubmitted,
    UserSteered,
    ReflectionStarted,
    ShellStarted,
    ShellFinished,
    EffortChanged,
    SpeedChanged,
    /// Historical speed toggle stored as a boolean; read but never written.
    FastModeChanged,
    CompactionStarted,
    CompactionFinished,
    ContextBudget,
    ContextObserved,
    WorkerTurnAccepted,
    WorkerTurnFinished,
    WorkerTurnsInterrupted,
    WorkerSteerFailed,
    WorkerStopped,
    SessionEnded,
}

impl LocalKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::SessionStarted => "session.started",
            Self::UserSubmitted => "user.submitted",
            Self::UserSteered => "user.steered",
            Self::ReflectionStarted => "reflection.started",
            Self::ShellStarted => "shell.started",
            Self::ShellFinished => "shell.finished",
            Self::EffortChanged => "effort.changed",
            Self::SpeedChanged => "speed.changed",
            Self::FastModeChanged => "fast_mode.changed",
            Self::CompactionStarted => "compaction.started",
            Self::CompactionFinished => "compaction.finished",
            Self::ContextBudget => "context.budget",
            Self::ContextObserved => "context.observed",
            Self::WorkerTurnAccepted => "worker.turn_accepted",
            Self::WorkerTurnFinished => "worker.turn_finished",
            Self::WorkerTurnsInterrupted => "worker.turns_interrupted",
            Self::WorkerSteerFailed => "worker.steer_failed",
            Self::WorkerStopped => "worker.stopped",
            Self::SessionEnded => "session.ended",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "session.started" => Self::SessionStarted,
            "user.submitted" => Self::UserSubmitted,
            "user.steered" => Self::UserSteered,
            "reflection.started" => Self::ReflectionStarted,
            "shell.started" => Self::ShellStarted,
            "shell.finished" => Self::ShellFinished,
            "effort.changed" => Self::EffortChanged,
            "speed.changed" => Self::SpeedChanged,
            "fast_mode.changed" => Self::FastModeChanged,
            "compaction.started" => Self::CompactionStarted,
            "compaction.finished" => Self::CompactionFinished,
            "context.budget" => Self::ContextBudget,
            "context.observed" => Self::ContextObserved,
            "worker.turn_accepted" => Self::WorkerTurnAccepted,
            "worker.turn_finished" => Self::WorkerTurnFinished,
            "worker.turns_interrupted" => Self::WorkerTurnsInterrupted,
            "worker.steer_failed" => Self::WorkerSteerFailed,
            "worker.stopped" => Self::WorkerStopped,
            "session.ended" => Self::SessionEnded,
            _ => return None,
        })
    }
}

/// The stored `type` of a transcript record.
///
/// Serializes as the same dotted string the record has always carried. Kinds this build does not
/// know are kept verbatim so that reading and re-encoding a record never changes it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RecordKind {
    Agent(AgentEventKind),
    Local(LocalKind),
    Unrecognized(Box<str>),
}

impl RecordKind {
    pub(crate) fn as_str(&self) -> &str {
        match self {
            Self::Agent(kind) => agent_kind_name(*kind),
            Self::Local(kind) => kind.as_str(),
            Self::Unrecognized(kind) => kind,
        }
    }

    fn parse(value: &str) -> Self {
        if let Some(kind) = LocalKind::parse(value) {
            return Self::Local(kind);
        }
        let agent =
            AgentEventKind::deserialize(StrDeserializer::<serde::de::value::Error>::new(value));
        agent.map_or_else(|_| Self::Unrecognized(value.into()), Self::Agent)
    }
}

impl fmt::Display for RecordKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for RecordKind {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for RecordKind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct KindVisitor;

        impl Visitor<'_> for KindVisitor {
            type Value = RecordKind;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a transcript record type")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<RecordKind, E> {
                Ok(RecordKind::parse(value))
            }
        }

        deserializer.deserialize_str(KindVisitor)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub(crate) struct TurnId(u64);

impl TurnId {
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub(crate) struct ShellId(u64);

impl ShellId {
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct SessionStarted {
    pub(crate) session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) parent_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) parent_sequence: Option<u64>,
    pub(crate) model: String,
    pub(crate) effort: ReasoningEffort,
    pub(crate) reasoning_mode: ReasoningMode,
    #[serde(alias = "fast_mode", deserialize_with = "deserialize_session_speed")]
    pub(crate) speed: Speed,
    pub(crate) workspace: PathBuf,
    pub(crate) application_version: String,
}

fn deserialize_session_speed<'de, D>(deserializer: D) -> Result<Speed, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StoredSpeed {
        Speed(Speed),
        FastMode(bool),
    }

    Ok(match StoredSpeed::deserialize(deserializer)? {
        StoredSpeed::Speed(speed) => speed,
        StoredSpeed::FastMode(true) => Speed::Fast,
        StoredSpeed::FastMode(false) => Speed::Standard,
    })
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SessionOutcome {
    Closed,
    Cancelled,
    Failed,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TerminalStopReason {
    #[error("provider misalignment policy violation")]
    MisalignmentPolicyViolation,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct SessionEnded {
    pub(crate) outcome: SessionOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum LocalEvent {
    SessionStarted(SessionStarted),
    UserSubmitted {
        id: TurnId,
        text: String,
    },
    UserSteered {
        text: String,
    },
    ReflectionStarted {
        id: TurnId,
    },
    ShellStarted {
        id: ShellId,
        command: String,
        workspace: PathBuf,
    },
    ShellFinished {
        id: ShellId,
        output: String,
        exit_code: Option<i32>,
        duration_ns: u64,
        truncated: bool,
        error: Option<String>,
    },
    EffortChanged {
        from: ReasoningEffort,
        to: ReasoningEffort,
    },
    SpeedChanged {
        from: Speed,
        to: Speed,
    },
    CompactionStarted,
    CompactionFinished {
        terminal_stop: Option<TerminalStopReason>,
        error: Option<String>,
        duration_ns: u64,
    },
    ContextBudget(ContextBudget),
    ContextObserved {
        prompt_cache: bool,
        previous_response: bool,
    },
    WorkerTurnAccepted {
        id: TurnId,
    },
    WorkerTurnFinished {
        id: TurnId,
        error: Option<String>,
        terminal_stop: Option<TerminalStopReason>,
    },
    WorkerTurnsInterrupted {
        count: usize,
        error: Option<String>,
    },
    WorkerSteerFailed {
        error: String,
    },
    WorkerStopped {
        error: Option<String>,
    },
    SessionEnded(SessionEnded),
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct TranscriptRecord {
    schema_version: u32,
    sequence: u64,
    recorded_at_unix_ms: u64,
    source: RecordSource,
    #[serde(rename = "type")]
    kind: RecordKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<AgentMetadata>,
    payload: Arc<RawValue>,
}

#[derive(Debug, Deserialize, Serialize)]
struct AgentMetadata {
    protocol_version: u32,
    request_id: Arc<str>,
    sequence: u64,
}

impl TranscriptRecord {
    pub(crate) fn from_agent(sequence: u64, recorded_at_unix_ms: u64, event: AgentEvent) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            sequence,
            recorded_at_unix_ms,
            source: RecordSource::Agent,
            kind: RecordKind::Agent(event.kind),
            agent: Some(AgentMetadata {
                protocol_version: event.protocol_version,
                request_id: event.request_id,
                sequence: event.seq,
            }),
            payload: event.payload,
        }
    }

    pub(crate) fn from_local(
        sequence: u64,
        recorded_at_unix_ms: u64,
        event: LocalEvent,
    ) -> Result<Self, serde_json::Error> {
        let (kind, payload) = match event {
            LocalEvent::SessionStarted(payload) => {
                (LocalKind::SessionStarted, to_raw_value(&payload)?)
            }
            LocalEvent::UserSubmitted { id, text } => (
                LocalKind::UserSubmitted,
                to_raw_value(&UserSubmitted { id, text })?,
            ),
            LocalEvent::UserSteered { text } => {
                (LocalKind::UserSteered, to_raw_value(&UserSteered { text })?)
            }
            LocalEvent::ReflectionStarted { id } => (
                LocalKind::ReflectionStarted,
                to_raw_value(&ReflectionStarted { id })?,
            ),
            LocalEvent::ShellStarted {
                id,
                command,
                workspace,
            } => (
                LocalKind::ShellStarted,
                to_raw_value(&ShellStarted {
                    id,
                    command,
                    workspace,
                })?,
            ),
            LocalEvent::ShellFinished {
                id,
                output,
                exit_code,
                duration_ns,
                truncated,
                error,
            } => (
                LocalKind::ShellFinished,
                to_raw_value(&ShellFinished {
                    id,
                    output,
                    exit_code,
                    duration_ns,
                    truncated,
                    error,
                })?,
            ),
            LocalEvent::EffortChanged { from, to } => (
                LocalKind::EffortChanged,
                to_raw_value(&EffortChanged { from, to })?,
            ),
            LocalEvent::SpeedChanged { from, to } => (
                LocalKind::SpeedChanged,
                to_raw_value(&SpeedChanged { from, to })?,
            ),
            LocalEvent::ContextBudget(budget) => (LocalKind::ContextBudget, to_raw_value(&budget)?),
            LocalEvent::ContextObserved {
                prompt_cache,
                previous_response,
            } => (
                LocalKind::ContextObserved,
                to_raw_value(&ContextObserved {
                    prompt_cache,
                    previous_response,
                })?,
            ),
            LocalEvent::CompactionStarted => (LocalKind::CompactionStarted, to_raw_value(&())?),
            LocalEvent::CompactionFinished {
                error,
                duration_ns,
                terminal_stop,
            } => (
                LocalKind::CompactionFinished,
                to_raw_value(&CompactionFinished {
                    error,
                    duration_ns,
                    terminal_stop,
                })?,
            ),
            LocalEvent::WorkerTurnAccepted { id } => (
                LocalKind::WorkerTurnAccepted,
                to_raw_value(&WorkerTurn { id })?,
            ),
            LocalEvent::WorkerTurnFinished {
                id,
                error,
                terminal_stop,
            } => (
                LocalKind::WorkerTurnFinished,
                to_raw_value(&WorkerTurnFinished {
                    id,
                    error,
                    terminal_stop,
                })?,
            ),
            LocalEvent::WorkerTurnsInterrupted { count, error } => (
                LocalKind::WorkerTurnsInterrupted,
                to_raw_value(&WorkerTurnsInterrupted { count, error })?,
            ),
            LocalEvent::WorkerSteerFailed { error } => (
                LocalKind::WorkerSteerFailed,
                to_raw_value(&WorkerSteerFailed { error })?,
            ),
            LocalEvent::WorkerStopped { error } => (
                LocalKind::WorkerStopped,
                to_raw_value(&WorkerStopped { error })?,
            ),
            LocalEvent::SessionEnded(payload) => (LocalKind::SessionEnded, to_raw_value(&payload)?),
        };
        Ok(Self {
            schema_version: SCHEMA_VERSION,
            sequence,
            recorded_at_unix_ms,
            source: RecordSource::Tact,
            kind: RecordKind::Local(kind),
            agent: None,
            payload: payload.into(),
        })
    }

    pub(crate) const fn schema_version(&self) -> u32 {
        self.schema_version
    }

    pub(crate) const fn recorded_at_unix_ms(&self) -> u64 {
        self.recorded_at_unix_ms
    }

    pub(crate) const fn sequence(&self) -> u64 {
        self.sequence
    }

    pub(crate) fn kind(&self) -> &RecordKind {
        &self.kind
    }

    pub(crate) const fn source(&self) -> RecordSource {
        self.source
    }

    /// The kind of a record Tact wrote, or `None` for agent and unrecognized records.
    pub(crate) const fn local_kind(&self) -> Option<LocalKind> {
        match (self.source, &self.kind) {
            (RecordSource::Tact, RecordKind::Local(kind)) => Some(*kind),
            _ => None,
        }
    }

    /// The kind of an agent runtime event, or `None` for local and unrecognized records.
    pub(crate) const fn agent_kind(&self) -> Option<AgentEventKind> {
        match (self.source, &self.kind) {
            (RecordSource::Agent, RecordKind::Agent(kind)) => Some(*kind),
            _ => None,
        }
    }

    /// The session metadata carried by a well-formed `session.started` record.
    pub(crate) fn session_started(&self) -> Option<SessionStarted> {
        if self.local_kind() != Some(LocalKind::SessionStarted) {
            return None;
        }
        self.decode_payload().ok()
    }

    pub(crate) fn agent_request_id(&self) -> Option<Arc<str>> {
        self.agent
            .as_ref()
            .map(|metadata| Arc::clone(&metadata.request_id))
    }

    pub(crate) fn payload_json(&self) -> &str {
        self.payload.get()
    }

    pub(crate) fn decode_payload<'a, T>(&'a self) -> Result<T, serde_json::Error>
    where
        T: serde::Deserialize<'a>,
    {
        serde_json::from_str(self.payload.get())
    }
}

#[derive(Serialize)]
struct UserSubmitted {
    id: TurnId,
    text: String,
}

#[derive(Serialize)]
struct UserSteered {
    text: String,
}

#[derive(Serialize)]
struct ReflectionStarted {
    id: TurnId,
}

#[derive(Serialize)]
struct ShellStarted {
    id: ShellId,
    command: String,
    workspace: PathBuf,
}

#[derive(Serialize)]
struct ShellFinished {
    id: ShellId,
    output: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    duration_ns: u64,
    truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct EffortChanged {
    from: ReasoningEffort,
    to: ReasoningEffort,
}

#[derive(Serialize)]
struct SpeedChanged {
    from: Speed,
    to: Speed,
}

#[derive(Serialize)]
struct ContextObserved {
    prompt_cache: bool,
    previous_response: bool,
}

#[derive(Serialize)]
struct CompactionFinished {
    error: Option<String>,
    duration_ns: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    terminal_stop: Option<TerminalStopReason>,
}

#[derive(Serialize)]
struct WorkerTurn {
    id: TurnId,
}

#[derive(Serialize)]
struct WorkerTurnFinished {
    id: TurnId,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    terminal_stop: Option<TerminalStopReason>,
}

#[derive(Serialize)]
struct WorkerTurnsInterrupted {
    count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct WorkerSteerFailed {
    error: String,
}

#[derive(Serialize)]
struct WorkerStopped {
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

const fn agent_kind_name(kind: AgentEventKind) -> &'static str {
    match kind {
        AgentEventKind::ApiEvent => "api.event",
        AgentEventKind::AssistantDelta => "assistant.delta",
        AgentEventKind::AssistantMessage => "assistant.message",
        AgentEventKind::ReasoningSummaryDelta => "reasoning.summary.delta",
        AgentEventKind::InputAccepted => "input.accepted",
        AgentEventKind::RunStarted => "run.started",
        AgentEventKind::RunSteered => "run.steered",
        AgentEventKind::RunError => "run.error",
        AgentEventKind::RunCompleted => "run.completed",
        AgentEventKind::RunFailed => "run.failed",
        AgentEventKind::ToolCall => "tool.call",
        AgentEventKind::ToolResult => "tool.result",
        AgentEventKind::ModelWarmupStarted => "model.warmup.started",
        AgentEventKind::ModelWarmupCompleted => "model.warmup.completed",
        AgentEventKind::ModelWarmupFailed => "model.warmup.failed",
        AgentEventKind::ModelCallStarted => "model.call.started",
        AgentEventKind::ModelCallCompleted => "model.call.completed",
        AgentEventKind::ModelCallFailed => "model.call.failed",
        AgentEventKind::ModelCompactionStarted => "model.compaction.started",
        AgentEventKind::ModelCompactionCompleted => "model.compaction.completed",
        AgentEventKind::ModelCompactionFailed => "model.compaction.failed",
        AgentEventKind::ModelAttemptStarted => "model.attempt.started",
        AgentEventKind::ModelAttemptFailed => "model.attempt.failed",
        AgentEventKind::ModelAttemptRetrying => "model.attempt.retrying",
        AgentEventKind::ModelConnectionStarted => "model.connection.started",
        AgentEventKind::ModelConnectionCompleted => "model.connection.completed",
        AgentEventKind::ModelConnectionFailed => "model.connection.failed",
    }
}

#[cfg(test)]
mod tests {
    use super::{
        LocalEvent, LocalKind, RecordKind, SessionStarted, ShellId, TranscriptRecord, TurnId,
    };
    use crate::app::config::Speed;
    use nanocodex::agent::events::{AgentEvent, AgentEventKind};
    use serde_json::{Value, json, value::to_raw_value};
    use std::sync::Arc;

    const AGENT_KINDS: [AgentEventKind; 27] = [
        AgentEventKind::ApiEvent,
        AgentEventKind::AssistantDelta,
        AgentEventKind::AssistantMessage,
        AgentEventKind::ReasoningSummaryDelta,
        AgentEventKind::InputAccepted,
        AgentEventKind::RunStarted,
        AgentEventKind::RunSteered,
        AgentEventKind::RunError,
        AgentEventKind::RunCompleted,
        AgentEventKind::RunFailed,
        AgentEventKind::ToolCall,
        AgentEventKind::ToolResult,
        AgentEventKind::ModelWarmupStarted,
        AgentEventKind::ModelWarmupCompleted,
        AgentEventKind::ModelWarmupFailed,
        AgentEventKind::ModelCallStarted,
        AgentEventKind::ModelCallCompleted,
        AgentEventKind::ModelCallFailed,
        AgentEventKind::ModelCompactionStarted,
        AgentEventKind::ModelCompactionCompleted,
        AgentEventKind::ModelCompactionFailed,
        AgentEventKind::ModelAttemptStarted,
        AgentEventKind::ModelAttemptFailed,
        AgentEventKind::ModelAttemptRetrying,
        AgentEventKind::ModelConnectionStarted,
        AgentEventKind::ModelConnectionCompleted,
        AgentEventKind::ModelConnectionFailed,
    ];

    const LOCAL_KINDS: [LocalKind; 19] = [
        LocalKind::SessionStarted,
        LocalKind::UserSubmitted,
        LocalKind::UserSteered,
        LocalKind::ReflectionStarted,
        LocalKind::ShellStarted,
        LocalKind::ShellFinished,
        LocalKind::EffortChanged,
        LocalKind::SpeedChanged,
        LocalKind::FastModeChanged,
        LocalKind::CompactionStarted,
        LocalKind::CompactionFinished,
        LocalKind::ContextBudget,
        LocalKind::ContextObserved,
        LocalKind::WorkerTurnAccepted,
        LocalKind::WorkerTurnFinished,
        LocalKind::WorkerTurnsInterrupted,
        LocalKind::WorkerSteerFailed,
        LocalKind::WorkerStopped,
        LocalKind::SessionEnded,
    ];

    #[test]
    fn agent_kinds_keep_the_runtime_event_names() {
        for kind in AGENT_KINDS {
            let record = TranscriptRecord::from_agent(
                1,
                1,
                AgentEvent {
                    protocol_version: 1,
                    request_id: Arc::from("request"),
                    seq: 1,
                    kind,
                    payload: to_raw_value(&json!({})).unwrap().into(),
                },
            );
            let encoded = serde_json::to_value(&record).unwrap();
            assert_eq!(encoded["type"], serde_json::to_value(kind).unwrap());

            let decoded: TranscriptRecord = serde_json::from_value(encoded).unwrap();
            assert_eq!(decoded.agent_kind(), Some(kind));
            assert_eq!(decoded.local_kind(), None);
        }
    }

    #[test]
    fn local_kinds_parse_from_their_stored_names() {
        for kind in LOCAL_KINDS {
            let decoded: RecordKind = serde_json::from_value(json!(kind.as_str())).unwrap();
            assert_eq!(decoded, RecordKind::Local(kind));
        }
    }

    #[test]
    fn unrecognized_kinds_round_trip_verbatim() {
        let encoded = json!({
            "schema_version": 2,
            "sequence": 1,
            "recorded_at_unix_ms": 1,
            "source": "tact",
            "type": "future.event",
            "payload": {"value": 1},
        });
        let record: TranscriptRecord = serde_json::from_value(encoded.clone()).unwrap();

        assert_eq!(
            record.kind(),
            &RecordKind::Unrecognized("future.event".into())
        );
        assert_eq!(record.local_kind(), None);
        assert_eq!(serde_json::to_value(record).unwrap(), encoded);
    }

    #[test]
    fn kinds_require_a_matching_source() {
        let record: TranscriptRecord = serde_json::from_value(json!({
            "schema_version": 2,
            "sequence": 1,
            "recorded_at_unix_ms": 1,
            "source": "agent",
            "type": "session.started",
            "payload": {},
        }))
        .unwrap();

        assert_eq!(record.local_kind(), None);
        assert_eq!(record.agent_kind(), None);
    }

    fn session_payload() -> Value {
        json!({
            "session_id": "session",
            "model": "gpt-6-astra",
            "effort": "medium",
            "reasoning_mode": "standard",
            "workspace": "/work",
            "application_version": "test",
        })
    }

    #[test]
    fn session_speed_round_trips_without_a_boolean_field() {
        for speed in Speed::ALL {
            let mut payload = session_payload();
            payload["speed"] = json!(speed);
            let started: SessionStarted = serde_json::from_value(payload).unwrap();
            let record =
                TranscriptRecord::from_local(1, 123, LocalEvent::SessionStarted(started.clone()))
                    .unwrap();
            let encoded = serde_json::to_value(&record).unwrap();

            assert_eq!(encoded["payload"]["speed"], json!(speed));
            assert!(encoded["payload"].get("fast_mode").is_none());
            let decoded: TranscriptRecord = serde_json::from_value(encoded).unwrap();
            assert_eq!(decoded.decode_payload::<SessionStarted>().unwrap(), started);
        }
    }

    #[test]
    fn historical_session_booleans_decode_at_the_payload_boundary() {
        for (enabled, expected) in [(false, Speed::Standard), (true, Speed::Fast)] {
            let mut payload = session_payload();
            payload["fast_mode"] = json!(enabled);
            let started: SessionStarted = serde_json::from_value(payload.clone()).unwrap();
            assert_eq!(started.speed, expected);

            payload["speed"] = json!(Speed::Ultrafast);
            assert!(serde_json::from_value::<SessionStarted>(payload).is_err());
        }
    }

    #[test]
    fn speed_change_records_keep_the_exact_preference() {
        let record = TranscriptRecord::from_local(
            2,
            124,
            LocalEvent::SpeedChanged {
                from: Speed::Fast,
                to: Speed::Ultrafast,
            },
        )
        .unwrap();
        assert_eq!(record.local_kind(), Some(LocalKind::SpeedChanged));
        assert_eq!(
            serde_json::to_value(record).unwrap()["payload"],
            json!({"from": "fast", "to": "ultrafast"})
        );
    }

    #[test]
    fn agent_record_retains_protocol_metadata_and_raw_payload() {
        let payload = json!({"text": "hello"});
        let record = TranscriptRecord::from_agent(
            7,
            123,
            AgentEvent {
                protocol_version: 1,
                request_id: Arc::from("session-a"),
                seq: 4,
                kind: AgentEventKind::AssistantDelta,
                payload: to_raw_value(&payload).unwrap().into(),
            },
        );
        let encoded = serde_json::to_value(record).unwrap();

        assert_eq!(encoded["schema_version"], 2);
        assert_eq!(encoded["sequence"], 7);
        assert_eq!(encoded["recorded_at_unix_ms"], 123);
        assert_eq!(encoded["source"], "agent");
        assert_eq!(encoded["type"], "assistant.delta");
        assert_eq!(encoded["agent"]["request_id"], "session-a");
        assert_eq!(encoded["agent"]["sequence"], 4);
        assert_eq!(encoded["payload"], payload);
    }

    #[test]
    fn local_record_uses_typed_payload() {
        let record = TranscriptRecord::from_local(
            1,
            123,
            LocalEvent::UserSubmitted {
                id: TurnId::new(9),
                text: "hello".to_owned(),
            },
        )
        .unwrap();
        let encoded = serde_json::to_value(record).unwrap();

        assert_eq!(encoded["source"], "tact");
        assert_eq!(encoded["type"], "user.submitted");
        assert_eq!(encoded["payload"], json!({"id": 9, "text": "hello"}));
    }

    #[test]
    fn shell_lifecycle_uses_structured_local_records() {
        let started = TranscriptRecord::from_local(
            1,
            123,
            LocalEvent::ShellStarted {
                id: ShellId::new(4),
                command: "pwd".to_owned(),
                workspace: "/work".into(),
            },
        )
        .unwrap();
        let finished = TranscriptRecord::from_local(
            2,
            124,
            LocalEvent::ShellFinished {
                id: ShellId::new(4),
                output: "/work\n".to_owned(),
                exit_code: Some(0),
                duration_ns: 10,
                truncated: false,
                error: None,
            },
        )
        .unwrap();

        assert_eq!(started.local_kind(), Some(LocalKind::ShellStarted));
        assert_eq!(finished.local_kind(), Some(LocalKind::ShellFinished));
        assert_eq!(
            serde_json::to_value(finished).unwrap()["payload"],
            json!({
                "id": 4,
                "output": "/work\n",
                "exit_code": 0,
                "duration_ns": 10,
                "truncated": false,
            })
        );
    }

    #[test]
    fn applied_steer_has_a_distinct_local_record() {
        let record = TranscriptRecord::from_local(
            1,
            123,
            LocalEvent::UserSteered {
                text: "change direction".to_owned(),
            },
        )
        .unwrap();
        let encoded = serde_json::to_value(record).unwrap();

        assert_eq!(encoded["type"], "user.steered");
        assert_eq!(encoded["payload"]["text"], "change direction");
    }

    #[test]
    fn reflection_start_contains_only_its_turn_id() {
        let record = TranscriptRecord::from_local(
            1,
            123,
            LocalEvent::ReflectionStarted { id: TurnId::new(9) },
        )
        .unwrap();
        let encoded = serde_json::to_value(record).unwrap();

        assert_eq!(encoded["type"], "reflection.started");
        assert_eq!(encoded["payload"], json!({"id": 9}));
    }
}
