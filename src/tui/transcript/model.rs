use super::{
    DirectedMessageEntry, EntryId, EntryKind, MessageDelivery, MessagePhase, ShellId, ToolEntry,
    ToolState, TranscriptEntry, TranscriptRecord, TransientStatus,
};
use crate::{
    app::config::ReasoningEffort,
    core::extensions::subagents::{
        AgentMessageUpdate, MessageDeliveryState, MessageDisposition, MessageSender, ThreadId,
    },
    tui::format::{format_duration, humanize_tool},
};
use nanocodex::{
    agent::events::{
        AssistantDelta, AssistantMessage, CompactionCompleted, CompactionFailed,
        ReasoningSummaryDelta, RunError,
    },
    oai::responses::MessagePhase as AgentMessagePhase,
};
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
};

const MAX_RETAINED_MESSAGE_THREADS: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EventVisibility {
    Persistent,
    Transient,
    StateOnly,
    ErrorFallback,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ModelChange {
    pub(crate) changed: bool,
    pub(crate) removed: Option<EntryId>,
}

#[derive(Default)]
pub(crate) struct TranscriptModel {
    entries: Vec<TranscriptEntry>,
    entry_indices: HashMap<EntryId, usize>,
    next_entry_id: usize,
    assistants: HashMap<AssistantKey, EntryId>,
    active_assistants: HashMap<(u32, MessagePhase), EntryId>,
    reasoning: HashMap<u32, EntryId>,
    tools: HashMap<String, EntryId>,
    shell_sessions: HashMap<i64, EntryId>,
    local_shells: HashMap<ShellId, EntryId>,
    message_threads: HashMap<ThreadId, EntryId>,
    message_order: VecDeque<ThreadId>,
    running_tools: HashSet<EntryId>,
    active_runs: usize,
    run_started_at_unix_ms: VecDeque<u64>,
    transient: Option<TransientStatus>,
    pending_error: Option<String>,
    pending_compaction_error: Option<String>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct AssistantKey {
    call: u32,
    item: Option<String>,
    phase: MessagePhase,
}

impl TranscriptModel {
    /// Copies the latest stable visual history without carrying live projection state.
    pub(crate) fn fork_snapshot(&self) -> Self {
        let end = if self.is_active() {
            self.entries
                .iter()
                .rposition(|entry| matches!(entry.kind, EntryKind::User { .. }))
                .unwrap_or(self.entries.len())
        } else {
            self.entries.len()
        };
        let entries = self.entries[..end]
            .iter()
            .filter(|entry| match &entry.kind {
                EntryKind::Assistant { complete, .. } => *complete,
                EntryKind::Tool(tool) => tool.state != ToolState::Running,
                _ => true,
            })
            .cloned()
            .collect::<Vec<_>>();
        let entry_indices = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.id, index))
            .collect();
        let message_threads = entries
            .iter()
            .filter_map(|entry| match &entry.kind {
                EntryKind::DirectedMessage(message) => Some((message.thread.id, entry.id)),
                _ => None,
            })
            .collect();
        let message_order = entries
            .iter()
            .filter_map(|entry| match &entry.kind {
                EntryKind::DirectedMessage(message) => Some(message.thread.id),
                _ => None,
            })
            .collect();
        Self {
            entries,
            entry_indices,
            next_entry_id: self.next_entry_id,
            message_threads,
            message_order,
            ..Self::default()
        }
    }

    pub(crate) fn entries(&self) -> &[TranscriptEntry] {
        &self.entries
    }

    pub(crate) fn entry(&self, id: EntryId) -> Option<&TranscriptEntry> {
        self.index_of(id).and_then(|index| self.entries.get(index))
    }

    pub(crate) fn index_of(&self, id: EntryId) -> Option<usize> {
        self.entry_indices.get(&id).copied()
    }

    pub(crate) fn transient(&self) -> Option<&TransientStatus> {
        self.transient.as_ref()
    }

    pub(crate) const fn is_active(&self) -> bool {
        self.active_runs > 0
    }

    pub(crate) fn has_running_tools(&self) -> bool {
        !self.running_tools.is_empty()
    }

    pub(crate) fn running_tool_ids(&self) -> impl Iterator<Item = EntryId> + '_ {
        self.running_tools.iter().copied()
    }

    pub(crate) fn apply(&mut self, record: &TranscriptRecord) -> ModelChange {
        if record.source() == "tact" {
            return self.apply_local(record);
        }
        if record.source() != "agent" {
            return ModelChange::default();
        }
        self.apply_agent(record)
    }

    pub(crate) fn apply_message(
        &mut self,
        perspective: MessageSender,
        update: AgentMessageUpdate,
    ) -> ModelChange {
        let Some(id) = self.message_threads.get(&update.thread.id).copied() else {
            let thread_id = update.thread.id;
            let id = self.push(EntryKind::DirectedMessage(DirectedMessageEntry {
                perspective,
                thread: update.thread,
                deliveries: vec![MessageDelivery {
                    message_id: update.message_id,
                    state: update.delivery,
                }],
            }));
            self.message_threads.insert(thread_id, id);
            self.message_order.push_back(thread_id);
            return ModelChange {
                changed: true,
                removed: self.trim_message_history(),
            };
        };

        let Some(index) = self.index_of(id) else {
            return ModelChange::default();
        };
        let EntryKind::DirectedMessage(message) = &self.entries[index].kind else {
            return ModelChange::default();
        };
        let previous_delivery = message
            .deliveries
            .iter()
            .find(|delivery| delivery.message_id == update.message_id);
        let changed = message.thread != update.thread
            || previous_delivery
                .is_none_or(|delivery| delivery_advances(&delivery.state, &update.delivery));
        if !changed {
            return ModelChange::default();
        }

        let EntryKind::DirectedMessage(message) = &mut self.entries[index].kind else {
            return ModelChange::default();
        };
        message.thread = update.thread;
        message.deliveries.retain(|delivery| {
            message
                .thread
                .messages
                .iter()
                .any(|retained| retained.id == delivery.message_id)
        });
        let delivery = message
            .deliveries
            .iter_mut()
            .find(|delivery| delivery.message_id == update.message_id);
        match delivery {
            Some(delivery) if delivery_advances(&delivery.state, &update.delivery) => {
                delivery.state = update.delivery;
            }
            None => message.deliveries.push(MessageDelivery {
                message_id: update.message_id,
                state: update.delivery,
            }),
            Some(_) => {}
        }
        self.entries[index].revision = self.entries[index].revision.saturating_add(1);
        ModelChange {
            changed: true,
            removed: self.trim_message_history(),
        }
    }

    fn apply_local(&mut self, record: &TranscriptRecord) -> ModelChange {
        let changed = match record.kind() {
            "user.submitted" => self.decode_local::<UserSubmitted>(record).map(|payload| {
                self.push(EntryKind::User { text: payload.text });
            }),
            "user.steered" => self.decode_local::<UserSteered>(record).map(|payload| {
                self.push(EntryKind::User { text: payload.text });
            }),
            "shell.started" => self
                .decode_local::<ShellStarted>(record)
                .map(|payload| self.shell_started(payload, record.recorded_at_unix_ms())),
            "shell.finished" => self
                .decode_local::<ShellFinished>(record)
                .map(|payload| self.shell_finished(payload)),
            "effort.changed" => self.decode_local::<EffortChanged>(record).map(|payload| {
                self.push(EntryKind::EffortChanged { to: payload.to });
            }),
            "fast_mode.changed" => self.decode_local::<FastModeChanged>(record).map(|payload| {
                self.push(EntryKind::FastModeChanged {
                    enabled: payload.to,
                });
            }),
            "worker.turn_finished" => {
                self.decode_local::<WorkerTurnFinished>(record)
                    .map(|payload| {
                        if let Some(error) = payload.error {
                            self.pending_error = Some(error);
                        }
                    })
            }
            "worker.turns_interrupted" => return self.apply_interruption(record),
            "worker.steer_failed" => {
                self.decode_local::<WorkerSteerFailed>(record)
                    .map(|payload| {
                        self.push(EntryKind::Error {
                            message: format!("Could not steer response: {}", payload.error),
                        });
                    })
            }
            "worker.stopped" => self.decode_local::<WorkerStopped>(record).map(|payload| {
                if let Some(error) = payload.error {
                    self.pending_error = Some(error);
                }
            }),
            "session.ended" => self.decode_local::<SessionEnded>(record).map(|payload| {
                if payload.outcome == "failed" {
                    self.finish_failed(payload.error);
                }
                self.agent_stream_closed();
            }),
            _ => return ModelChange::default(),
        };
        match changed {
            Ok(()) => ModelChange {
                changed: true,
                ..ModelChange::default()
            },
            Err(error) => self.projection_error(record, error, true),
        }
    }

    fn apply_interruption(&mut self, record: &TranscriptRecord) -> ModelChange {
        let payload = match self.decode_local::<WorkerTurnsInterrupted>(record) {
            Ok(payload) => payload,
            Err(error) => return self.projection_error(record, error, true),
        };
        if let Some(error) = payload.error {
            self.push(EntryKind::Error {
                message: format!("Could not interrupt response: {error}"),
            });
            return ModelChange {
                changed: true,
                ..ModelChange::default()
            };
        }
        if payload.count == 0 {
            return ModelChange::default();
        }
        self.push(EntryKind::Interrupted {
            count: payload.count,
        });
        ModelChange {
            changed: true,
            ..ModelChange::default()
        }
    }

    fn shell_started(&mut self, payload: ShellStarted, started_at_unix_ms: u64) {
        let id = self.push(EntryKind::Tool(ToolEntry {
            name: "exec_command".to_owned(),
            arguments: serde_json::json!({
                "cmd": payload.command,
                "workdir": payload.workspace,
            }),
            started_at_unix_ms,
            state: ToolState::Running,
            duration_ns: None,
            result: None,
            metadata: None,
            substeps: Vec::new(),
        }));
        self.local_shells.insert(payload.id, id);
        self.running_tools.insert(id);
    }

    fn shell_finished(&mut self, payload: ShellFinished) {
        let Some(id) = self.local_shells.remove(&payload.id) else {
            return;
        };
        let failed = payload.error.is_some() || payload.exit_code != Some(0);
        self.update(id, |kind| {
            if let EntryKind::Tool(tool) = kind {
                tool.state = if failed {
                    ToolState::Failed
                } else {
                    ToolState::Succeeded
                };
                tool.duration_ns = Some(payload.duration_ns);
                tool.result = Some(serde_json::json!({
                    "output": payload.output,
                    "exit_code": payload.exit_code,
                    "truncated": payload.truncated,
                    "error": payload.error,
                }));
            }
        });
        self.running_tools.remove(&id);
    }

    fn apply_agent(&mut self, record: &TranscriptRecord) -> ModelChange {
        let previous_activity = self.transient.clone();
        let result = match record.kind() {
            "assistant.delta" => self.assistant_delta(record),
            "assistant.message" => self.assistant_message(record),
            "reasoning.summary.delta" => self.reasoning_delta(record),
            "run.started" => {
                self.active_runs = self.active_runs.saturating_add(1);
                self.run_started_at_unix_ms
                    .push_back(record.recorded_at_unix_ms());
                self.transient = Some(TransientStatus::Thinking);
                Ok(true)
            }
            "run.error" => self.decode_local::<RunError>(record).map(|payload| {
                self.pending_error = Some(payload.message.clone());
                self.transient = Some(TransientStatus::Error(payload.message));
                true
            }),
            "run.completed" => {
                self.complete_turn(record);
                Ok(true)
            }
            "run.failed" => {
                self.run_started_at_unix_ms.pop_front();
                self.finish_failed(None);
                Ok(true)
            }
            "tool.call" => self.tool_call(record),
            "tool.result" => self.tool_result(record),
            "model.warmup.started" => {
                self.transient = Some(TransientStatus::Warming);
                Ok(true)
            }
            "model.warmup.completed" => {
                self.transient = self.is_active().then_some(TransientStatus::Thinking);
                Ok(true)
            }
            "model.warmup.failed"
            | "model.call.failed"
            | "model.attempt.failed"
            | "model.connection.failed" => self.capture_error(record),
            "model.call.started" => {
                self.materialize_compaction_failure();
                self.transient = Some(TransientStatus::Thinking);
                Ok(true)
            }
            "model.call.completed" => {
                self.transient = self.is_active().then_some(TransientStatus::Thinking);
                Ok(true)
            }
            "model.compaction.started" => {
                self.transient = Some(TransientStatus::Compacting);
                Ok(true)
            }
            "model.compaction.completed" => self.compaction_completed(record),
            "model.compaction.failed" => self.compaction_failed(record),
            "model.attempt.retrying" => self.retrying(record),
            "model.connection.started" => self.connection_started(record),
            "model.connection.completed" => {
                self.transient = self.is_active().then_some(TransientStatus::Thinking);
                self.pending_error = None;
                Ok(true)
            }
            _ => Ok(false),
        };
        let activity_changed = previous_activity != self.transient;
        match result {
            Ok(changed) => ModelChange {
                changed: changed || activity_changed,
                ..ModelChange::default()
            },
            Err(error) => self.projection_error(
                record,
                error,
                visibility(record.source(), record.kind()) == EventVisibility::Persistent,
            ),
        }
    }

    fn assistant_delta(&mut self, record: &TranscriptRecord) -> Result<bool, serde_json::Error> {
        let payload = record.decode_payload::<AssistantDelta>()?;
        let phase = message_phase(payload.phase);
        let key = AssistantKey {
            call: payload.model_call_index,
            item: payload.item_id,
            phase,
        };
        let id = if let Some(&id) = self.assistants.get(&key) {
            id
        } else {
            let id = self.push(EntryKind::Assistant {
                text: String::new(),
                complete: false,
            });
            self.assistants.insert(key, id);
            self.active_assistants
                .insert((payload.model_call_index, phase), id);
            id
        };
        self.update(id, |kind| {
            if let EntryKind::Assistant { text, .. } = kind {
                text.push_str(&payload.text);
            }
        });
        self.transient = Some(TransientStatus::Responding);
        Ok(true)
    }

    fn assistant_message(&mut self, record: &TranscriptRecord) -> Result<bool, serde_json::Error> {
        let payload = record.decode_payload::<AssistantMessage>()?;
        let phase = message_phase(payload.phase);
        let key = AssistantKey {
            call: payload.model_call_index,
            item: payload.item_id,
            phase,
        };
        let id = self
            .assistants
            .get(&key)
            .copied()
            .or_else(|| {
                self.active_assistants
                    .get(&(payload.model_call_index, phase))
                    .copied()
            })
            .unwrap_or_else(|| {
                let id = self.push(EntryKind::Assistant {
                    text: String::new(),
                    complete: false,
                });
                self.assistants.insert(key, id);
                id
            });
        self.update(id, |kind| {
            if let EntryKind::Assistant { text, complete, .. } = kind {
                *text = payload.text;
                *complete = true;
            }
        });
        self.transient = self.is_active().then_some(TransientStatus::Thinking);
        Ok(true)
    }

    fn reasoning_delta(&mut self, record: &TranscriptRecord) -> Result<bool, serde_json::Error> {
        let payload = record.decode_payload::<ReasoningSummaryDelta>()?;
        let id = self
            .reasoning
            .get(&payload.model_call_index)
            .copied()
            .unwrap_or_else(|| {
                let id = self.push(EntryKind::Reasoning {
                    text: String::new(),
                });
                self.reasoning.insert(payload.model_call_index, id);
                id
            });
        self.update(id, |kind| {
            if let EntryKind::Reasoning { text } = kind {
                if text.ends_with("**") && payload.text.starts_with("**") {
                    text.push_str("  \n");
                }
                text.push_str(&payload.text);
            }
        });
        Ok(true)
    }

    fn tool_call(&mut self, record: &TranscriptRecord) -> Result<bool, serde_json::Error> {
        let ToolCallPayload {
            call_id,
            tool,
            arguments,
        } = record.decode_payload::<ToolCallPayload>()?;
        if tool == "write_stdin"
            && let Some(session_id) = arguments.get("session_id").and_then(Value::as_i64)
            && let Some(id) = self.shell_sessions.get(&session_id).copied()
        {
            let substep = arguments
                .get("chars")
                .and_then(Value::as_str)
                .filter(|chars| !chars.is_empty())
                .map_or_else(
                    || "polled process".to_owned(),
                    |chars| format!("sent {chars:?}"),
                );
            self.update(id, |kind| {
                if let EntryKind::Tool(tool) = kind {
                    tool.state = ToolState::Running;
                    tool.substeps.push(substep);
                }
            });
            self.tools.insert(call_id, id);
            self.running_tools.insert(id);
            self.transient = Some(TransientStatus::Tool("Shell".to_owned()));
            return Ok(true);
        }
        let hidden = tool == "wait";
        let transient = if hidden {
            TransientStatus::WaitingForBackgroundWork
        } else {
            self.hide_exec_parent(&call_id);
            TransientStatus::Tool(humanize_tool(&tool))
        };
        let id = self.push_with_visibility(
            EntryKind::Tool(ToolEntry {
                name: tool,
                arguments,
                started_at_unix_ms: record.recorded_at_unix_ms(),
                state: ToolState::Running,
                duration_ns: None,
                result: None,
                metadata: None,
                substeps: Vec::new(),
            }),
            hidden,
        );
        self.tools.insert(call_id, id);
        self.running_tools.insert(id);
        self.transient = Some(transient);
        Ok(true)
    }

    fn tool_result(&mut self, record: &TranscriptRecord) -> Result<bool, serde_json::Error> {
        let payload = record.decode_payload::<ToolResultPayload>()?;
        let shell_followup = payload.tool == "write_stdin";
        let result = normalize_result(payload.result);
        let state = tool_result_state(&payload.tool, &payload.status, &result);
        let shell_session = (payload.tool == "exec_command")
            .then(|| tool_session_id(&result))
            .flatten();
        let id = self
            .tools
            .get(&payload.call_id)
            .copied()
            .unwrap_or_else(|| {
                let id = self.push(EntryKind::Tool(ToolEntry {
                    name: payload.tool.clone(),
                    arguments: Value::Null,
                    started_at_unix_ms: record.recorded_at_unix_ms(),
                    state: ToolState::Running,
                    duration_ns: None,
                    result: None,
                    metadata: None,
                    substeps: Vec::new(),
                }));
                self.tools.insert(payload.call_id.clone(), id);
                id
            });
        self.update(id, |kind| {
            if let EntryKind::Tool(tool) = kind {
                tool.state = state;
                tool.duration_ns = Some(if shell_followup {
                    elapsed_nanoseconds(tool.started_at_unix_ms, record.recorded_at_unix_ms())
                        .max(payload.duration_ns)
                } else {
                    payload.duration_ns
                });
                tool.result = Some(if shell_followup {
                    merge_shell_result(tool.result.take(), result)
                } else {
                    result
                });
                tool.metadata = payload.metadata;
                if tool.name == "wait" && state == ToolState::Succeeded {
                    // Successful wait wrappers do not add historical noise.
                }
            }
        });
        if payload.tool == "wait"
            && state == ToolState::Failed
            && let Some(index) = self.index_of(id)
        {
            self.entries[index].hidden = false;
        }
        if state == ToolState::Running {
            if let Some(session_id) = shell_session {
                self.shell_sessions.insert(session_id, id);
            }
            self.running_tools.insert(id);
        } else {
            self.shell_sessions
                .retain(|_, shell_entry| *shell_entry != id);
            self.running_tools.remove(&id);
        }
        self.transient = self.is_active().then_some(TransientStatus::Thinking);
        Ok(true)
    }

    fn compaction_completed(
        &mut self,
        record: &TranscriptRecord,
    ) -> Result<bool, serde_json::Error> {
        let payload = record.decode_payload::<CompactionCompleted>()?;
        self.push(EntryKind::ContextCompacted {
            duration_ns: payload.duration_ns,
        });
        self.transient = self.is_active().then_some(TransientStatus::Thinking);
        Ok(true)
    }

    fn compaction_failed(&mut self, record: &TranscriptRecord) -> Result<bool, serde_json::Error> {
        let payload = record.decode_payload::<CompactionFailed>()?;
        self.pending_compaction_error = Some(payload.error.clone());
        self.pending_error = Some(payload.error);
        self.transient = self.is_active().then_some(TransientStatus::Thinking);
        Ok(true)
    }

    fn retrying(&mut self, record: &TranscriptRecord) -> Result<bool, serde_json::Error> {
        let payload = record.decode_payload::<RetryPayload>()?;
        let delay = format_duration(payload.delay_ns);
        self.pending_error = Some(payload.error);
        self.transient = Some(TransientStatus::Retrying(delay));
        Ok(true)
    }

    fn connection_started(&mut self, record: &TranscriptRecord) -> Result<bool, serde_json::Error> {
        let payload = record.decode_payload::<ConnectionPayload>()?;
        self.transient = Some(if payload.purpose == "reconnect" {
            TransientStatus::Reconnecting
        } else {
            TransientStatus::Connecting
        });
        Ok(true)
    }

    fn capture_error(&mut self, record: &TranscriptRecord) -> Result<bool, serde_json::Error> {
        let payload = record.decode_payload::<ErrorPayload>()?;
        self.pending_error = Some(payload.error);
        Ok(false)
    }

    fn finish_success(&mut self) {
        self.materialize_compaction_failure();
        self.finish_activity();
        self.pending_error = None;
    }

    fn complete_turn(&mut self, record: &TranscriptRecord) {
        let payload_duration_ns = record
            .decode_payload::<RunDurationPayload>()
            .ok()
            .and_then(|payload| payload.duration_ns);
        let recorded_duration_ns = self.run_started_at_unix_ms.pop_front().map(|started_at| {
            record
                .recorded_at_unix_ms()
                .saturating_sub(started_at)
                .saturating_mul(1_000_000)
        });
        let duration_ns = payload_duration_ns.or(recorded_duration_ns);
        self.finish_success();
        let Some(duration_ns) = duration_ns else {
            return;
        };
        self.push(EntryKind::TurnCompleted { duration_ns });
    }

    fn finish_failed(&mut self, error: Option<String>) {
        self.pending_compaction_error = None;
        if error.is_none()
            && self.pending_error.is_none()
            && self
                .entries
                .last()
                .is_some_and(|entry| matches!(entry.kind, EntryKind::Error { .. }))
        {
            self.finish_activity();
            return;
        }
        let message = error
            .or_else(|| self.pending_error.take())
            .unwrap_or_else(|| "The agent run failed".to_owned());
        if !self.entries.last().is_some_and(|entry| {
            matches!(&entry.kind, EntryKind::Error { message: existing } if existing == &message)
        }) {
            self.push(EntryKind::Error { message });
        }
        self.finish_activity();
    }

    fn finish_activity(&mut self) {
        self.active_runs = self.active_runs.saturating_sub(1);
        if self.active_runs == 0 {
            self.fail_orphaned_tools();
        }
        self.transient = self.is_active().then_some(TransientStatus::Thinking);
    }

    fn fail_orphaned_tools(&mut self) {
        let local_shells = self.local_shells.values().copied().collect::<HashSet<_>>();
        let orphaned = self
            .running_tools
            .iter()
            .copied()
            .filter(|id| !local_shells.contains(id))
            .collect::<Vec<_>>();
        for id in &orphaned {
            self.update(*id, |kind| {
                let EntryKind::Tool(tool) = kind else {
                    return;
                };
                tool.state = ToolState::Failed;
                let result = tool.result.get_or_insert_with(|| serde_json::json!({}));
                if let Value::Object(result) = result
                    && result.get("error").is_none_or(Value::is_null)
                {
                    result.insert(
                        "error".to_owned(),
                        Value::String("tool call ended without a terminal result".to_owned()),
                    );
                }
            });
            self.running_tools.remove(id);
        }
        self.shell_sessions.retain(|_, id| !orphaned.contains(id));
    }

    pub(crate) fn agent_stream_closed(&mut self) -> bool {
        let changed = self.active_runs > 0
            || self.running_tools.iter().any(|id| {
                !self
                    .local_shells
                    .values()
                    .any(|local_shell| local_shell == id)
            });
        self.active_runs = 0;
        self.run_started_at_unix_ms.clear();
        self.fail_orphaned_tools();
        self.transient = self.is_active().then_some(TransientStatus::Thinking);
        changed
    }

    fn materialize_compaction_failure(&mut self) {
        let Some(message) = self.pending_compaction_error.take() else {
            return;
        };
        self.push(EntryKind::ContextCompactionFailed { message });
    }

    fn hide_exec_parent(&mut self, call_id: &str) {
        let parent = call_id
            .match_indices('/')
            .rev()
            .find_map(|(separator, _)| self.tools.get(&call_id[..separator]).copied());
        let Some(parent) = parent else {
            return;
        };
        let Some(index) = self.index_of(parent) else {
            return;
        };
        let EntryKind::Tool(tool) = &self.entries[index].kind else {
            return;
        };
        if tool.name == "exec" {
            self.entries[index].hidden = true;
            self.entries[index].revision = self.entries[index].revision.saturating_add(1);
        }
    }

    fn projection_error(
        &mut self,
        record: &TranscriptRecord,
        error: serde_json::Error,
        visible: bool,
    ) -> ModelChange {
        let message = format!("Could not render {}: {error}", record.kind());
        if visible {
            self.push(EntryKind::Error { message });
        } else {
            self.pending_error = Some(message);
        }
        ModelChange {
            changed: visible,
            ..ModelChange::default()
        }
    }

    fn decode_local<T: serde::de::DeserializeOwned>(
        &self,
        record: &TranscriptRecord,
    ) -> Result<T, serde_json::Error> {
        record.decode_payload()
    }

    fn push(&mut self, kind: EntryKind) -> EntryId {
        self.push_with_visibility(kind, false)
    }

    fn push_with_visibility(&mut self, kind: EntryKind, hidden: bool) -> EntryId {
        let id = EntryId::from_index(self.next_entry_id);
        self.next_entry_id = self.next_entry_id.saturating_add(1);
        self.entry_indices.insert(id, self.entries.len());
        self.entries.push(TranscriptEntry {
            id,
            revision: 1,
            kind,
            hidden,
        });
        id
    }

    fn trim_message_history(&mut self) -> Option<EntryId> {
        if self.message_order.len() <= MAX_RETAINED_MESSAGE_THREADS {
            return None;
        }
        let position = self.message_order.iter().position(|thread_id| {
            let Some(id) = self.message_threads.get(thread_id) else {
                return true;
            };
            let Some(entry) = self.entry(*id) else {
                return true;
            };
            let EntryKind::DirectedMessage(message) = &entry.kind else {
                return true;
            };
            !message.deliveries.iter().any(|delivery| {
                matches!(
                    delivery.state,
                    MessageDeliveryState::Admitted {
                        disposition: MessageDisposition::Queued
                    }
                )
            })
        })?;
        let thread_id = self
            .message_order
            .remove(position)
            .expect("the retained message thread should still exist");
        let id = self.message_threads.remove(&thread_id)?;
        let removed_index = self.entry_indices.remove(&id)?;
        self.entries.remove(removed_index);
        for (index, entry) in self.entries.iter().enumerate().skip(removed_index) {
            self.entry_indices.insert(entry.id, index);
        }
        Some(id)
    }

    fn update(&mut self, id: EntryId, update: impl FnOnce(&mut EntryKind)) {
        let Some(index) = self.index_of(id) else {
            return;
        };
        update(&mut self.entries[index].kind);
        self.entries[index].revision = self.entries[index].revision.saturating_add(1);
    }
}

fn delivery_advances(current: &MessageDeliveryState, next: &MessageDeliveryState) -> bool {
    current != next && matches!(current, MessageDeliveryState::Admitted { .. })
}

fn message_phase(phase: Option<AgentMessagePhase>) -> MessagePhase {
    match phase {
        Some(AgentMessagePhase::Commentary) => MessagePhase::Commentary,
        Some(AgentMessagePhase::FinalAnswer) | None => MessagePhase::Final,
    }
}

fn visibility(source: &str, kind: &str) -> EventVisibility {
    if source == "tact" {
        return match kind {
            "user.submitted"
            | "worker.turns_interrupted"
            | "effort.changed"
            | "fast_mode.changed" => EventVisibility::Persistent,
            "worker.turn_finished" | "worker.stopped" | "session.ended" => {
                EventVisibility::ErrorFallback
            }
            _ => EventVisibility::StateOnly,
        };
    }
    match kind {
        "assistant.delta"
        | "assistant.message"
        | "reasoning.summary.delta"
        | "tool.call"
        | "tool.result"
        | "model.compaction.completed"
        | "model.compaction.failed" => EventVisibility::Persistent,
        "run.started"
        | "model.warmup.started"
        | "model.call.started"
        | "model.compaction.started"
        | "model.attempt.retrying"
        | "model.connection.started" => EventVisibility::Transient,
        "run.error"
        | "run.failed"
        | "model.warmup.failed"
        | "model.call.failed"
        | "model.attempt.failed"
        | "model.connection.failed" => EventVisibility::ErrorFallback,
        _ => EventVisibility::StateOnly,
    }
}

fn tool_session_id(result: &Value) -> Option<i64> {
    if let Value::String(text) = result {
        let decoded = serde_json::from_str::<Value>(text).ok()?;
        return decoded.get("session_id").and_then(Value::as_i64);
    }
    result.get("session_id").and_then(Value::as_i64)
}

fn tool_result_state(tool: &str, status: &str, result: &Value) -> ToolState {
    if !matches!(status, "success" | "completed") {
        return ToolState::Failed;
    }
    if !matches!(tool, "exec_command" | "write_stdin") {
        return ToolState::Succeeded;
    }
    if result.get("error").is_some_and(|error| !error.is_null()) {
        return ToolState::Failed;
    }
    if let Some(exit_code) = result.get("exit_code").and_then(Value::as_i64) {
        return if exit_code == 0 {
            ToolState::Succeeded
        } else {
            ToolState::Failed
        };
    }
    if tool_session_id(result).is_some() && result.get("exit_code").is_none() {
        return ToolState::Running;
    }
    ToolState::Failed
}

fn elapsed_nanoseconds(started_at_unix_ms: u64, finished_at_unix_ms: u64) -> u64 {
    finished_at_unix_ms
        .saturating_sub(started_at_unix_ms)
        .saturating_mul(1_000_000)
}

fn normalize_result(result: Value) -> Value {
    let Value::String(encoded) = result else {
        return result;
    };
    serde_json::from_str(&encoded).unwrap_or(Value::String(encoded))
}

fn merge_shell_result(current: Option<Value>, next: Value) -> Value {
    let Some(Value::Object(mut current)) = current else {
        return next;
    };
    let mut next = match next {
        Value::Object(next) => next,
        other => return other,
    };
    let previous_output = current
        .remove("output")
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default();
    if let Some(Value::String(output)) = next.get_mut("output") {
        output.insert_str(0, &previous_output);
    }
    Value::Object(next)
}

#[derive(Deserialize)]
struct UserSubmitted {
    text: String,
}

#[derive(Deserialize)]
struct UserSteered {
    text: String,
}

#[derive(Deserialize)]
struct ShellStarted {
    id: ShellId,
    command: String,
    workspace: PathBuf,
}

#[derive(Deserialize)]
struct ShellFinished {
    id: ShellId,
    output: String,
    exit_code: Option<i32>,
    duration_ns: u64,
    truncated: bool,
    error: Option<String>,
}

#[derive(Deserialize)]
struct EffortChanged {
    to: ReasoningEffort,
}

#[derive(Deserialize)]
struct FastModeChanged {
    to: bool,
}

#[derive(Deserialize)]
struct WorkerTurnFinished {
    error: Option<String>,
}

#[derive(Deserialize)]
struct WorkerTurnsInterrupted {
    count: usize,
    error: Option<String>,
}

#[derive(Deserialize)]
struct WorkerSteerFailed {
    error: String,
}

#[derive(Deserialize)]
struct WorkerStopped {
    error: Option<String>,
}

#[derive(Deserialize)]
struct SessionEnded {
    outcome: String,
    error: Option<String>,
}

#[derive(Deserialize)]
struct ToolCallPayload {
    call_id: String,
    tool: String,
    arguments: Value,
}

#[derive(Deserialize)]
struct ToolResultPayload {
    call_id: String,
    tool: String,
    status: String,
    duration_ns: u64,
    result: Value,
    metadata: Option<Value>,
}

#[derive(Deserialize)]
struct RunDurationPayload {
    #[serde(default)]
    duration_ns: Option<u64>,
}

#[derive(Deserialize)]
struct ErrorPayload {
    error: String,
}

#[derive(Deserialize)]
struct RetryPayload {
    delay_ns: u64,
    error: String,
}

#[derive(Deserialize)]
struct ConnectionPayload {
    purpose: String,
}

#[cfg(test)]
mod tests {
    use super::{
        EntryKind, EventVisibility, MAX_RETAINED_MESSAGE_THREADS, ToolState, TranscriptModel,
        merge_shell_result, visibility,
    };
    use crate::{
        app::config::ReasoningEffort,
        core::extensions::subagents::{AgentId, AgentMessageUpdate, MessageSender, ThreadId},
        tui::transcript::{
            LocalEvent, SessionEnded, SessionOutcome, ShellId, TranscriptRecord, TurnId,
        },
    };
    use nanocodex::agent::events::{AgentEvent, AgentEventKind};
    use serde::Serialize;
    use serde_json::{json, value::to_raw_value};
    use std::sync::Arc;

    fn agent(kind: AgentEventKind, payload: impl Serialize) -> TranscriptRecord {
        agent_at(1, kind, payload)
    }

    fn agent_at(
        recorded_at_unix_ms: u64,
        kind: AgentEventKind,
        payload: impl Serialize,
    ) -> TranscriptRecord {
        TranscriptRecord::from_agent(
            1,
            recorded_at_unix_ms,
            AgentEvent {
                protocol_version: 1,
                request_id: Arc::from("session"),
                seq: 1,
                kind,
                payload: to_raw_value(&payload).unwrap().into(),
            },
        )
    }

    fn message_update(state: &str) -> AgentMessageUpdate {
        message_update_with_id(1, state)
    }

    fn message_update_with_id(id: u64, state: &str) -> AgentMessageUpdate {
        serde_json::from_value(json!({
            "message_id": id,
            "thread": {
                "id": id,
                "participants": [
                    {"kind": "agent", "agent_id": 1},
                    {"kind": "agent", "agent_id": 2}
                ],
                "messages": [{
                    "id": id,
                    "thread_id": id,
                    "from": {"kind": "agent", "agent_id": 1},
                    "to": 2,
                    "priority": "deferred",
                    "purpose": "coordinate",
                    "body": "status update"
                }]
            },
            "delivery": {
                "state": state,
                "disposition": "queued"
            }
        }))
        .unwrap()
    }

    #[test]
    fn message_delivery_updates_upsert_one_entry_per_thread() {
        let mut model = TranscriptModel::default();
        let perspective = MessageSender::Agent {
            agent_id: AgentId::new(1),
        };
        let admitted = message_update("admitted");

        assert!(model.apply_message(perspective, admitted.clone()).changed);
        assert!(!model.apply_message(perspective, admitted).changed);
        assert!(
            model
                .apply_message(perspective, message_update("delivered"))
                .changed
        );
        assert!(
            !model
                .apply_message(perspective, message_update("admitted"))
                .changed
        );

        assert_eq!(model.entries().len(), 1);
        let EntryKind::DirectedMessage(thread) = &model.entries()[0].kind else {
            panic!("message update should create a directed-message entry");
        };
        assert_eq!(thread.deliveries.len(), 1);
        assert!(matches!(
            thread.deliveries[0].state,
            crate::core::extensions::subagents::MessageDeliveryState::Delivered { .. }
        ));

        let mut snapshot = model.fork_snapshot();
        assert!(
            !snapshot
                .apply_message(perspective, message_update("delivered"))
                .changed
        );
        assert_eq!(snapshot.entries().len(), 1);
    }

    #[test]
    fn completed_turn_records_the_full_run_duration() {
        let mut model = TranscriptModel::default();
        model.apply(&agent_at(1_000, AgentEventKind::RunStarted, json!({})));
        model.apply(&agent_at(
            66_432,
            AgentEventKind::RunCompleted,
            json!({"duration_ns": 70_123_000_000_u64}),
        ));

        assert!(matches!(
            model.entries().last().map(|entry| &entry.kind),
            Some(EntryKind::TurnCompleted {
                duration_ns: 70_123_000_000
            })
        ));
    }

    #[test]
    fn interrupting_without_active_turns_is_a_no_op() {
        let mut model = TranscriptModel::default();
        let record = TranscriptRecord::from_local(
            1,
            1,
            LocalEvent::WorkerTurnsInterrupted {
                count: 0,
                error: None,
            },
        )
        .unwrap();

        let update = model.apply(&record);

        assert!(!update.changed);
        assert!(model.entries().is_empty());
    }

    #[test]
    fn failed_turn_does_not_claim_completion() {
        let mut model = TranscriptModel::default();
        model.apply(&agent_at(1_000, AgentEventKind::RunStarted, json!({})));
        model.apply(&agent_at(2_000, AgentEventKind::RunFailed, json!({})));

        assert!(
            !model
                .entries()
                .iter()
                .any(|entry| matches!(entry.kind, EntryKind::TurnCompleted { .. }))
        );
    }

    #[test]
    fn malformed_completion_timing_does_not_leave_the_run_active() {
        let mut model = TranscriptModel::default();
        model.apply(&agent_at(1_000, AgentEventKind::RunStarted, json!({})));
        model.apply(&agent_at(
            2_000,
            AgentEventKind::RunCompleted,
            json!("invalid"),
        ));

        assert!(!model.is_active());

        model.apply(&agent_at(3_000, AgentEventKind::RunStarted, json!({})));
        model.apply(&agent_at(
            4_000,
            AgentEventKind::RunCompleted,
            json!({"duration_ns": 1_000_000_000_u64}),
        ));
        assert!(!model.is_active());
        assert!(matches!(
            model.entries().last().map(|entry| &entry.kind),
            Some(EntryKind::TurnCompleted {
                duration_ns: 1_000_000_000
            })
        ));
    }

    #[test]
    fn completed_directed_message_history_is_bounded() {
        let mut model = TranscriptModel::default();
        let perspective = MessageSender::Agent {
            agent_id: AgentId::new(1),
        };

        for id in 1..=u64::try_from(MAX_RETAINED_MESSAGE_THREADS + 1).unwrap() {
            assert!(
                model
                    .apply_message(perspective, message_update_with_id(id, "delivered"))
                    .changed
            );
        }

        assert_eq!(model.entries().len(), MAX_RETAINED_MESSAGE_THREADS);
        assert_eq!(model.message_threads.len(), MAX_RETAINED_MESSAGE_THREADS);
        assert!(model.entries().iter().all(|entry| {
            matches!(
                &entry.kind,
                EntryKind::DirectedMessage(message) if message.thread.id != ThreadId::new(1)
            )
        }));
    }

    #[test]
    fn successful_internal_events_are_deliberately_omitted() {
        let state_only = [
            "api.event",
            "run.steered",
            "run.completed",
            "model.warmup.completed",
            "model.call.completed",
            "model.attempt.started",
            "model.connection.completed",
        ];
        for kind in state_only {
            assert_eq!(
                visibility("agent", kind),
                EventVisibility::StateOnly,
                "{kind}"
            );
        }
    }

    #[test]
    fn every_stable_agent_event_has_an_explicit_visibility() {
        let cases = [
            ("api.event", EventVisibility::StateOnly),
            ("assistant.delta", EventVisibility::Persistent),
            ("assistant.message", EventVisibility::Persistent),
            ("reasoning.summary.delta", EventVisibility::Persistent),
            ("run.started", EventVisibility::Transient),
            ("run.steered", EventVisibility::StateOnly),
            ("run.error", EventVisibility::ErrorFallback),
            ("run.completed", EventVisibility::StateOnly),
            ("run.failed", EventVisibility::ErrorFallback),
            ("tool.call", EventVisibility::Persistent),
            ("tool.result", EventVisibility::Persistent),
            ("model.warmup.started", EventVisibility::Transient),
            ("model.warmup.completed", EventVisibility::StateOnly),
            ("model.warmup.failed", EventVisibility::ErrorFallback),
            ("model.call.started", EventVisibility::Transient),
            ("model.call.completed", EventVisibility::StateOnly),
            ("model.call.failed", EventVisibility::ErrorFallback),
            ("model.compaction.started", EventVisibility::Transient),
            ("model.compaction.completed", EventVisibility::Persistent),
            ("model.compaction.failed", EventVisibility::Persistent),
            ("model.attempt.started", EventVisibility::StateOnly),
            ("model.attempt.failed", EventVisibility::ErrorFallback),
            ("model.attempt.retrying", EventVisibility::Transient),
            ("model.connection.started", EventVisibility::Transient),
            ("model.connection.completed", EventVisibility::StateOnly),
            ("model.connection.failed", EventVisibility::ErrorFallback),
        ];

        assert_eq!(cases.len(), 26);
        for (kind, expected) in cases {
            assert_eq!(visibility("agent", kind), expected, "{kind}");
        }
    }

    #[test]
    fn canonical_message_replaces_streamed_deltas() {
        let mut model = TranscriptModel::default();
        model.apply(&agent(
            AgentEventKind::AssistantDelta,
            json!({"model_call_index": 1, "item_id": "a", "phase": "final_answer", "text": "hel"}),
        ));
        model.apply(&agent(
            AgentEventKind::AssistantMessage,
            json!({"model_call_index": 1, "item_id": "a", "phase": "final_answer", "text": "hello"}),
        ));

        assert_eq!(model.entries().len(), 1);
        assert!(matches!(
            &model.entries()[0].kind,
            EntryKind::Assistant { text, complete: true, .. } if text == "hello"
        ));
    }

    #[test]
    fn ordinary_reasoning_deltas_remain_one_streamed_step() {
        let mut model = TranscriptModel::default();
        model.apply(&agent(
            AgentEventKind::ReasoningSummaryDelta,
            json!({"model_call_index": 1, "text": "Inspecting the request"}),
        ));
        model.apply(&agent(
            AgentEventKind::ReasoningSummaryDelta,
            json!({"model_call_index": 1, "text": " and event ordering"}),
        ));

        assert!(matches!(
            &model.entries()[0].kind,
            EntryKind::Reasoning { text } if text == "Inspecting the request and event ordering"
        ));
    }

    #[test]
    fn canonical_compaction_events_use_typed_projection() {
        let mut model = TranscriptModel::default();
        model.apply(&agent(AgentEventKind::RunStarted, json!({})));
        model.apply(&agent(
            AgentEventKind::ModelCompactionCompleted,
            json!({
                "after_model_call_index": 1,
                "attempt": 1,
                "connection_generation": 1,
                "status": "completed",
                "duration_ns": 42,
                "time_to_first_event_ns": 10,
                "time_to_first_output_ns": 20,
                "usage": null
            }),
        ));

        assert!(matches!(
            model.entries().last().map(|entry| &entry.kind),
            Some(EntryKind::ContextCompacted { duration_ns: 42 })
        ));
    }

    #[test]
    fn recovered_retry_leaves_no_persistent_entry() {
        let mut model = TranscriptModel::default();
        model.apply(&agent(AgentEventKind::RunStarted, json!({})));
        model.apply(&agent(
            AgentEventKind::ModelAttemptRetrying,
            json!({"delay_ns": 500_000_000, "error": "temporary"}),
        ));
        assert!(model.transient().is_some());
        model.apply(&agent(AgentEventKind::ModelConnectionCompleted, json!({})));

        assert!(model.entries().is_empty());
        assert!(model.transient().is_some());
    }

    #[test]
    fn run_failure_adds_one_best_error() {
        let mut model = TranscriptModel::default();
        model.apply(&agent(
            AgentEventKind::RunError,
            json!({"message": "service unavailable"}),
        ));
        model.apply(&agent(AgentEventKind::RunFailed, json!({})));
        model.apply(&agent(AgentEventKind::RunFailed, json!({})));

        assert_eq!(
            model
                .entries()
                .iter()
                .filter(|entry| matches!(entry.kind, EntryKind::Error { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn closing_a_session_fails_tools_without_terminal_results() {
        let mut model = TranscriptModel::default();
        model.apply(&agent(AgentEventKind::RunStarted, json!({})));
        model.apply(&agent(
            AgentEventKind::ToolCall,
            json!({"call_id": "shell-1", "tool": "exec_command", "arguments": {"cmd": "sleep 5"}}),
        ));
        model.apply(
            &TranscriptRecord::from_local(
                2,
                2,
                LocalEvent::SessionEnded(SessionEnded {
                    outcome: SessionOutcome::Closed,
                    error: None,
                }),
            )
            .unwrap(),
        );

        assert!(!model.has_running_tools());
        assert!(matches!(
            &model.entries()[0].kind,
            EntryKind::Tool(tool) if tool.state == ToolState::Failed
        ));
    }

    #[test]
    fn local_user_event_is_persistent() {
        let mut model = TranscriptModel::default();
        let record = TranscriptRecord::from_local(
            1,
            1,
            LocalEvent::UserSubmitted {
                id: TurnId::new(3),
                text: "hello".to_owned(),
            },
        )
        .unwrap();
        model.apply(&record);

        assert!(
            matches!(&model.entries()[0].kind, EntryKind::User { text, .. } if text == "hello")
        );
    }

    #[test]
    fn applied_session_settings_are_visible_transcript_entries() {
        let mut model = TranscriptModel::default();
        model.apply(
            &TranscriptRecord::from_local(
                1,
                1,
                LocalEvent::EffortChanged {
                    from: ReasoningEffort::Medium,
                    to: ReasoningEffort::High,
                },
            )
            .unwrap(),
        );
        model.apply(
            &TranscriptRecord::from_local(
                2,
                2,
                LocalEvent::FastModeChanged {
                    from: false,
                    to: true,
                },
            )
            .unwrap(),
        );

        assert!(matches!(
            model.entries()[0].kind,
            EntryKind::EffortChanged {
                to: ReasoningEffort::High
            }
        ));
        assert!(matches!(
            model.entries()[1].kind,
            EntryKind::FastModeChanged { enabled: true }
        ));
    }

    #[test]
    fn fork_snapshot_keeps_stable_history_and_drops_the_active_turn() {
        let mut model = TranscriptModel::default();
        for (sequence, text) in [(1, "completed"), (2, "still running")] {
            model.apply(
                &TranscriptRecord::from_local(
                    sequence,
                    sequence,
                    LocalEvent::UserSubmitted {
                        id: TurnId::new(sequence),
                        text: text.to_owned(),
                    },
                )
                .unwrap(),
            );
        }
        model.apply(&agent(AgentEventKind::RunStarted, json!({})));

        let snapshot = model.fork_snapshot();

        assert_eq!(snapshot.entries().len(), 1);
        assert!(matches!(
            &snapshot.entries()[0].kind,
            EntryKind::User { text } if text == "completed"
        ));
        assert!(!snapshot.is_active());
        assert!(!snapshot.has_running_tools());
        assert!(snapshot.transient().is_none());
    }

    #[test]
    fn local_shell_lifecycle_projects_to_one_tool_entry() {
        let mut model = TranscriptModel::default();
        let started = TranscriptRecord::from_local(
            1,
            1,
            LocalEvent::ShellStarted {
                id: ShellId::new(7),
                command: "printf hello".to_owned(),
                workspace: "/work".into(),
            },
        )
        .unwrap();
        let finished = TranscriptRecord::from_local(
            2,
            2,
            LocalEvent::ShellFinished {
                id: ShellId::new(7),
                output: "hello".to_owned(),
                exit_code: Some(0),
                duration_ns: 10,
                truncated: false,
                error: None,
            },
        )
        .unwrap();

        model.apply(&started);
        assert!(model.has_running_tools());
        model.apply(&finished);

        assert!(!model.has_running_tools());
        assert!(matches!(
            &model.entries()[0].kind,
            EntryKind::Tool(tool)
                if tool.state == ToolState::Succeeded
                    && tool.result.as_ref().and_then(|value| value["output"].as_str())
                        == Some("hello")
        ));
    }

    #[test]
    fn local_shell_without_an_exit_code_is_failed() {
        let mut model = TranscriptModel::default();
        for record in [
            TranscriptRecord::from_local(
                1,
                1,
                LocalEvent::ShellStarted {
                    id: ShellId::new(7),
                    command: "sleep 100".to_owned(),
                    workspace: "/work".into(),
                },
            )
            .unwrap(),
            TranscriptRecord::from_local(
                2,
                2,
                LocalEvent::ShellFinished {
                    id: ShellId::new(7),
                    output: String::new(),
                    exit_code: None,
                    duration_ns: 10,
                    truncated: false,
                    error: None,
                },
            )
            .unwrap(),
        ] {
            model.apply(&record);
        }

        assert!(!model.has_running_tools());
        assert!(matches!(
            &model.entries()[0].kind,
            EntryKind::Tool(tool) if tool.state == ToolState::Failed
        ));
    }

    #[test]
    fn applied_steer_becomes_a_user_entry_only_when_persisted() {
        let mut model = TranscriptModel::default();
        let record = TranscriptRecord::from_local(
            1,
            1,
            LocalEvent::UserSteered {
                text: "narrow the scope".to_owned(),
            },
        )
        .unwrap();

        model.apply(&record);

        assert!(matches!(
            &model.entries()[0].kind,
            EntryKind::User { text, .. } if text == "narrow the scope"
        ));
    }

    #[test]
    fn activity_tracks_all_concurrent_runs() {
        let mut model = TranscriptModel::default();
        model.apply(&agent(AgentEventKind::RunStarted, json!({})));
        model.apply(&agent(AgentEventKind::RunStarted, json!({})));

        model.apply(&agent(AgentEventKind::RunCompleted, json!({})));
        assert!(model.is_active());

        model.apply(&agent(AgentEventKind::RunCompleted, json!({})));
        assert!(!model.is_active());
    }

    #[test]
    fn assistant_delta_uses_a_streaming_status() {
        let mut model = TranscriptModel::default();
        model.apply(&agent(AgentEventKind::RunStarted, json!({})));

        model.apply(&agent(
            AgentEventKind::AssistantDelta,
            json!({"model_call_index": 1, "text": "hello"}),
        ));

        assert_eq!(model.transient(), Some(&super::TransientStatus::Responding));
    }

    #[test]
    fn shell_followups_preserve_output_and_replace_process_state() {
        let merged = merge_shell_result(
            Some(json!({
                "output": "first ",
                "session_id": 7,
                "wall_time_seconds": 1.0,
            })),
            json!({
                "output": "second",
                "exit_code": 0,
                "wall_time_seconds": 2.0,
            }),
        );

        assert_eq!(merged["output"], "first second");
        assert_eq!(merged["exit_code"], 0);
        assert_eq!(merged["wall_time_seconds"], 2.0);
        assert!(merged.get("session_id").is_none());
    }

    #[test]
    fn yielded_shell_sessions_remain_running_until_they_exit() {
        let mut model = TranscriptModel::default();
        model.apply(&agent_at(
            1_000,
            AgentEventKind::ToolCall,
            json!({
                "call_id": "shell",
                "tool": "exec_command",
                "arguments": {"cmd": "cargo test"},
            }),
        ));
        model.apply(&agent_at(
            2_000,
            AgentEventKind::ToolResult,
            json!({
                "call_id": "shell",
                "tool": "exec_command",
                "status": "completed",
                "duration_ns": 1_u64,
                "result": {"output": "running", "session_id": 7},
                "metadata": null,
            }),
        ));

        assert!(matches!(
            &model.entries()[0].kind,
            EntryKind::Tool(tool) if tool.state == ToolState::Running
        ));

        model.apply(&agent_at(
            3_000,
            AgentEventKind::ToolCall,
            json!({
                "call_id": "stdin",
                "tool": "write_stdin",
                "arguments": {"session_id": 7},
            }),
        ));
        model.apply(&agent_at(
            4_500,
            AgentEventKind::ToolResult,
            json!({
                "call_id": "stdin",
                "tool": "write_stdin",
                "status": "completed",
                "duration_ns": 2_u64,
                "result": {"output": " done", "exit_code": 0},
                "metadata": null,
            }),
        ));

        assert!(matches!(
            &model.entries()[0].kind,
            EntryKind::Tool(tool)
                if tool.state == ToolState::Succeeded
                    && tool.duration_ns == Some(3_500_000_000)
                    && tool.result.as_ref().and_then(|result| result.get("output"))
                        == Some(&json!("running done"))
        ));
    }

    #[test]
    fn killed_shell_result_resolves_as_failed() {
        let mut model = TranscriptModel::default();
        model.apply(&agent(
            AgentEventKind::ToolCall,
            json!({
                "call_id": "shell",
                "tool": "exec_command",
                "arguments": {"cmd": "sleep 100"},
            }),
        ));
        model.apply(&agent(
            AgentEventKind::ToolResult,
            json!({
                "call_id": "shell",
                "tool": "exec_command",
                "status": "completed",
                "duration_ns": 1_u64,
                "result": {"output": "", "exit_code": null},
                "metadata": null,
            }),
        ));

        assert!(!model.has_running_tools());
        assert!(matches!(
            &model.entries()[0].kind,
            EntryKind::Tool(tool) if tool.state == ToolState::Failed
        ));
    }

    #[test]
    fn ending_a_run_fails_tools_missing_their_terminal_result() {
        let mut model = TranscriptModel::default();
        model.apply(&agent(AgentEventKind::RunStarted, json!({})));
        model.apply(&agent(
            AgentEventKind::ToolCall,
            json!({
                "call_id": "shell",
                "tool": "exec_command",
                "arguments": {"cmd": "sleep 100"},
            }),
        ));

        model.apply(&agent(AgentEventKind::RunCompleted, json!({})));

        assert!(!model.has_running_tools());
        assert!(matches!(
            &model.entries()[0].kind,
            EntryKind::Tool(tool)
                if tool.state == ToolState::Failed
                    && tool.result.as_ref().and_then(|result| result["error"].as_str())
                        == Some("tool call ended without a terminal result")
        ));
    }
}
