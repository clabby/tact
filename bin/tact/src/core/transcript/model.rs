use super::{
    CompactionFinished, DirectedMessageEntry, EffortChanged, EntryId, EntryKind, FastModeChanged,
    LocalKind, MessageDelivery, MessagePhase, ReflectionStarted, SessionEnded, SessionOutcome,
    SessionStarted, ShellFinished, ShellId, ShellStarted, SpeedChanged, ToolEntry, ToolState,
    TranscriptEntry, TranscriptRecord, TransientStatus, UserImage, UserSteered, UserSubmitted,
    WorkerSteerFailed, WorkerStopped, WorkerTurnFinished, WorkerTurnsInterrupted,
};
use nanocodex::{
    agent::events::{
        AgentEventKind, AssistantDelta, AssistantMessage, CompactionCompleted, CompactionFailed,
        ReasoningSummaryDelta, RunError,
    },
    oai::{PromptInput, UserInput, responses::MessagePhase as AgentMessagePhase},
};
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
};
use tact_subagents::{
    AgentMessageUpdate, MessageDeliveryState, MessageDisposition, MessageSender, ThreadId,
};

const MAX_RETAINED_MESSAGE_THREADS: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EventVisibility {
    Persistent,
    Transient,
    StateOnly,
    ErrorFallback,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CodeCellTerminal {
    Completed,
    Terminated,
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
    active_assistants: HashMap<(ModelCallKey, MessagePhase), EntryId>,
    reasoning: HashMap<ModelCallKey, EntryId>,
    tools: HashMap<String, EntryId>,
    shell_sessions: HashMap<i64, EntryId>,
    shell_followups: HashMap<String, EntryId>,
    code_children: HashMap<EntryId, Vec<EntryId>>,
    code_cells: HashMap<String, EntryId>,
    local_shells: HashMap<ShellId, EntryId>,
    message_threads: HashMap<ThreadId, EntryId>,
    message_order: VecDeque<ThreadId>,
    running_tools: HashSet<EntryId>,
    active_runs: usize,
    // Native telemetry can arrive after the worker receipt on its separate channel.
    // The next run starts a new telemetry scope for automatic compaction.
    manual_compaction: Option<ManualCompaction>,
    run_started_at_unix_ms: VecDeque<u64>,
    transient: Option<TransientStatus>,
    pending_error: Option<String>,
    pending_compaction_error: Option<String>,
}

#[derive(Clone, Copy)]
enum ManualCompaction {
    Running,
    Finished,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct AssistantKey {
    call: ModelCallKey,
    item: Option<String>,
    phase: MessagePhase,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ModelCallKey {
    request: Option<Arc<str>>,
    turn: Option<Arc<str>>,
    call: u32,
}

#[derive(Deserialize)]
struct TurnPayload<P> {
    turn_id: Option<Arc<str>>,
    #[serde(flatten)]
    payload: P,
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
        self.active_runs > 0 || matches!(self.manual_compaction, Some(ManualCompaction::Running))
    }

    pub(crate) fn has_running_tools(&self) -> bool {
        !self.running_tools.is_empty()
    }

    pub(crate) fn running_tool_ids(&self) -> impl Iterator<Item = EntryId> + '_ {
        self.running_tools.iter().copied()
    }

    pub(crate) fn apply(&mut self, record: &TranscriptRecord) -> ModelChange {
        if let Some(kind) = record.local_kind() {
            return self.apply_local(kind, record);
        }
        if let Some(kind) = record.agent_kind() {
            return self.apply_agent(kind, record);
        }
        ModelChange::default()
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

        self.reasoning.clear();
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

    fn apply_local(&mut self, kind: LocalKind, record: &TranscriptRecord) -> ModelChange {
        let changed = match kind {
            LocalKind::SessionStarted => record.decode_payload::<SessionStarted>().map(|payload| {
                if let Some(session_id) = payload.parent_session_id {
                    *self = self.fork_snapshot();
                    self.push(EntryKind::ForkedFrom { session_id });
                }
            }),
            LocalKind::UserSubmitted => record.decode_payload::<UserSubmitted>().map(|payload| {
                self.push(EntryKind::User {
                    text: payload.text,
                    images: Vec::new(),
                });
            }),
            LocalKind::UserSteered => record.decode_payload::<UserSteered>().map(|payload| {
                self.push(EntryKind::User {
                    text: payload.text,
                    images: Vec::new(),
                });
            }),
            LocalKind::CompactionStarted => {
                self.manual_compaction = Some(ManualCompaction::Running);
                self.transient = Some(TransientStatus::Compacting);
                Ok(())
            }
            LocalKind::CompactionFinished => {
                record
                    .decode_payload::<CompactionFinished>()
                    .map(|payload| {
                        self.manual_compaction = Some(ManualCompaction::Finished);
                        self.transient = None;
                        self.pending_compaction_error = None;
                        self.pending_error = None;
                        match payload.error {
                            Some(message) => {
                                self.push(EntryKind::ContextCompactionFailed { message });
                            }
                            None => {
                                self.push(EntryKind::ContextCompacted {
                                    duration_ns: payload.duration_ns,
                                });
                            }
                        }
                    })
            }
            LocalKind::ReflectionStarted => {
                record.decode_payload::<ReflectionStarted>().map(|_| {
                    self.push(EntryKind::ReflectionStarted);
                })
            }
            LocalKind::ShellStarted => record
                .decode_payload::<ShellStarted>()
                .map(|payload| self.shell_started(payload, record.recorded_at_unix_ms())),
            LocalKind::ShellFinished => record
                .decode_payload::<ShellFinished>()
                .map(|payload| self.shell_finished(payload)),
            LocalKind::EffortChanged => record.decode_payload::<EffortChanged>().map(|payload| {
                self.push(EntryKind::EffortChanged { to: payload.to });
            }),
            LocalKind::SpeedChanged => record.decode_payload::<SpeedChanged>().map(|payload| {
                self.push(EntryKind::SpeedChanged { speed: payload.to });
            }),
            LocalKind::FastModeChanged => {
                record.decode_payload::<FastModeChanged>().map(|payload| {
                    self.push(EntryKind::SpeedChanged {
                        speed: payload.speed(),
                    });
                })
            }
            LocalKind::WorkerTurnFinished => {
                record
                    .decode_payload::<WorkerTurnFinished>()
                    .map(|payload| {
                        if let Some(error) = payload.error {
                            self.pending_error = Some(error);
                        }
                    })
            }
            LocalKind::WorkerTurnsInterrupted => return self.apply_interruption(record),
            LocalKind::WorkerSteerFailed => {
                record.decode_payload::<WorkerSteerFailed>().map(|payload| {
                    self.push(EntryKind::Error {
                        message: format!("Could not steer response: {}", payload.error),
                    });
                })
            }
            LocalKind::WorkerStopped => record.decode_payload::<WorkerStopped>().map(|payload| {
                if let Some(error) = payload.error {
                    self.pending_error = Some(error);
                }
            }),
            LocalKind::SessionEnded => record.decode_payload::<SessionEnded>().map(|payload| {
                if payload.outcome == SessionOutcome::Failed {
                    self.finish_failed(payload.error);
                }
                self.agent_stream_closed();
            }),
            LocalKind::ContextBudget
            | LocalKind::ContextObserved
            | LocalKind::WorkerTurnAccepted => return ModelChange::default(),
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
        let payload = match record.decode_payload::<WorkerTurnsInterrupted>() {
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
            child_count: 0,
        }));
        self.local_shells.insert(payload.id, id);
        self.running_tools.insert(id);
    }

    fn shell_finished(&mut self, payload: ShellFinished) {
        let Some(id) = self.local_shells.remove(&payload.id) else {
            return;
        };
        self.reasoning.clear();
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

    fn apply_agent(&mut self, kind: AgentEventKind, record: &TranscriptRecord) -> ModelChange {
        let previous_activity = self.transient.clone();
        if matches!(
            kind,
            AgentEventKind::AssistantDelta
                | AgentEventKind::AssistantMessage
                | AgentEventKind::RunStarted
                | AgentEventKind::RunCompleted
                | AgentEventKind::RunFailed
                | AgentEventKind::ToolCall
                | AgentEventKind::ToolResult
        ) {
            self.reasoning.clear();
        }
        let result = match kind {
            AgentEventKind::InputAccepted => self.input_accepted(record),
            AgentEventKind::AssistantDelta => self.assistant_delta(record),
            AgentEventKind::AssistantMessage => self.assistant_message(record),
            AgentEventKind::ReasoningSummaryDelta => self.reasoning_delta(record),
            AgentEventKind::RunStarted => {
                self.manual_compaction = None;
                self.active_runs = self.active_runs.saturating_add(1);
                self.run_started_at_unix_ms
                    .push_back(record.recorded_at_unix_ms());
                self.transient = Some(TransientStatus::Thinking);
                Ok(true)
            }
            AgentEventKind::RunError => record.decode_payload::<RunError>().map(|payload| {
                self.pending_error = Some(payload.message.clone());
                self.transient = Some(TransientStatus::Error(payload.message));
                true
            }),
            AgentEventKind::RunCompleted => {
                self.complete_turn(record);
                Ok(true)
            }
            AgentEventKind::RunFailed => {
                self.run_started_at_unix_ms.pop_front();
                self.finish_failed(None);
                Ok(true)
            }
            AgentEventKind::ToolCall => self.tool_call(record),
            AgentEventKind::ToolResult => self.tool_result(record),
            AgentEventKind::ModelWarmupStarted => {
                self.transient = Some(TransientStatus::Warming);
                Ok(true)
            }
            AgentEventKind::ModelWarmupCompleted => {
                self.transient = self.is_active().then_some(TransientStatus::Thinking);
                Ok(true)
            }
            AgentEventKind::ModelWarmupFailed
            | AgentEventKind::ModelCallFailed
            | AgentEventKind::ModelAttemptFailed
            | AgentEventKind::ModelConnectionFailed => self.capture_error(record),
            AgentEventKind::ModelCallStarted => {
                self.materialize_compaction_failure();
                self.transient = Some(TransientStatus::Thinking);
                Ok(true)
            }
            AgentEventKind::ModelCallCompleted => {
                self.transient = self.is_active().then_some(TransientStatus::Thinking);
                Ok(true)
            }
            AgentEventKind::ModelCompactionStarted
            | AgentEventKind::ModelCompactionCompleted
            | AgentEventKind::ModelCompactionFailed
                if self.manual_compaction.is_some() =>
            {
                Ok(false)
            }
            AgentEventKind::ModelCompactionStarted => {
                self.transient = Some(TransientStatus::Compacting);
                Ok(true)
            }
            AgentEventKind::ModelCompactionCompleted => self.compaction_completed(record),
            AgentEventKind::ModelCompactionFailed => self.compaction_failed(record),
            AgentEventKind::ModelAttemptRetrying => self.retrying(record),
            AgentEventKind::ModelConnectionStarted => self.connection_started(record),
            AgentEventKind::ModelConnectionCompleted => {
                self.transient = self.is_active().then_some(TransientStatus::Thinking);
                self.pending_error = None;
                Ok(true)
            }
            AgentEventKind::ApiEvent
            | AgentEventKind::RunSteered
            | AgentEventKind::ModelAttemptStarted => Ok(false),
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
                visibility(kind) == EventVisibility::Persistent,
            ),
        }
    }

    fn input_accepted(&mut self, record: &TranscriptRecord) -> Result<bool, serde_json::Error> {
        #[derive(Deserialize)]
        struct InputAccepted {
            input: PromptInput,
        }
        let payload = record.decode_payload::<InputAccepted>()?;
        let PromptInput::Content(input) = payload.input else {
            return Ok(false);
        };
        let Some(entry) = self
            .entries
            .iter_mut()
            .rev()
            .find(|entry| matches!(entry.kind, EntryKind::User { .. }))
        else {
            return Ok(false);
        };
        let EntryKind::User { text, images } = &mut entry.kind else {
            return Ok(false);
        };
        let markers = text.match_indices("[Image #").filter_map(|(start, _)| {
            let suffix = &text[start + "[Image #".len()..];
            let digits = suffix.bytes().take_while(u8::is_ascii_digit).count();
            (digits > 0 && suffix.as_bytes().get(digits) == Some(&b']'))
                .then_some(start..start + "[Image #".len() + digits + 1)
        });
        let attached = input
            .into_iter()
            .filter_map(|part| match part {
                UserInput::Image { image_url, .. } => Some(image_url),
                _ => None,
            })
            .zip(markers)
            .map(|(data_url, range)| UserImage { range, data_url })
            .collect::<Vec<_>>();
        if *images == attached {
            return Ok(false);
        }
        *images = attached;
        entry.revision = entry.revision.saturating_add(1);
        Ok(true)
    }

    fn assistant_delta(&mut self, record: &TranscriptRecord) -> Result<bool, serde_json::Error> {
        let TurnPayload { turn_id, payload } =
            record.decode_payload::<TurnPayload<AssistantDelta>>()?;
        let phase = message_phase(payload.phase);
        let key = AssistantKey {
            call: ModelCallKey {
                request: record.agent_request_id(),
                turn: turn_id,
                call: payload.model_call_index,
            },
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
            self.active_assistants.insert((key.call.clone(), phase), id);
            self.assistants.insert(key, id);
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
        let TurnPayload { turn_id, payload } =
            record.decode_payload::<TurnPayload<AssistantMessage>>()?;
        let phase = message_phase(payload.phase);
        let key = AssistantKey {
            call: ModelCallKey {
                request: record.agent_request_id(),
                turn: turn_id,
                call: payload.model_call_index,
            },
            item: payload.item_id,
            phase,
        };
        let id = self
            .assistants
            .get(&key)
            .copied()
            .or_else(|| {
                self.active_assistants
                    .get(&(key.call.clone(), phase))
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
        let TurnPayload { turn_id, payload } =
            record.decode_payload::<TurnPayload<ReasoningSummaryDelta>>()?;
        let key = ModelCallKey {
            request: record.agent_request_id(),
            turn: turn_id,
            call: payload.model_call_index,
        };
        let id = self
            .reasoning
            .get(&key)
            .copied()
            .filter(|id| self.entries.last().is_some_and(|entry| entry.id == *id))
            .unwrap_or_else(|| {
                let id = self.push(EntryKind::Reasoning {
                    text: String::new(),
                });
                self.reasoning.insert(key, id);
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
        let parent = self.code_parent(&call_id);
        if tool == "write_stdin"
            && let Some(session_id) = arguments.get("session_id").and_then(Value::as_i64)
            && let Some(id) = self.shell_sessions.get(&session_id).copied()
        {
            if parent.is_some() {
                self.shell_followups.insert(call_id.clone(), id);
            } else {
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
        }
        let displayed_parent = parent.and_then(|parent| self.next_code_child_parent(parent));
        let hidden = tool == "wait" && parent.is_none();
        let transient = if hidden {
            TransientStatus::WaitingForBackgroundWork
        } else {
            TransientStatus::Tool(humanize_tool(&tool))
        };
        let id = self.push_with_parent(
            EntryKind::Tool(ToolEntry {
                name: tool,
                arguments,
                started_at_unix_ms: record.recorded_at_unix_ms(),
                state: ToolState::Running,
                duration_ns: None,
                result: None,
                metadata: None,
                substeps: Vec::new(),
                child_count: 0,
            }),
            hidden,
            displayed_parent,
        );
        if let Some(parent) = parent {
            self.register_code_child(parent, id);
        }
        self.tools.insert(call_id, id);
        self.running_tools.insert(id);
        self.transient = Some(transient);
        Ok(true)
    }

    fn tool_result(&mut self, record: &TranscriptRecord) -> Result<bool, serde_json::Error> {
        let payload = record.decode_payload::<ToolResultPayload>()?;
        let resumed_shell = self.shell_followups.remove(&payload.call_id);
        let shell_followup = payload.tool == "write_stdin";
        let result = if matches!(payload.tool.as_str(), "exec_command" | "write_stdin") {
            normalize_result(payload.structured_result)
        } else {
            normalize_result(payload.result)
        };
        let resumed_result = resumed_shell.map(|_| result.clone());
        let state = tool_result_state(&payload.tool, &payload.status, &result);
        let entry_state = if resumed_shell.is_some() && state == ToolState::Running {
            ToolState::Succeeded
        } else {
            state
        };
        let shell_session = (payload.tool == "exec_command")
            .then(|| tool_session_id(&result))
            .flatten();
        let id = self
            .tools
            .get(&payload.call_id)
            .copied()
            .unwrap_or_else(|| {
                let parent = self.code_parent(&payload.call_id);
                let displayed_parent =
                    parent.and_then(|parent| self.next_code_child_parent(parent));
                let id = self.push_with_parent(
                    EntryKind::Tool(ToolEntry {
                        name: payload.tool.clone(),
                        arguments: Value::Null,
                        started_at_unix_ms: record.recorded_at_unix_ms(),
                        state: ToolState::Running,
                        duration_ns: None,
                        result: None,
                        metadata: None,
                        substeps: Vec::new(),
                        child_count: 0,
                    }),
                    false,
                    displayed_parent,
                );
                if let Some(parent) = parent {
                    self.register_code_child(parent, id);
                }
                self.tools.insert(payload.call_id.clone(), id);
                id
            });
        let running_code_cell = (payload.tool == "exec")
            .then(|| running_code_cell_id(&result))
            .flatten()
            .map(str::to_owned);
        let observed_code_cell = (payload.tool == "wait")
            .then(|| self.requested_code_cell(id))
            .flatten()
            .map(str::to_owned);
        let code_cell_terminal = code_cell_terminal(&result);
        self.update(id, |kind| {
            if let EntryKind::Tool(tool) = kind {
                tool.state = entry_state;
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
            }
        });
        if let Some(shell) = resumed_shell {
            let resumed_result = resumed_result.expect("resumed shell result was retained");
            self.update(shell, |kind| {
                if let EntryKind::Tool(tool) = kind {
                    tool.state = state;
                    tool.duration_ns = Some(
                        elapsed_nanoseconds(tool.started_at_unix_ms, record.recorded_at_unix_ms())
                            .max(payload.duration_ns),
                    );
                    tool.result = Some(merge_shell_result(tool.result.take(), resumed_result));
                }
            });
            if state != ToolState::Running {
                self.shell_sessions.retain(|_, entry| *entry != shell);
                self.running_tools.remove(&shell);
            }
        }
        if payload.tool == "wait"
            && state == ToolState::Failed
            && let Some(index) = self.index_of(id)
        {
            self.entries[index].hidden = false;
        }
        if entry_state == ToolState::Running {
            if let Some(session_id) = shell_session {
                self.shell_sessions.insert(session_id, id);
            }
            self.running_tools.insert(id);
        } else {
            self.shell_sessions
                .retain(|_, shell_entry| *shell_entry != id);
            self.running_tools.remove(&id);
        }
        if let Some(cell_id) = running_code_cell {
            self.code_cells.insert(cell_id, id);
        }
        if let Some(cell_id) = observed_code_cell
            && let Some(terminal) = code_cell_terminal
            && let Some(parent) = self.code_cells.remove(&cell_id)
            && terminal == CodeCellTerminal::Terminated
        {
            self.fail_unfinished_code_children(parent);
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
        self.pending_error = Some(payload.error);
        self.transient = Some(TransientStatus::Retrying {
            delay_ns: payload.delay_ns,
            next_attempt: payload.next_attempt,
            max_attempts: payload.max_attempts,
        });
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
        self.fail_unfinished_tools(&orphaned);
    }

    fn fail_unfinished_code_children(&mut self, parent: EntryId) {
        let unfinished = self
            .code_children
            .get(&parent)
            .into_iter()
            .flatten()
            .filter(|id| self.running_tools.contains(id))
            .copied()
            .collect::<Vec<_>>();
        self.fail_unfinished_tools(&unfinished);
    }

    fn fail_unfinished_tools(&mut self, unfinished: &[EntryId]) {
        for id in unfinished {
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
        self.shell_sessions.retain(|_, id| !unfinished.contains(id));
        self.shell_followups
            .retain(|_, id| !unfinished.contains(id));
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
        self.manual_compaction = None;
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

    fn push(&mut self, kind: EntryKind) -> EntryId {
        self.push_with_visibility(kind, false)
    }

    fn push_with_visibility(&mut self, kind: EntryKind, hidden: bool) -> EntryId {
        self.push_with_parent(kind, hidden, None)
    }

    fn push_with_parent(
        &mut self,
        kind: EntryKind,
        hidden: bool,
        parent: Option<EntryId>,
    ) -> EntryId {
        if let Some(parent) = parent {
            self.join_workflow(parent);
        }
        let id = EntryId::from_index(self.next_entry_id);
        self.next_entry_id = self.next_entry_id.saturating_add(1);
        self.entry_indices.insert(id, self.entries.len());
        self.entries.push(TranscriptEntry {
            id,
            revision: 1,
            kind,
            hidden,
            parent,
            trailing_spacer: true,
        });
        id
    }

    fn join_workflow(&mut self, parent: EntryId) {
        let Some(previous) = self.entries.iter_mut().rev().find(|entry| !entry.hidden) else {
            return;
        };
        if previous.id != parent && previous.parent != Some(parent) {
            return;
        }
        previous.trailing_spacer = false;
        previous.revision = previous.revision.saturating_add(1);
    }

    fn code_parent(&self, call_id: &str) -> Option<EntryId> {
        let (parent_call_id, child) = call_id.rsplit_once("/code-")?;
        child.parse::<u64>().ok()?;
        let parent = self.tools.get(parent_call_id).copied()?;
        let entry = self.entry(parent)?;
        matches!(&entry.kind, EntryKind::Tool(tool) if tool.name == "exec").then_some(parent)
    }

    fn requested_code_cell(&self, id: EntryId) -> Option<&str> {
        let EntryKind::Tool(tool) = &self.entry(id)?.kind else {
            return None;
        };
        tool.arguments.get("cell_id")?.as_str()
    }

    fn next_code_child_parent(&self, parent: EntryId) -> Option<EntryId> {
        self.code_children
            .get(&parent)
            .is_some_and(|children| !children.is_empty())
            .then_some(parent)
    }

    fn register_code_child(&mut self, parent: EntryId, child: EntryId) {
        self.update(parent, |kind| {
            if let EntryKind::Tool(tool) = kind {
                tool.child_count = tool.child_count.saturating_add(1);
            }
        });
        let children = self.code_children.entry(parent).or_default();
        children.push(child);
        let child_count = children.len();

        if child_count == 1 {
            let index = self.index_of(parent).expect("code parent is retained");
            self.entries[index].hidden = true;
            self.entries[index].revision = self.entries[index].revision.saturating_add(1);
            return;
        }
        let previous_child = self.code_children[&parent][child_count - 2];
        if child_count == 2 {
            let parent_index = self.index_of(parent).expect("code parent is retained");
            self.entries[parent_index].hidden = false;
            self.entries[parent_index].trailing_spacer = false;
            self.entries[parent_index].revision =
                self.entries[parent_index].revision.saturating_add(1);

            self.move_entry_after(previous_child, parent);
        }

        let previous_index = self
            .index_of(previous_child)
            .expect("code child is retained");
        self.entries[previous_index].parent = Some(parent);
        self.entries[previous_index].trailing_spacer = false;
        self.entries[previous_index].revision =
            self.entries[previous_index].revision.saturating_add(1);

        let child_index = self.index_of(child).expect("code child is retained");
        self.entries[child_index].parent = Some(parent);
        self.entries[child_index].trailing_spacer = true;
        self.entries[child_index].revision = self.entries[child_index].revision.saturating_add(1);
        self.move_entry_after(child, previous_child);
    }

    fn move_entry_after(&mut self, id: EntryId, previous: EntryId) {
        let index = self.index_of(id).expect("moved entry is retained");
        let entry = self.entries.remove(index);
        let previous_index = self
            .entries
            .iter()
            .position(|entry| entry.id == previous)
            .expect("preceding entry is retained");
        self.entries.insert(previous_index + 1, entry);
        self.entry_indices = self
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.id, index))
            .collect();
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

/// How an agent event surfaces when its payload cannot be projected.
const fn visibility(kind: AgentEventKind) -> EventVisibility {
    match kind {
        AgentEventKind::AssistantDelta
        | AgentEventKind::AssistantMessage
        | AgentEventKind::ReasoningSummaryDelta
        | AgentEventKind::ToolCall
        | AgentEventKind::ToolResult
        | AgentEventKind::ModelCompactionCompleted
        | AgentEventKind::ModelCompactionFailed => EventVisibility::Persistent,
        AgentEventKind::RunStarted
        | AgentEventKind::ModelWarmupStarted
        | AgentEventKind::ModelCallStarted
        | AgentEventKind::ModelCompactionStarted
        | AgentEventKind::ModelAttemptRetrying
        | AgentEventKind::ModelConnectionStarted => EventVisibility::Transient,
        AgentEventKind::RunError
        | AgentEventKind::RunFailed
        | AgentEventKind::ModelWarmupFailed
        | AgentEventKind::ModelCallFailed
        | AgentEventKind::ModelAttemptFailed
        | AgentEventKind::ModelConnectionFailed => EventVisibility::ErrorFallback,
        AgentEventKind::ApiEvent
        | AgentEventKind::InputAccepted
        | AgentEventKind::RunSteered
        | AgentEventKind::RunCompleted
        | AgentEventKind::ModelWarmupCompleted
        | AgentEventKind::ModelCallCompleted
        | AgentEventKind::ModelAttemptStarted
        | AgentEventKind::ModelConnectionCompleted => EventVisibility::StateOnly,
    }
}

fn tool_session_id(result: &Value) -> Option<i64> {
    if let Value::String(text) = result {
        let decoded = serde_json::from_str::<Value>(text).ok()?;
        return decoded.get("session_id").and_then(Value::as_i64);
    }
    result.get("session_id").and_then(Value::as_i64)
}

fn running_code_cell_id(result: &Value) -> Option<&str> {
    code_mode_status(result)?
        .strip_prefix("Script running with cell ID ")?
        .split_whitespace()
        .next()
}

fn code_cell_terminal(result: &Value) -> Option<CodeCellTerminal> {
    let status = code_mode_status(result)?;
    if status.starts_with("Script completed") {
        Some(CodeCellTerminal::Completed)
    } else if status.starts_with("Script terminated") {
        Some(CodeCellTerminal::Terminated)
    } else {
        None
    }
}

fn code_mode_status(result: &Value) -> Option<&str> {
    match result {
        Value::String(status) => Some(status),
        Value::Array(items) => items
            .iter()
            .find_map(|item| item.get("text").and_then(Value::as_str)),
        Value::Object(fields) => fields.get("text").and_then(Value::as_str),
        Value::Null | Value::Bool(_) | Value::Number(_) => None,
    }
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
    structured_result: Value,
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
    next_attempt: u32,
    max_attempts: u32,
    error: String,
}

#[derive(Deserialize)]
struct ConnectionPayload {
    purpose: String,
}

/// The tool label both front-ends show while a tool runs and in its transcript entry.
pub(crate) fn humanize_tool(name: &str) -> String {
    name.trim_start_matches("mcp__")
        .replace("__", " · ")
        .replace('_', " ")
}

#[cfg(test)]
mod tests {
    use super::{
        EntryKind, EventVisibility, MAX_RETAINED_MESSAGE_THREADS, ToolState, TranscriptModel,
        merge_shell_result, visibility,
    };
    use crate::{
        app::config::{ReasoningEffort, Speed},
        core::transcript::{
            EffortChanged, LocalEvent, ReflectionStarted, SessionEnded, SessionOutcome,
            ShellFinished, ShellId, ShellStarted, SpeedChanged, TranscriptRecord, TurnId,
            UserSteered, UserSubmitted, WorkerTurnsInterrupted,
        },
    };
    use nanocodex::agent::events::{AgentEvent, AgentEventKind};
    use serde::Serialize;
    use serde_json::{json, value::to_raw_value};
    use std::sync::Arc;
    use tact_subagents::{AgentId, AgentMessageUpdate, MessageSender, ThreadId};

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

    fn agent_turn_at(
        turn_id: &str,
        recorded_at_unix_ms: u64,
        kind: AgentEventKind,
        mut payload: serde_json::Value,
    ) -> TranscriptRecord {
        payload["turn_id"] = json!(turn_id);
        agent_at(recorded_at_unix_ms, kind, payload)
    }

    fn assistant_replies(model: &TranscriptModel) -> Vec<(&str, bool)> {
        model
            .entries()
            .iter()
            .filter_map(|entry| match &entry.kind {
                EntryKind::Assistant { text, complete } => Some((text.as_str(), *complete)),
                _ => None,
            })
            .collect()
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
            tact_subagents::MessageDeliveryState::Delivered { .. }
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
            LocalEvent::WorkerTurnsInterrupted(WorkerTurnsInterrupted {
                count: 0,
                error: None,
            }),
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
    fn every_stable_agent_event_has_an_explicit_visibility() {
        let cases = [
            (AgentEventKind::ApiEvent, EventVisibility::StateOnly),
            (AgentEventKind::AssistantDelta, EventVisibility::Persistent),
            (
                AgentEventKind::AssistantMessage,
                EventVisibility::Persistent,
            ),
            (
                AgentEventKind::ReasoningSummaryDelta,
                EventVisibility::Persistent,
            ),
            (AgentEventKind::RunStarted, EventVisibility::Transient),
            (AgentEventKind::RunSteered, EventVisibility::StateOnly),
            (AgentEventKind::RunError, EventVisibility::ErrorFallback),
            (AgentEventKind::RunCompleted, EventVisibility::StateOnly),
            (AgentEventKind::RunFailed, EventVisibility::ErrorFallback),
            (AgentEventKind::ToolCall, EventVisibility::Persistent),
            (AgentEventKind::ToolResult, EventVisibility::Persistent),
            (
                AgentEventKind::ModelWarmupStarted,
                EventVisibility::Transient,
            ),
            (
                AgentEventKind::ModelWarmupCompleted,
                EventVisibility::StateOnly,
            ),
            (
                AgentEventKind::ModelWarmupFailed,
                EventVisibility::ErrorFallback,
            ),
            (AgentEventKind::ModelCallStarted, EventVisibility::Transient),
            (
                AgentEventKind::ModelCallCompleted,
                EventVisibility::StateOnly,
            ),
            (
                AgentEventKind::ModelCallFailed,
                EventVisibility::ErrorFallback,
            ),
            (
                AgentEventKind::ModelCompactionStarted,
                EventVisibility::Transient,
            ),
            (
                AgentEventKind::ModelCompactionCompleted,
                EventVisibility::Persistent,
            ),
            (
                AgentEventKind::ModelCompactionFailed,
                EventVisibility::Persistent,
            ),
            (
                AgentEventKind::ModelAttemptStarted,
                EventVisibility::StateOnly,
            ),
            (
                AgentEventKind::ModelAttemptFailed,
                EventVisibility::ErrorFallback,
            ),
            (
                AgentEventKind::ModelAttemptRetrying,
                EventVisibility::Transient,
            ),
            (
                AgentEventKind::ModelConnectionStarted,
                EventVisibility::Transient,
            ),
            (
                AgentEventKind::ModelConnectionCompleted,
                EventVisibility::StateOnly,
            ),
            (
                AgentEventKind::ModelConnectionFailed,
                EventVisibility::ErrorFallback,
            ),
        ];

        for (kind, expected) in cases {
            assert_eq!(visibility(kind), expected, "{kind:?}");
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
    fn assistant_replies_with_restarted_call_indexes_survive_new_turns_and_replay() {
        for (stream_second, item_id) in [
            (false, Some("message")),
            (true, None),
            (true, Some("message")),
        ] {
            let mut model = TranscriptModel::default();
            let mut records = Vec::new();
            for (turn, request) in [(1, "first"), (2, "second")] {
                records.push(
                    TranscriptRecord::from_local(
                        1,
                        turn,
                        LocalEvent::UserSubmitted(UserSubmitted {
                            id: TurnId::new(turn),
                            text: request.to_owned(),
                        }),
                    )
                    .unwrap(),
                );
                records.push(agent_turn_at(
                    request,
                    turn,
                    AgentEventKind::RunStarted,
                    json!({}),
                ));
                for phase in ["commentary", "final_answer"] {
                    if turn == 1 || stream_second {
                        for text in [request, " partial"] {
                            records.push(agent_turn_at(
                                request,
                                turn,
                                AgentEventKind::AssistantDelta,
                                json!({
                                    "model_call_index": 1,
                                    "item_id": item_id,
                                    "phase": phase,
                                    "text": text,
                                }),
                            ));
                        }
                    }
                    records.push(agent_turn_at(
                        request,
                        turn,
                        AgentEventKind::AssistantMessage,
                        json!({
                            "model_call_index": 1,
                            "item_id": null,
                            "phase": phase,
                            "text": format!("{request} {phase}"),
                        }),
                    ));
                }
                records.push(agent_turn_at(
                    request,
                    turn,
                    AgentEventKind::RunCompleted,
                    json!({}),
                ));
            }
            let mut replay = TranscriptModel::default();
            for record in records {
                model.apply(&record);
                if record.agent_kind() != Some(AgentEventKind::AssistantDelta) {
                    let persisted = serde_json::to_string(&record).unwrap();
                    replay.apply(&serde_json::from_str(&persisted).unwrap());
                }
            }
            for transcript in [&model, &replay] {
                assert_eq!(
                    assistant_replies(transcript),
                    [
                        ("first commentary", true),
                        ("first final_answer", true),
                        ("second commentary", true),
                        ("second final_answer", true),
                    ],
                    "stream_second={stream_second}, item_id={item_id:?}"
                );
            }
        }
    }

    #[test]
    fn claude_replies_with_null_stream_identity_remain_separate_across_turns() {
        let mut model = TranscriptModel::default();
        for request in ["first", "second"] {
            model.apply(&agent_turn_at(
                request,
                1,
                AgentEventKind::RunStarted,
                json!({}),
            ));
            model.apply(&agent_turn_at(
                request,
                1,
                AgentEventKind::AssistantDelta,
                json!({"model_call_index": 1, "item_id": null, "phase": null, "text": "partial"}),
            ));
            model.apply(&agent_turn_at(
                request,
                1,
                AgentEventKind::AssistantMessage,
                json!({
                    "model_call_index": 1,
                    "item_id": format!("{request}-message"),
                    "phase": null,
                    "text": request,
                }),
            ));
            model.apply(&agent_turn_at(
                request,
                1,
                AgentEventKind::RunCompleted,
                json!({}),
            ));
        }
        assert_eq!(
            assistant_replies(&model),
            [("first", true), ("second", true)]
        );
    }

    #[test]
    fn concurrent_runs_keep_assistant_streams_and_final_messages_separate() {
        let mut model = TranscriptModel::default();
        for request in ["first", "second"] {
            model.apply(&agent_turn_at(
                request,
                1,
                AgentEventKind::RunStarted,
                json!({}),
            ));
            model.apply(&agent_turn_at(
                request,
                1,
                AgentEventKind::AssistantDelta,
                json!({"model_call_index": 1, "item_id": "message", "text": request}),
            ));
        }
        for request in ["first", "second"] {
            model.apply(&agent_turn_at(
                request,
                1,
                AgentEventKind::AssistantMessage,
                json!({"model_call_index": 1, "item_id": null, "text": request}),
            ));
            model.apply(&agent_turn_at(
                request,
                1,
                AgentEventKind::RunCompleted,
                json!({}),
            ));
        }
        assert_eq!(
            assistant_replies(&model),
            [("first", true), ("second", true)]
        );
    }

    #[test]
    fn assistant_item_ids_coalesce_streams_across_phases_and_calls() {
        for stream_item in [None, Some("item")] {
            let mut model = TranscriptModel::default();
            model.apply(&agent(AgentEventKind::RunStarted, json!({})));
            for call in 1..=2 {
                for phase in ["commentary", "final_answer"] {
                    let item = format!("item-{call}-{phase}");
                    for text in ["partial", " reply"] {
                        model.apply(&agent(
                            AgentEventKind::AssistantDelta,
                            json!({
                                "model_call_index": call,
                                "item_id": stream_item.map(|_| &item),
                                "phase": phase,
                                "text": text,
                            }),
                        ));
                    }
                    model.apply(&agent(
                        AgentEventKind::AssistantMessage,
                        json!({
                            "model_call_index": call,
                            "item_id": item,
                            "phase": phase,
                            "text": format!("{call} {phase}"),
                        }),
                    ));
                }
            }
            assert_eq!(
                assistant_replies(&model),
                [
                    ("1 commentary", true),
                    ("1 final_answer", true),
                    ("2 commentary", true),
                    ("2 final_answer", true),
                ],
                "stream_item={stream_item:?}"
            );
        }
    }

    #[test]
    fn concurrent_runs_do_not_share_reasoning_blocks() {
        let mut model = TranscriptModel::default();
        model.apply(&agent_turn_at(
            "run-a",
            1,
            AgentEventKind::ReasoningSummaryDelta,
            json!({"model_call_index": 1, "text": "run a"}),
        ));
        model.apply(&agent_turn_at(
            "run-b",
            2,
            AgentEventKind::ReasoningSummaryDelta,
            json!({"model_call_index": 1, "text": "run b"}),
        ));

        let reasoning = model
            .entries()
            .iter()
            .filter_map(|entry| match &entry.kind {
                EntryKind::Reasoning { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(reasoning, ["run a", "run b"]);
    }

    #[test]
    fn reasoning_after_messages_and_tools_starts_new_blocks() {
        let mut model = TranscriptModel::default();
        model.apply(&agent(AgentEventKind::RunStarted, json!({})));
        model.apply(&agent(
            AgentEventKind::ReasoningSummaryDelta,
            json!({"model_call_index": 1, "text": "before message"}),
        ));
        model.apply(&agent(
            AgentEventKind::AssistantMessage,
            json!({
                "model_call_index": 1,
                "item_id": "message",
                "phase": "commentary",
                "text": "status update",
            }),
        ));
        model.apply(&agent(AgentEventKind::RunCompleted, json!({})));

        model.apply(&agent(AgentEventKind::RunStarted, json!({})));
        model.apply(&agent(
            AgentEventKind::ReasoningSummaryDelta,
            json!({"model_call_index": 1, "text": "after message"}),
        ));
        model.apply(&agent(
            AgentEventKind::ToolCall,
            json!({
                "call_id": "tool",
                "tool": "exec_command",
                "arguments": {"cmd": "true"},
            }),
        ));
        model.apply(&agent(
            AgentEventKind::ReasoningSummaryDelta,
            json!({"model_call_index": 1, "text": "after tool"}),
        ));

        let reasoning = model
            .entries()
            .iter()
            .filter_map(|entry| match &entry.kind {
                EntryKind::Reasoning { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(reasoning, ["before message", "after message", "after tool"]);
    }

    #[test]
    fn reasoning_after_in_place_shell_followup_starts_new_block() {
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
                "result": "Process running with session ID 7",
                "structured_result": {
                    "output": "running",
                    "session_id": 7,
                    "wall_time_seconds": 0.0,
                },
                "metadata": null,
            }),
        ));
        model.apply(&agent(
            AgentEventKind::ReasoningSummaryDelta,
            json!({"model_call_index": 1, "text": "before followup"}),
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
            4_000,
            AgentEventKind::ToolResult,
            json!({
                "call_id": "stdin",
                "tool": "write_stdin",
                "status": "completed",
                "duration_ns": 1_u64,
                "result": "Process exited with code 0",
                "structured_result": {
                    "output": "done",
                    "exit_code": 0,
                    "wall_time_seconds": 0.0,
                },
                "metadata": null,
            }),
        ));
        model.apply(&agent(
            AgentEventKind::ReasoningSummaryDelta,
            json!({"model_call_index": 1, "text": "after followup"}),
        ));

        let reasoning = model
            .entries()
            .iter()
            .filter_map(|entry| match &entry.kind {
                EntryKind::Reasoning { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(reasoning, ["before followup", "after followup"]);
    }

    #[test]
    fn ignored_message_update_does_not_split_reasoning() {
        let mut model = TranscriptModel::default();
        let perspective = MessageSender::Agent {
            agent_id: AgentId::new(1),
        };
        let admitted = message_update("admitted");
        assert!(model.apply_message(perspective, admitted.clone()).changed);
        model.apply(&agent(
            AgentEventKind::ReasoningSummaryDelta,
            json!({"model_call_index": 1, "text": "before no-op"}),
        ));

        assert!(!model.apply_message(perspective, admitted).changed);
        model.apply(&agent(
            AgentEventKind::ReasoningSummaryDelta,
            json!({"model_call_index": 1, "text": " after no-op"}),
        ));

        let reasoning = model
            .entries()
            .iter()
            .filter_map(|entry| match &entry.kind {
                EntryKind::Reasoning { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(reasoning, ["before no-op after no-op"]);
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
            json!({"delay_ns": 500_000_000, "attempt": 1, "next_attempt": 2, "max_attempts": 5, "error": "temporary"}),
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
    fn accepted_images_attach_to_user_markers_and_replay_identically() {
        let text = "é [Image #7] text [Image #21] [Image #bad]";
        let submitted = TranscriptRecord::from_local(
            1,
            1,
            LocalEvent::UserSubmitted(UserSubmitted {
                id: TurnId::new(3),
                text: text.to_owned(),
            }),
        )
        .unwrap();
        let accepted = agent(
            AgentEventKind::InputAccepted,
            json!({
                "input": [
                    {"type": "text", "text": "prompt"},
                    {"type": "image", "image_url": "data:image/png;base64,first"},
                    {"type": "image", "image_url": "data:image/png;base64,second"},
                    {"type": "image", "image_url": "data:image/png;base64,extra"}
                ]
            }),
        );
        let mut model = TranscriptModel::default();
        model.apply(&submitted);
        let revision = model.entries()[0].revision;
        assert!(model.apply(&accepted).changed);
        assert!(model.entries()[0].revision > revision);
        let EntryKind::User { images, .. } = &model.entries()[0].kind else {
            panic!("user entry missing")
        };
        assert_eq!(images.len(), 2);
        assert_eq!(images[0].range, 3..13);
        assert_eq!(images[1].range, 19..30);
        assert_eq!(&text[images[0].range.clone()], "[Image #7]");
        assert_eq!(images[1].data_url, "data:image/png;base64,second");
        let mut replay = TranscriptModel::default();
        for record in [&submitted, &accepted] {
            let encoded = serde_json::to_string(record).unwrap();
            let restored = serde_json::from_str::<TranscriptRecord>(&encoded).unwrap();
            replay.apply(&restored);
        }
        let EntryKind::User {
            images: replayed, ..
        } = &replay.entries()[0].kind
        else {
            panic!("user entry missing")
        };
        assert_eq!(images, replayed);
        assert!(!model.apply(&accepted).changed);
    }

    #[test]
    fn accepted_plain_text_does_not_change_the_user_entry() {
        let mut model = TranscriptModel::default();
        model.apply(
            &TranscriptRecord::from_local(
                1,
                1,
                LocalEvent::UserSubmitted(UserSubmitted {
                    id: TurnId::new(3),
                    text: "hello".to_owned(),
                }),
            )
            .unwrap(),
        );
        let revision = model.entries()[0].revision;
        assert!(
            !model
                .apply(&agent(
                    AgentEventKind::InputAccepted,
                    json!({"input": "hello"})
                ))
                .changed
        );
        assert_eq!(model.entries().len(), 1);
        assert_eq!(model.entries()[0].revision, revision);
    }

    #[test]
    fn local_user_event_is_persistent() {
        let mut model = TranscriptModel::default();
        let record = TranscriptRecord::from_local(
            1,
            1,
            LocalEvent::UserSubmitted(UserSubmitted {
                id: TurnId::new(3),
                text: "hello".to_owned(),
            }),
        )
        .unwrap();
        model.apply(&record);

        assert!(
            matches!(&model.entries()[0].kind, EntryKind::User { text, .. } if text == "hello")
        );
    }

    #[test]
    fn reflection_start_is_a_persistent_typed_entry() {
        let mut model = TranscriptModel::default();
        let record = TranscriptRecord::from_local(
            1,
            1,
            LocalEvent::ReflectionStarted(ReflectionStarted { id: TurnId::new(3) }),
        )
        .unwrap();

        model.apply(&record);

        assert!(matches!(
            model.entries().first().map(|entry| &entry.kind),
            Some(EntryKind::ReflectionStarted)
        ));
    }

    #[test]
    fn applied_session_settings_are_visible_transcript_entries() {
        let mut model = TranscriptModel::default();
        model.apply(
            &TranscriptRecord::from_local(
                1,
                1,
                LocalEvent::EffortChanged(EffortChanged {
                    from: ReasoningEffort::Medium,
                    to: ReasoningEffort::High,
                }),
            )
            .unwrap(),
        );
        model.apply(
            &TranscriptRecord::from_local(
                2,
                2,
                LocalEvent::SpeedChanged(SpeedChanged {
                    from: Speed::Standard,
                    to: Speed::Ultrafast,
                }),
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
            EntryKind::SpeedChanged {
                speed: Speed::Ultrafast
            }
        ));
    }

    #[test]
    fn historical_fast_mode_changes_render_as_speed_entries() {
        let mut model = TranscriptModel::default();
        for (sequence, from, to, speed) in [
            (1, false, true, Speed::Fast),
            (2, true, false, Speed::Standard),
        ] {
            let encoded = json!({
                "schema_version": 2,
                "sequence": sequence,
                "recorded_at_unix_ms": sequence,
                "source": "tact",
                "type": "fast_mode.changed",
                "payload": { "from": from, "to": to },
            });
            let record: TranscriptRecord = serde_json::from_value(encoded.clone()).unwrap();
            model.apply(&record);

            assert!(matches!(
                model.entries().last().map(|entry| &entry.kind),
                Some(EntryKind::SpeedChanged { speed: rendered }) if *rendered == speed
            ));
            assert_eq!(serde_json::to_value(record).unwrap(), encoded);
        }
    }

    #[test]
    fn fork_snapshot_keeps_stable_history_and_drops_the_active_turn() {
        let mut model = TranscriptModel::default();
        for (sequence, text) in [(1, "completed"), (2, "still running")] {
            model.apply(
                &TranscriptRecord::from_local(
                    sequence,
                    sequence,
                    LocalEvent::UserSubmitted(UserSubmitted {
                        id: TurnId::new(sequence),
                        text: text.to_owned(),
                    }),
                )
                .unwrap(),
            );
        }
        model.apply(&agent(AgentEventKind::RunStarted, json!({})));

        let snapshot = model.fork_snapshot();

        assert_eq!(snapshot.entries().len(), 1);
        assert!(matches!(
            &snapshot.entries()[0].kind,
            EntryKind::User { text, .. } if text == "completed"
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
            LocalEvent::ShellStarted(ShellStarted {
                id: ShellId::new(7),
                command: "printf hello".to_owned(),
                workspace: "/work".into(),
            }),
        )
        .unwrap();
        let finished = TranscriptRecord::from_local(
            2,
            2,
            LocalEvent::ShellFinished(ShellFinished {
                id: ShellId::new(7),
                output: "hello".to_owned(),
                exit_code: Some(0),
                duration_ns: 10,
                truncated: false,
                error: None,
            }),
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
                LocalEvent::ShellStarted(ShellStarted {
                    id: ShellId::new(7),
                    command: "sleep 100".to_owned(),
                    workspace: "/work".into(),
                }),
            )
            .unwrap(),
            TranscriptRecord::from_local(
                2,
                2,
                LocalEvent::ShellFinished(ShellFinished {
                    id: ShellId::new(7),
                    output: String::new(),
                    exit_code: None,
                    duration_ns: 10,
                    truncated: false,
                    error: None,
                }),
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
            LocalEvent::UserSteered(UserSteered {
                text: "narrow the scope".to_owned(),
            }),
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
                "result": "Wall time: 0.0000 seconds\nProcess running with session ID 7\nOutput:\nrunning",
                "structured_result": {
                    "output": "running",
                    "session_id": 7,
                    "wall_time_seconds": 0.0,
                },
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
                "result": "Wall time: 0.0000 seconds\nProcess exited with code 0\nOutput:\n done",
                "structured_result": {
                    "output": " done",
                    "exit_code": 0,
                    "wall_time_seconds": 0.0,
                },
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
    fn code_mode_shell_followups_remain_visible_workflow_children() {
        let mut model = TranscriptModel::default();
        model.apply(&agent_at(
            1_000,
            AgentEventKind::ToolCall,
            json!({
                "call_id": "workflow",
                "tool": "exec",
                "arguments": "await tools.exec_command({cmd: 'cargo test'})",
            }),
        ));
        model.apply(&agent_at(
            2_000,
            AgentEventKind::ToolCall,
            json!({
                "call_id": "workflow/code-1",
                "tool": "exec_command",
                "arguments": {"cmd": "cargo test"},
            }),
        ));
        model.apply(&agent_at(
            3_000,
            AgentEventKind::ToolResult,
            json!({
                "call_id": "workflow/code-1",
                "tool": "exec_command",
                "status": "completed",
                "duration_ns": 1_u64,
                "result": "Wall time: 0.0000 seconds\nProcess running with session ID 7\nOutput:\nrunning",
                "structured_result": {
                    "output": "running",
                    "session_id": 7,
                    "wall_time_seconds": 0.0,
                },
                "metadata": null,
            }),
        ));

        model.apply(&agent_at(
            4_000,
            AgentEventKind::ToolCall,
            json!({
                "call_id": "workflow/code-2",
                "tool": "write_stdin",
                "arguments": {"session_id": 7},
            }),
        ));

        assert_eq!(model.entries().len(), 3);
        assert_eq!(model.entries()[2].parent, Some(model.entries()[0].id));
        assert!(matches!(
            &model.entries()[2].kind,
            EntryKind::Tool(tool)
                if tool.name == "write_stdin" && tool.state == ToolState::Running
        ));

        model.apply(&agent_at(
            5_000,
            AgentEventKind::ToolResult,
            json!({
                "call_id": "workflow/code-2",
                "tool": "write_stdin",
                "status": "completed",
                "duration_ns": 2_u64,
                "result": "Wall time: 0.0000 seconds\nProcess running with session ID 7\nOutput:\n still",
                "structured_result": {
                    "output": " still",
                    "session_id": 7,
                    "wall_time_seconds": 0.0,
                },
                "metadata": null,
            }),
        ));

        assert!(matches!(
            &model.entries()[1].kind,
            EntryKind::Tool(tool) if tool.state == ToolState::Running
        ));
        assert!(matches!(
            &model.entries()[2].kind,
            EntryKind::Tool(tool) if tool.state == ToolState::Succeeded
        ));

        model.apply(&agent_at(
            6_000,
            AgentEventKind::ToolCall,
            json!({
                "call_id": "workflow/code-3",
                "tool": "write_stdin",
                "arguments": {"session_id": 7},
            }),
        ));
        model.apply(&agent_at(
            7_000,
            AgentEventKind::ToolResult,
            json!({
                "call_id": "workflow/code-3",
                "tool": "write_stdin",
                "status": "completed",
                "duration_ns": 2_u64,
                "result": "Wall time: 0.0000 seconds\nProcess exited with code 0\nOutput:\n done",
                "structured_result": {
                    "output": " done",
                    "exit_code": 0,
                    "wall_time_seconds": 0.0,
                },
                "metadata": null,
            }),
        ));

        for entry in &model.entries()[1..] {
            assert!(matches!(
                &entry.kind,
                EntryKind::Tool(tool) if tool.state == ToolState::Succeeded
            ));
        }
        assert!(model.shell_sessions.is_empty());
        let EntryKind::Tool(shell) = &model.entries()[1].kind else {
            panic!("original shell should remain a tool entry");
        };
        assert_eq!(
            shell.result.as_ref().unwrap()["output"],
            "running still done"
        );
    }

    #[test]
    fn terminating_a_code_cell_after_a_failed_wait_fails_its_unfinished_shell() {
        let mut model = TranscriptModel::default();
        model.apply(&agent(
            AgentEventKind::ToolCall,
            json!({
                "call_id": "workflow",
                "tool": "exec",
                "arguments": "await tools.exec_command({cmd: 'sleep 100'})",
            }),
        ));
        model.apply(&agent(
            AgentEventKind::ToolCall,
            json!({
                "call_id": "workflow/code-1",
                "tool": "exec_command",
                "arguments": {"cmd": "sleep 100"},
            }),
        ));
        model.apply(&agent(
            AgentEventKind::ToolResult,
            json!({
                "call_id": "workflow/code-1",
                "tool": "exec_command",
                "status": "completed",
                "duration_ns": 1_u64,
                "result": "Process running with session ID 7",
                "structured_result": {
                    "output": "",
                    "session_id": 7,
                    "wall_time_seconds": 0.0,
                },
                "metadata": null,
            }),
        ));
        model.apply(&agent(
            AgentEventKind::ToolResult,
            json!({
                "call_id": "workflow",
                "tool": "exec",
                "status": "completed",
                "duration_ns": 2_u64,
                "result": [{"text": "Script running with cell ID 1"}],
                "structured_result": [{"text": "Script running with cell ID 1"}],
                "metadata": null,
            }),
        ));
        model.apply(&agent(
            AgentEventKind::ToolCall,
            json!({
                "call_id": "busy-wait",
                "tool": "wait",
                "arguments": {"cell_id": "1"},
            }),
        ));
        model.apply(&agent(
            AgentEventKind::ToolResult,
            json!({
                "call_id": "busy-wait",
                "tool": "wait",
                "status": "failed",
                "duration_ns": 3_u64,
                "result": [{
                    "text": "Script failed\nWall time: 0.001 seconds\nOutput:\nexec cell 1 already has an active observer"
                }],
                "structured_result": [{
                    "text": "Script failed\nWall time: 0.001 seconds\nOutput:\nexec cell 1 already has an active observer"
                }],
                "metadata": null,
            }),
        ));
        assert!(model.has_running_tools());

        model.apply(&agent(
            AgentEventKind::ToolCall,
            json!({
                "call_id": "terminate",
                "tool": "wait",
                "arguments": {"cell_id": "1", "terminate": true},
            }),
        ));
        model.apply(&agent(
            AgentEventKind::ToolResult,
            json!({
                "call_id": "terminate",
                "tool": "wait",
                "status": "completed",
                "duration_ns": 4_u64,
                "result": [{"text": "Script terminated"}],
                "structured_result": [{"text": "Script terminated"}],
                "metadata": null,
            }),
        ));

        assert!(!model.has_running_tools());
        assert!(matches!(
            &model.entries()[1].kind,
            EntryKind::Tool(tool)
                if tool.state == ToolState::Failed
                    && tool.result.as_ref().and_then(|result| result["error"].as_str())
                        == Some("tool call ended without a terminal result")
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
                "result": "Wall time: 0.0000 seconds\nOutput:\n",
                "structured_result": {"output": "", "wall_time_seconds": 0.0},
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

    #[test]
    fn code_mode_tools_promote_into_nested_chronological_entries() {
        let mut model = TranscriptModel::default();
        model.apply(&agent(
            AgentEventKind::ToolCall,
            json!({
                "call_id": "workflow",
                "tool": "exec",
                "arguments": "await tools.exec_command({cmd: 'cargo test'})",
            }),
        ));
        let parent_revision = model.entries()[0].revision;

        model.apply(&agent(
            AgentEventKind::ToolCall,
            json!({
                "call_id": "workflow/code-1",
                "tool": "exec_command",
                "arguments": {"cmd": "cargo test"},
            }),
        ));

        assert_eq!(model.entries().len(), 2);
        assert!(model.entries()[0].hidden);
        let EntryKind::Tool(workflow) = &model.entries()[0].kind else {
            panic!("code workflow should remain a tool entry");
        };
        assert_eq!(workflow.child_count, 1);
        assert!(model.entries()[0].revision > parent_revision);
        assert!(model.entries()[0].trailing_spacer);
        assert_eq!(model.entries()[1].parent, None);
        assert!(model.entries()[1].trailing_spacer);
        assert!(matches!(
            &model.entries()[1].kind,
            EntryKind::Tool(tool)
                if tool.name == "exec_command" && tool.state == ToolState::Running
        ));

        model.apply(&agent(
            AgentEventKind::ToolResult,
            json!({
                "call_id": "workflow/code-1",
                "tool": "exec_command",
                "status": "completed",
                "duration_ns": 10,
                "result": "Wall time: 0.0000 seconds\nProcess exited with code 0\nOutput:\nok",
                "structured_result": {
                    "output": "ok",
                    "exit_code": 0,
                    "wall_time_seconds": 0.0,
                },
                "metadata": null,
            }),
        ));

        assert!(matches!(
            &model.entries()[1].kind,
            EntryKind::Tool(tool)
                if tool.state == ToolState::Succeeded
                    && tool.result.as_ref().unwrap()["output"] == "ok"
        ));

        model.apply(&agent(
            AgentEventKind::ToolCall,
            json!({
                "call_id": "workflow/code-2",
                "tool": "memory",
                "arguments": {"operation": "scan", "query": "test"},
            }),
        ));

        let EntryKind::Tool(workflow) = &model.entries()[0].kind else {
            panic!("code workflow should remain a tool entry");
        };
        assert_eq!(workflow.child_count, 2);
        assert!(!model.entries()[0].hidden);
        assert!(!model.entries()[0].trailing_spacer);
        assert_eq!(model.entries()[1].parent, Some(model.entries()[0].id));
        assert_eq!(model.entries()[2].parent, Some(model.entries()[0].id));
        assert!(!model.entries()[1].trailing_spacer);
        assert!(model.entries()[2].trailing_spacer);
    }

    #[test]
    fn late_code_children_remain_with_the_original_batch() {
        let mut model = TranscriptModel::default();
        model.apply(&agent(
            AgentEventKind::ToolCall,
            json!({"call_id": "workflow", "tool": "exec", "arguments": "text('start')"}),
        ));
        for child in 1..=2 {
            model.apply(&agent(
                AgentEventKind::ToolCall,
                json!({
                    "call_id": format!("workflow/code-{child}"),
                    "tool": "wait",
                    "arguments": {"cell_id": "cell"},
                }),
            ));
        }
        model.apply(&agent(
            AgentEventKind::AssistantMessage,
            json!({
                "model_call_index": 1,
                "item_id": "message",
                "phase": "final_answer",
                "text": "still waiting",
            }),
        ));
        model.apply(&agent(
            AgentEventKind::ToolCall,
            json!({
                "call_id": "workflow/code-3",
                "tool": "wait",
                "arguments": {"cell_id": "cell"},
            }),
        ));

        let parent = model.entries()[0].id;
        assert!(
            model.entries()[1..=3]
                .iter()
                .all(|entry| entry.parent == Some(parent))
        );
        assert!(matches!(
            &model.entries()[4].kind,
            EntryKind::Assistant { text, .. } if text == "still waiting"
        ));
    }

    #[test]
    fn resumed_single_code_child_keeps_its_workflow_association_and_timeline_position() {
        let mut model = TranscriptModel::default();
        model.apply(&agent(
            AgentEventKind::ToolCall,
            json!({"call_id": "workflow", "tool": "exec", "arguments": "text('start')"}),
        ));
        let parent = model.entries()[0].id;
        model.apply(&agent(
            AgentEventKind::ToolResult,
            json!({
                "call_id": "workflow",
                "tool": "exec",
                "status": "completed",
                "duration_ns": 10,
                "result": [{"text": "Script running"}],
                "structured_result": [{"text": "Script running"}],
                "metadata": null,
            }),
        ));
        model.apply(&agent(
            AgentEventKind::ToolCall,
            json!({"call_id": "wait-1", "tool": "wait", "arguments": {"cell_id": "1"}}),
        ));
        model.apply(&agent(
            AgentEventKind::ToolCall,
            json!({
                "call_id": "workflow/code-2",
                "tool": "apply_patch",
                "arguments": "*** Begin Patch\n*** End Patch",
            }),
        ));

        assert_eq!(model.entries().len(), 3);
        assert!(model.entries()[1].hidden);
        assert_eq!(model.entries()[2].parent, None);
        assert_eq!(model.code_children[&parent], vec![model.entries()[2].id]);
        assert!(matches!(
            &model.entries()[2].kind,
            EntryKind::Tool(tool) if tool.name == "apply_patch"
        ));
    }

    #[test]
    fn only_canonical_code_child_ids_are_nested() {
        let mut model = TranscriptModel::default();
        model.apply(&agent(
            AgentEventKind::ToolCall,
            json!({"call_id": "workflow", "tool": "exec", "arguments": "text('start')"}),
        ));
        model.apply(&agent(
            AgentEventKind::ToolCall,
            json!({
                "call_id": "workflow/not-code-1",
                "tool": "memory",
                "arguments": {"operation": "scan", "query": "test"},
            }),
        ));

        assert_eq!(model.entries()[1].parent, None);
        let EntryKind::Tool(workflow) = &model.entries()[0].kind else {
            panic!("workflow should remain a tool entry");
        };
        assert_eq!(workflow.child_count, 0);
    }

    #[test]
    fn code_mode_waits_remain_visible_workflow_children() {
        let mut model = TranscriptModel::default();
        model.apply(&agent(
            AgentEventKind::ToolCall,
            json!({"call_id": "workflow", "tool": "exec", "arguments": "await tools.wait({})"}),
        ));
        model.apply(&agent(
            AgentEventKind::ToolCall,
            json!({
                "call_id": "workflow/code-1",
                "tool": "wait",
                "arguments": {"cell_id": "cell-1"},
            }),
        ));

        assert!(!model.entries()[1].hidden);
        assert_eq!(model.entries()[1].parent, None);
        assert_eq!(
            model.code_children[&model.entries()[0].id],
            vec![model.entries()[1].id]
        );
    }
}
