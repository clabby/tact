//! Structured child-agent evidence for headless runs.

use crate::{
    app::error::RuntimeError,
    core::extensions::subagents::{
        AgentDescriptor, AgentId, AgentMessageUpdate, AgentStatus, AgentUpdate, ScopedAgentUpdate,
    },
};
use nanocodex::agent::events::{
    AgentEvent, AgentEventData, AgentEventKind, EventUsage, RunEvent, RunTerminal,
};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufWriter, Write},
    path::PathBuf,
};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

const PROTOCOL_VERSION: u32 = 1;

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum AgentOrigin {
    Spawn,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum RunOutcome {
    Completed,
    Cancelled,
    Failed,
}

pub(super) struct OrchestrationRecorder {
    finish: oneshot::Sender<Finish>,
    task: JoinHandle<Result<(), RuntimeError>>,
}

struct Finish {
    root_session_id: String,
    outcome: RunOutcome,
}

struct Recorder {
    path: PathBuf,
    output: BufWriter<File>,
    sequence: u64,
    agents: BTreeMap<AgentId, ObservedAgent>,
    child_metrics: ChildMetrics,
}

struct ObservedAgent {
    descriptor: AgentDescriptor,
    final_status: AgentStatus,
    outcome: Option<AgentStatus>,
}

#[derive(Serialize)]
struct Record<'a, T> {
    protocol_version: u32,
    sequence: u64,
    root_session_id: &'a str,
    #[serde(flatten)]
    payload: T,
}

#[derive(Serialize)]
#[serde(tag = "type")]
enum UpdateRecord<'a> {
    #[serde(rename = "agent.added")]
    Added { agent: AgentRecord<'a> },
    #[serde(rename = "agent.event")]
    Event {
        agent_id: AgentId,
        event: &'a AgentEvent,
    },
    #[serde(rename = "agent.status")]
    Status {
        agent_id: AgentId,
        status: &'a AgentStatus,
    },
    #[serde(rename = "agent.message")]
    Message { update: &'a AgentMessageUpdate },
}

#[derive(Serialize)]
struct AgentRecord<'a> {
    id: AgentId,
    session_id: &'a str,
    role: &'a str,
    task: &'a str,
    origin: AgentOrigin,
    parent_id: Option<AgentId>,
}

#[derive(Serialize)]
#[serde(tag = "type")]
enum SummaryRecord {
    #[serde(rename = "orchestration.completed")]
    Completed {
        outcome: RunOutcome,
        agents_started: usize,
        active_agent_ids: Vec<AgentId>,
        failed_agent_ids: Vec<AgentId>,
        agents: Vec<AgentSummary>,
        child_metrics: ChildMetrics,
    },
}

#[derive(Serialize)]
struct AgentSummary {
    id: AgentId,
    role: String,
    parent_id: Option<AgentId>,
    origin: AgentOrigin,
    final_status: AgentStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<AgentStatus>,
}

#[derive(Clone, Default, Serialize)]
struct ChildMetrics {
    turns: u64,
    model_calls: u64,
    tool_calls: u64,
    cost_usd: Option<f64>,
    #[serde(with = "SerializableEventUsage")]
    usage: EventUsage,
    #[serde(with = "SerializableEventUsage")]
    warmup_usage: EventUsage,
}

#[derive(Serialize)]
#[serde(remote = "EventUsage")]
struct SerializableEventUsage {
    input_tokens: u64,
    cached_input_tokens: u64,
    cache_write_input_tokens: u64,
    output_tokens: u64,
    reasoning_output_tokens: u64,
    total_tokens: u64,
}

impl OrchestrationRecorder {
    pub(super) fn start(
        mut updates: mpsc::UnboundedReceiver<ScopedAgentUpdate>,
        path: Option<PathBuf>,
    ) -> Result<Self, RuntimeError> {
        let mut recorder = path.map(Recorder::create).transpose()?;
        let (finish, mut finished) = oneshot::channel::<Finish>();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    finish = &mut finished => {
                        let Ok(finish) = finish else {
                            return Ok(());
                        };
                        while let Ok(update) = updates.try_recv() {
                            if let Some(recorder) = &mut recorder {
                                recorder.update(&update)?;
                            }
                        }
                        if let Some(recorder) = &mut recorder {
                            recorder.finish(&finish.root_session_id, finish.outcome)?;
                        }
                        return Ok(());
                    }
                    update = updates.recv() => {
                        let Some(update) = update else {
                            continue;
                        };
                        if let Some(recorder) = &mut recorder {
                            recorder.update(&update)?;
                        }
                    }
                }
            }
        });
        Ok(Self { finish, task })
    }

    pub(super) async fn finish(
        self,
        root_session_id: &str,
        outcome: RunOutcome,
    ) -> Result<(), RuntimeError> {
        let _ = self.finish.send(Finish {
            root_session_id: root_session_id.to_owned(),
            outcome,
        });
        self.task
            .await
            .map_err(RuntimeError::OrchestrationLogTask)??;
        Ok(())
    }
}

impl Recorder {
    fn create(path: PathBuf) -> Result<Self, RuntimeError> {
        let file = File::create(&path).map_err(|source| RuntimeError::CreateOrchestrationLog {
            path: path.clone(),
            source,
        })?;
        Ok(Self {
            path,
            output: BufWriter::new(file),
            sequence: 0,
            agents: BTreeMap::new(),
            child_metrics: ChildMetrics::default(),
        })
    }

    fn update(&mut self, scoped: &ScopedAgentUpdate) -> Result<(), RuntimeError> {
        let payload = match &scoped.update {
            AgentUpdate::Added(descriptor) => {
                self.agents.insert(
                    descriptor.id,
                    ObservedAgent {
                        descriptor: descriptor.clone(),
                        final_status: AgentStatus::Pending,
                        outcome: None,
                    },
                );
                UpdateRecord::Added {
                    agent: AgentRecord::from(descriptor),
                }
            }
            AgentUpdate::Event { id, event } => {
                self.child_metrics.observe(event);
                UpdateRecord::Event {
                    agent_id: *id,
                    event,
                }
            }
            AgentUpdate::Status { id, status } => {
                if let Some(agent) = self.agents.get_mut(id) {
                    agent.final_status = status.clone();
                    if matches!(
                        status,
                        AgentStatus::Completed { .. }
                            | AgentStatus::Interrupted
                            | AgentStatus::Failed { .. }
                    ) {
                        agent.outcome = Some(status.clone());
                    }
                }
                UpdateRecord::Status {
                    agent_id: *id,
                    status,
                }
            }
            AgentUpdate::Message(update) => UpdateRecord::Message { update },
        };
        self.write(&scoped.root_session_id, payload)
    }

    fn finish(&mut self, root_session_id: &str, outcome: RunOutcome) -> Result<(), RuntimeError> {
        let active_agent_ids = self
            .agents
            .iter()
            .filter_map(|(id, agent)| agent.final_status.is_active().then_some(*id))
            .collect();
        let failed_agent_ids = self
            .agents
            .iter()
            .filter_map(|(id, agent)| {
                agent
                    .outcome
                    .as_ref()
                    .is_some_and(|status| matches!(status, AgentStatus::Failed { .. }))
                    .then_some(*id)
            })
            .collect();
        let agents = self
            .agents
            .values()
            .map(|agent| AgentSummary {
                id: agent.descriptor.id,
                role: agent.descriptor.role.clone(),
                parent_id: agent.descriptor.parent,
                origin: AgentOrigin::Spawn,
                final_status: agent.final_status.clone(),
                outcome: agent.outcome.clone(),
            })
            .collect::<Vec<_>>();
        self.write(
            root_session_id,
            SummaryRecord::Completed {
                outcome,
                agents_started: agents.len(),
                active_agent_ids,
                failed_agent_ids,
                agents,
                child_metrics: self.child_metrics.clone(),
            },
        )
    }

    fn write(
        &mut self,
        root_session_id: &str,
        payload: impl Serialize,
    ) -> Result<(), RuntimeError> {
        self.sequence = self.sequence.saturating_add(1);
        let record = Record {
            protocol_version: PROTOCOL_VERSION,
            sequence: self.sequence,
            root_session_id,
            payload,
        };
        serde_json::to_writer(&mut self.output, &record).map_err(|source| {
            RuntimeError::EncodeOrchestrationLog {
                path: self.path.clone(),
                source,
            }
        })?;
        self.output
            .write_all(b"\n")
            .and_then(|()| self.output.flush())
            .map_err(|source| RuntimeError::WriteOrchestrationLog {
                path: self.path.clone(),
                source,
            })
    }
}

impl ChildMetrics {
    fn observe(&mut self, event: &AgentEvent) {
        if event.kind == AgentEventKind::ToolCall {
            self.tool_calls = self.tool_calls.saturating_add(1);
        }
        if !event.kind.is_terminal() {
            return;
        }

        let Ok(AgentEventData::Run(run)) = event.data() else {
            return;
        };
        let terminal = match run {
            RunEvent::Completed(terminal) | RunEvent::Failed(terminal) => terminal,
            _ => return,
        };

        self.observe_terminal(&terminal);
    }

    fn observe_terminal(&mut self, terminal: &RunTerminal) {
        self.turns = self.turns.saturating_add(1);
        self.model_calls = self
            .model_calls
            .saturating_add(u64::from(terminal.metrics.model_calls));
        if let Some(cost_usd) = terminal.cost_usd {
            *self.cost_usd.get_or_insert(0.0) += cost_usd;
        }
        add_usage(&mut self.usage, &terminal.metrics.usage);
        add_usage(&mut self.warmup_usage, &terminal.metrics.warmup_usage);
    }
}

fn add_usage(total: &mut EventUsage, usage: &EventUsage) {
    total.input_tokens = total.input_tokens.saturating_add(usage.input_tokens);
    total.cached_input_tokens = total
        .cached_input_tokens
        .saturating_add(usage.cached_input_tokens);
    total.cache_write_input_tokens = total
        .cache_write_input_tokens
        .saturating_add(usage.cache_write_input_tokens);
    total.output_tokens = total.output_tokens.saturating_add(usage.output_tokens);
    total.reasoning_output_tokens = total
        .reasoning_output_tokens
        .saturating_add(usage.reasoning_output_tokens);
    total.total_tokens = total.total_tokens.saturating_add(usage.total_tokens);
}

impl<'a> From<&'a AgentDescriptor> for AgentRecord<'a> {
    fn from(descriptor: &'a AgentDescriptor) -> Self {
        Self {
            id: descriptor.id,
            session_id: &descriptor.session_id,
            role: &descriptor.role,
            task: &descriptor.task,
            origin: AgentOrigin::Spawn,
            parent_id: descriptor.parent,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{OrchestrationRecorder, RunOutcome};
    use crate::core::extensions::subagents::{
        AgentDescriptor, AgentId, AgentStatus, AgentUpdate, ScopedAgentUpdate,
    };
    use nanocodex::agent::events::{AgentEvent, AgentEventKind};
    use serde_json::{Value, json, value::to_raw_value};
    use std::{fs, sync::Arc};
    use tempfile::tempdir;
    use tokio::sync::mpsc;

    fn run_terminal(model_calls: u32, cost_usd: Option<f64>) -> Value {
        json!({
            "status": "completed",
            "model": "gpt-5.6-sol",
            "reasoning_mode": "summary",
            "effort": "high",
            "transport": "responses_websocket_v2",
            "orchestration": "local",
            "duration_ms": 1,
            "duration_ns": 1_000_000,
            "model_calls": model_calls,
            "steers": 0,
            "compactions": 0,
            "tool_calls": 0,
            "connection_attempts": 1,
            "websocket_reconnects": 0,
            "response_attempts": model_calls,
            "response_retries": 0,
            "connection_duration_ns": 100,
            "retry_backoff_duration_ns": 0,
            "model_duration_ns": 900_000,
            "compaction_duration_ns": 0,
            "warmup_duration_ns": 100_000,
            "tool_work_duration_ns": 0,
            "tool_wall_duration_ns": 0,
            "usage": {
                "input_tokens": 100,
                "cached_input_tokens": 60,
                "cache_write_input_tokens": 0,
                "output_tokens": 20,
                "reasoning_output_tokens": 0,
                "total_tokens": 120
            },
            "warmup_usage": {
                "input_tokens": 10,
                "cached_input_tokens": 0,
                "cache_write_input_tokens": 0,
                "output_tokens": 0,
                "reasoning_output_tokens": 0,
                "total_tokens": 10
            },
            "estimated_cost": null,
            "cost_usd": cost_usd
        })
    }

    #[tokio::test]
    async fn records_child_lifecycle_and_final_cleanup_state() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("orchestration.jsonl");
        let (sender, receiver) = mpsc::unbounded_channel();
        let recorder = OrchestrationRecorder::start(receiver, Some(path.clone())).unwrap();
        let id = AgentId::new(1);
        sender
            .send(ScopedAgentUpdate {
                root_session_id: "root".to_owned(),
                update: AgentUpdate::Added(AgentDescriptor {
                    id,
                    session_id: "child".to_owned(),
                    role: "researcher".to_owned(),
                    task: "inspect the task".to_owned(),
                    parent: None,
                }),
            })
            .unwrap();
        sender
            .send(ScopedAgentUpdate {
                root_session_id: "root".to_owned(),
                update: AgentUpdate::Event {
                    id,
                    event: AgentEvent {
                        protocol_version: 1,
                        request_id: Arc::from("child"),
                        seq: 1,
                        kind: AgentEventKind::RunCompleted,
                        payload: to_raw_value(&run_terminal(2, Some(0.25))).unwrap().into(),
                    },
                },
            })
            .unwrap();
        for status in [
            AgentStatus::Running,
            AgentStatus::Completed {
                output: json!("done"),
            },
            AgentStatus::Closed,
        ] {
            sender
                .send(ScopedAgentUpdate {
                    root_session_id: "root".to_owned(),
                    update: AgentUpdate::Status { id, status },
                })
                .unwrap();
        }

        recorder
            .finish("root", RunOutcome::Completed)
            .await
            .unwrap();

        let records = fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(records.len(), 6);
        assert_eq!(records[0]["type"], "agent.added");
        assert_eq!(records[5]["type"], "orchestration.completed");
        assert_eq!(records[5]["agents_started"], 1);
        assert_eq!(records[5]["active_agent_ids"], serde_json::json!([]));
        assert_eq!(records[5]["failed_agent_ids"], serde_json::json!([]));
        assert_eq!(records[5]["agents"][0]["final_status"]["state"], "closed");
        assert_eq!(records[5]["agents"][0]["outcome"]["state"], "completed");
        assert_eq!(records[5]["child_metrics"]["turns"], 1);
        assert_eq!(records[5]["child_metrics"]["model_calls"], 2);
        assert_eq!(records[5]["child_metrics"]["cost_usd"], 0.25);
        assert_eq!(records[5]["child_metrics"]["usage"]["total_tokens"], 120);
        assert_eq!(
            records[5]["child_metrics"]["warmup_usage"]["total_tokens"],
            10
        );
    }

    #[tokio::test]
    async fn preserves_unavailable_child_cost() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("orchestration.jsonl");
        let (sender, receiver) = mpsc::unbounded_channel();
        let recorder = OrchestrationRecorder::start(receiver, Some(path.clone())).unwrap();
        let id = AgentId::new(1);
        sender
            .send(ScopedAgentUpdate {
                root_session_id: "root".to_owned(),
                update: AgentUpdate::Event {
                    id,
                    event: AgentEvent {
                        protocol_version: 1,
                        request_id: Arc::from("child"),
                        seq: 1,
                        kind: AgentEventKind::RunCompleted,
                        payload: to_raw_value(&run_terminal(1, None)).unwrap().into(),
                    },
                },
            })
            .unwrap();

        recorder
            .finish("root", RunOutcome::Completed)
            .await
            .unwrap();

        let summary = fs::read_to_string(path).unwrap();
        let summary = serde_json::from_str::<Value>(summary.lines().last().unwrap()).unwrap();
        assert_eq!(summary["child_metrics"]["cost_usd"], Value::Null);
    }

    #[tokio::test]
    async fn disabled_recording_still_drains_updates() {
        let (sender, receiver) = mpsc::unbounded_channel();
        let recorder = OrchestrationRecorder::start(receiver, None).unwrap();
        drop(sender);

        recorder
            .finish("root", RunOutcome::Completed)
            .await
            .unwrap();
    }
}
