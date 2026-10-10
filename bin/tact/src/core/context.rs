//! Content-free context diagnostics projected from transcript telemetry.

use crate::core::transcript::{CompactionFinished, ContextObserved, LocalKind, TranscriptRecord};
use nanocodex::{
    agent::events::AgentEventKind,
    oai::{
        self,
        events::{CompactionStarted, ModelCallCompleted},
        responses::Usage,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use std::{borrow::Cow, collections::VecDeque, mem};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ContinuationMode {
    FullContext,
    PreviousResponse,
}

pub(crate) const MODEL_WINDOW_TOKENS: u64 = oai::CONTEXT_WINDOW_TOKENS;
pub(crate) const AUTO_COMPACT_TOKEN_LIMIT: u64 = 244_800;

/// Distinct tool names tracked individually; any further names share one overflow entry.
const TRACKED_TOOLS: usize = 64;
/// Tools listed in a breakdown, the last one merging every remaining tool when needed.
const SHOWN_TOOLS: usize = 12;
const LARGEST_ITEMS: usize = 6;
const HISTORY_CALLS: usize = 120;
const OTHER_TOOLS: &str = "(other tools)";
/// A rough text density, used only to separate the fixed prefix or compacted summary from new
/// input on the first call of a context. Every later step is measured by the server.
const BYTES_PER_TOKEN: u64 = 4;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ContextBudget {
    pub(crate) active_tokens: u64,
    pub(crate) window_tokens: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub(crate) struct TokenUsage {
    pub(crate) input: u64,
    pub(crate) cached_input: u64,
    pub(crate) uncached_input: u64,
    pub(crate) output: u64,
    pub(crate) total: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct CompactionDiagnostics {
    pub(crate) trigger: CompactionTrigger,
    pub(crate) started_at_unix_ms: u64,
    pub(crate) completed_at_unix_ms: Option<u64>,
    pub(crate) before_tokens: Option<u64>,
    pub(crate) after_tokens: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CompactionTrigger {
    Automatic,
    Manual,
}

/// Where a slice of the active context came from.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ContextCategory {
    /// Instructions, tool definitions, and any other fixed prefix.
    Prefix,
    /// Prompts and steering messages.
    User,
    /// Assistant message text.
    Assistant,
    /// Model reasoning.
    Reasoning,
    /// Tool call arguments.
    ToolCalls,
    /// Tool results.
    ToolOutput,
    /// The summary that replaced earlier history at the last compaction.
    Compacted,
    /// Input the transcript cannot account for, such as images.
    Other,
}

impl ContextCategory {
    const ALL: [Self; 8] = [
        Self::Prefix,
        Self::User,
        Self::Assistant,
        Self::Reasoning,
        Self::ToolCalls,
        Self::ToolOutput,
        Self::Compacted,
        Self::Other,
    ];
}

/// The tokens one category holds in the active context.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct CategoryUsage {
    pub(crate) kind: ContextCategory,
    pub(crate) tokens: u64,
    /// How many transcript items were attributed to the category since the last compaction.
    pub(crate) items: u64,
}

/// What one tool contributed to the active context.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ToolUsage {
    pub(crate) name: String,
    pub(crate) calls: u64,
    pub(crate) call_tokens: u64,
    pub(crate) output_tokens: u64,
}

/// One of the largest single items in the active context. Carries no content.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct LargestItem {
    pub(crate) kind: ContextCategory,
    /// The tool that produced the item, for tool calls and results.
    pub(crate) tool: Option<String>,
    /// The prompt, counted from 1, during which the item entered the context.
    pub(crate) turn: u64,
    pub(crate) tokens: u64,
}

/// An attribution of the active context to its sources.
///
/// The categories sum to `input_tokens`, the server-reported input size of the latest model call.
/// Each growth step between calls is measured by the server and split among the items that caused
/// it in proportion to their size, so the shares are estimates and the total is exact.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ContextBreakdown {
    pub(crate) input_tokens: u64,
    /// Every category in display order, including empty ones.
    pub(crate) categories: Vec<CategoryUsage>,
    /// Tools by descending output tokens.
    pub(crate) tools: Vec<ToolUsage>,
    /// The largest items by descending tokens.
    pub(crate) largest: Vec<LargestItem>,
}

/// One model call on the context-size history.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct CallPoint {
    /// The call's index within the session.
    pub(crate) call: u64,
    pub(crate) input: u64,
    pub(crate) cached: u64,
    pub(crate) output: u64,
    /// Whether a compaction finished just before this call.
    pub(crate) after_compaction: bool,
}

/// A count-only projection that never retains request content or opaque identifiers.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ContextDiagnostics {
    pub(crate) model_window_tokens: u64,
    pub(crate) auto_compact_token_limit: Option<u64>,
    pub(crate) active_tokens: Option<u64>,
    pub(crate) usage: Option<TokenUsage>,
    pub(crate) continuation: Option<ContinuationMode>,
    pub(crate) prompt_cache: Option<bool>,
    pub(crate) compactions_started: u64,
    pub(crate) compactions_completed: u64,
    pub(crate) last_compaction: Option<CompactionDiagnostics>,
    /// Absent until a model call reports usage, and for models that report none. Only a
    /// [snapshot](Self::snapshot) carries it: observing a record keeps just the running attribution,
    /// so a restored session does not build a breakdown for every call it replays.
    pub(crate) breakdown: Option<ContextBreakdown>,
    /// The most recent calls, oldest first, bounded so a long session stays small. Only a
    /// [snapshot](Self::snapshot) carries it.
    pub(crate) history: Vec<CallPoint>,
    #[serde(skip)]
    awaiting_post_compaction_usage: bool,
    #[serde(skip)]
    manual_compaction: bool,
    #[serde(skip)]
    attribution: Box<Attribution>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ContextObservation {
    pub(crate) completed_tokens: Option<u64>,
}

impl Default for ContextDiagnostics {
    fn default() -> Self {
        Self {
            model_window_tokens: MODEL_WINDOW_TOKENS,
            auto_compact_token_limit: Some(AUTO_COMPACT_TOKEN_LIMIT),
            active_tokens: None,
            usage: None,
            continuation: None,
            prompt_cache: None,
            compactions_started: 0,
            compactions_completed: 0,
            last_compaction: None,
            breakdown: None,
            history: Vec::new(),
            awaiting_post_compaction_usage: false,
            manual_compaction: false,
            attribution: Box::default(),
        }
    }
}

impl ContextDiagnostics {
    #[cfg(test)]
    fn rebuild<'a>(records: impl IntoIterator<Item = &'a TranscriptRecord>) -> Self {
        let mut diagnostics = Self::default();
        for record in records {
            diagnostics.observe(record);
        }
        diagnostics.snapshot()
    }

    /// The projection with its breakdown and call history filled in, for a view to show. Building
    /// them walks every tool and call, so it happens when a view asks and not on every record.
    pub(crate) fn snapshot(&self) -> Self {
        Self {
            model_window_tokens: self.model_window_tokens,
            auto_compact_token_limit: self.auto_compact_token_limit,
            active_tokens: self.active_tokens,
            usage: self.usage,
            continuation: self.continuation,
            prompt_cache: self.prompt_cache,
            compactions_started: self.compactions_started,
            compactions_completed: self.compactions_completed,
            last_compaction: self.last_compaction,
            breakdown: self
                .attribution
                .latest_input
                .map(|input| self.attribution.breakdown(input)),
            history: self.attribution.history.iter().copied().collect(),
            awaiting_post_compaction_usage: self.awaiting_post_compaction_usage,
            manual_compaction: self.manual_compaction,
            attribution: Box::default(),
        }
    }

    pub(crate) fn observe(&mut self, record: &TranscriptRecord) -> ContextObservation {
        if let Some(kind) = record.agent_kind() {
            return self.observe_agent(kind, record);
        }
        match record.local_kind() {
            Some(LocalKind::CompactionStarted) => {
                self.manual_compaction = true;
                self.compactions_started = self.compactions_started.saturating_add(1);
                self.last_compaction = Some(CompactionDiagnostics {
                    trigger: CompactionTrigger::Manual,
                    started_at_unix_ms: record.recorded_at_unix_ms(),
                    completed_at_unix_ms: None,
                    before_tokens: self.active_tokens,
                    after_tokens: None,
                });
                self.awaiting_post_compaction_usage = false;
                ContextObservation::default()
            }
            Some(LocalKind::CompactionFinished) => {
                if let Ok(CompactionFinished { error: None, .. }) = record.decode_payload() {
                    self.observe_compaction_completed(record);
                }
                ContextObservation::default()
            }
            Some(LocalKind::ContextBudget) => {
                let Ok(budget) = record.decode_payload::<ContextBudget>() else {
                    return ContextObservation::default();
                };
                if budget.window_tokens == 0 {
                    return ContextObservation::default();
                }
                self.set_native_budget(budget);
                ContextObservation {
                    completed_tokens: Some(budget.active_tokens),
                }
            }
            Some(LocalKind::ContextObserved) => {
                self.observe_context_snapshot(record);
                ContextObservation::default()
            }
            _ => ContextObservation::default(),
        }
    }

    fn observe_agent(
        &mut self,
        kind: AgentEventKind,
        record: &TranscriptRecord,
    ) -> ContextObservation {
        self.attribution.observe(kind, record);
        match kind {
            AgentEventKind::ApiEvent => self.observe_api_event(record),
            AgentEventKind::ModelCallCompleted => self.observe_model_call_completed(record),
            AgentEventKind::RunStarted => {
                self.manual_compaction = false;
                ContextObservation::default()
            }
            AgentEventKind::ModelCompactionStarted | AgentEventKind::ModelCompactionCompleted
                if self.manual_compaction =>
            {
                ContextObservation::default()
            }
            AgentEventKind::ModelCompactionStarted => {
                self.observe_compaction_started(record);
                ContextObservation::default()
            }
            AgentEventKind::ModelCompactionCompleted => {
                self.observe_compaction_completed(record);
                ContextObservation::default()
            }
            _ => ContextObservation::default(),
        }
    }

    pub(crate) fn set_native_budget(&mut self, budget: ContextBudget) {
        self.model_window_tokens = budget.window_tokens;
        self.active_tokens = Some(budget.active_tokens);
        self.auto_compact_token_limit = None;
        if self.awaiting_post_compaction_usage {
            if let Some(compaction) = &mut self.last_compaction {
                compaction.after_tokens = Some(budget.active_tokens);
            }
            self.awaiting_post_compaction_usage = false;
        }
    }

    fn observe_api_event(&mut self, record: &TranscriptRecord) -> ContextObservation {
        let Ok(payload) = record.decode_payload::<ApiEvent>() else {
            return ContextObservation::default();
        };
        if payload.phase != "generation" {
            return ContextObservation::default();
        }
        match payload.direction {
            "outbound" => {
                self.observe_request(payload.event);
                ContextObservation::default()
            }
            "inbound" => self.observe_response_event(payload.event),
            _ => ContextObservation::default(),
        }
    }

    fn observe_request(&mut self, request: &RawValue) {
        let Ok(request) = serde_json::from_str::<ApiRequest>(request.get()) else {
            return;
        };
        self.prompt_cache = Some(request.prompt_cache_key.is_some_and(raw_value_is_string));
        self.continuation = Some(
            if request
                .previous_response_id
                .is_some_and(raw_value_is_string)
            {
                ContinuationMode::PreviousResponse
            } else {
                ContinuationMode::FullContext
            },
        );
    }

    fn observe_response_event(&mut self, event: &RawValue) -> ContextObservation {
        let Ok(event) = serde_json::from_str::<ResponseEvent>(event.get()) else {
            return ContextObservation::default();
        };
        if event.kind != "response.completed" {
            return ContextObservation::default();
        }
        let usage = event
            .response
            .and_then(|response| response.usage)
            .map(usage_into_tokens);
        let completed_tokens = usage.map(|usage| usage.total);
        self.set_usage(usage);
        ContextObservation { completed_tokens }
    }

    fn observe_model_call_completed(&mut self, record: &TranscriptRecord) -> ContextObservation {
        let Ok(payload) = record.decode_payload::<ModelCallCompleted>() else {
            return ContextObservation::default();
        };
        let Some(usage) = payload.usage else {
            // Without a measurement there is nothing to split; dropping the pending items also
            // keeps them from accumulating for models that never report usage.
            self.attribution.produced.clear();
            self.attribution.arrived.clear();
            return ContextObservation::default();
        };
        let reasoning = usage
            .output_tokens_details
            .as_ref()
            .map_or(0, |details| details.reasoning_tokens);
        let usage = usage_into_tokens(usage);
        self.set_usage(Some(usage));
        self.attribution
            .complete_call(payload.call_index, usage, reasoning);
        ContextObservation {
            completed_tokens: Some(usage.total),
        }
    }

    fn observe_context_snapshot(&mut self, record: &TranscriptRecord) {
        let Ok(snapshot) = record.decode_payload::<ContextObserved>() else {
            return;
        };
        self.prompt_cache = Some(snapshot.prompt_cache);
        self.continuation = Some(if snapshot.previous_response {
            ContinuationMode::PreviousResponse
        } else {
            ContinuationMode::FullContext
        });
    }

    fn set_usage(&mut self, usage: Option<TokenUsage>) {
        let Some(usage) = usage else {
            return;
        };
        self.active_tokens = Some(usage.total);
        self.usage = Some(usage);
        if self.awaiting_post_compaction_usage {
            if let Some(compaction) = &mut self.last_compaction {
                compaction.after_tokens = Some(usage.input);
            }
            self.awaiting_post_compaction_usage = false;
        }
    }

    fn observe_compaction_started(&mut self, record: &TranscriptRecord) {
        let payload = record.decode_payload::<CompactionStarted>().ok();
        let before_tokens = payload
            .as_ref()
            .map(|payload| payload.active_context_tokens);
        if let Some(payload) = payload {
            self.auto_compact_token_limit = Some(payload.auto_compact_token_limit);
        }
        self.compactions_started = self.compactions_started.saturating_add(1);
        self.last_compaction = Some(CompactionDiagnostics {
            trigger: CompactionTrigger::Automatic,
            started_at_unix_ms: record.recorded_at_unix_ms(),
            completed_at_unix_ms: None,
            before_tokens,
            after_tokens: None,
        });
        self.awaiting_post_compaction_usage = false;
    }

    fn observe_compaction_completed(&mut self, record: &TranscriptRecord) {
        self.compactions_completed = self.compactions_completed.saturating_add(1);
        self.attribution.compacted();
        if let Some(compaction) = &mut self.last_compaction {
            compaction.completed_at_unix_ms = Some(record.recorded_at_unix_ms());
            self.awaiting_post_compaction_usage = true;
        }
    }
}

/// Identifies one model call. Call indices restart with every run, so the run disambiguates them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CallStamp {
    run: u64,
    index: u32,
}

/// A transcript item waiting for the measured growth it caused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingItem {
    kind: ContextCategory,
    /// An index into `Attribution::tools`.
    tool: Option<usize>,
    turn: u64,
    bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AttributedItem {
    kind: ContextCategory,
    tool: Option<usize>,
    turn: u64,
    tokens: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PreviousCall {
    stamp: CallStamp,
    output: u64,
    reasoning: u64,
}

/// Running state behind [`ContextBreakdown`] and the call history.
///
/// Each call's input equals the previous call's input and output plus whatever entered the context
/// in between. When a call reports usage, the previous call's output is split among the items it
/// produced and the remaining growth among the items that arrived since, both by size. Category
/// totals are whole tokens that always sum to the latest input, so no final rounding is needed.
/// Per-record work is a small decode and a push; the heavier accounting runs once per call.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Attribution {
    tokens: [u64; ContextCategory::ALL.len()],
    items: [u64; ContextCategory::ALL.len()],
    /// At most `TRACKED_TOOLS` named entries, then one `OTHER_TOOLS` overflow entry.
    tools: Vec<ToolUsage>,
    /// The largest attributed items, by descending tokens.
    largest: Vec<AttributedItem>,
    /// Assistant text and tool calls awaiting the output size of the call that produced them.
    produced: Vec<(CallStamp, PendingItem)>,
    /// Prompts, steers, and tool results awaiting the next call's input size.
    arrived: Vec<PendingItem>,
    /// The last call that reported usage, absent at the start of a context.
    previous: Option<PreviousCall>,
    compaction_pending: bool,
    /// The number of runs started, each delivering one prompt.
    runs: u64,
    /// The number of calls that reported usage.
    calls: u64,
    /// The most recent calls, oldest first.
    history: VecDeque<CallPoint>,
    /// The input size of the latest call that reported usage, which the categories add up to.
    latest_input: Option<u64>,
}

impl Attribution {
    fn observe(&mut self, kind: AgentEventKind, record: &TranscriptRecord) {
        if !matches!(
            kind,
            AgentEventKind::RunStarted
                | AgentEventKind::RunSteered
                | AgentEventKind::AssistantMessage
                | AgentEventKind::ToolCall
                | AgentEventKind::ToolResult
        ) {
            return;
        }
        if kind == AgentEventKind::RunStarted {
            self.runs += 1;
        }
        let Ok(payload) = record.decode_payload::<SizedPayload>() else {
            return;
        };
        match (kind, payload) {
            (
                AgentEventKind::RunStarted | AgentEventKind::RunSteered,
                SizedPayload {
                    instruction_bytes: Some(bytes),
                    ..
                },
            ) => self.arrive(ContextCategory::User, None, bytes),
            (
                AgentEventKind::AssistantMessage,
                SizedPayload {
                    model_call_index: Some(index),
                    text: Some(text),
                    ..
                },
            ) => self.produce(
                index,
                ContextCategory::Assistant,
                None,
                json_text_bytes(text),
            ),
            (
                AgentEventKind::ToolCall,
                SizedPayload {
                    model_call_index: Some(index),
                    tool: Some(tool),
                    arguments: Some(arguments),
                    ..
                },
            ) => {
                let tool = self.tool_slot(&tool);
                self.produce(
                    index,
                    ContextCategory::ToolCalls,
                    Some(tool),
                    json_text_bytes(arguments),
                );
            }
            (
                AgentEventKind::ToolResult,
                SizedPayload {
                    tool: Some(tool),
                    result: Some(result),
                    ..
                },
            ) => {
                let tool = self.tool_slot(&tool);
                self.arrive(
                    ContextCategory::ToolOutput,
                    Some(tool),
                    json_text_bytes(result),
                );
            }
            _ => {}
        }
    }

    fn arrive(&mut self, kind: ContextCategory, tool: Option<usize>, bytes: u64) {
        self.arrived.push(PendingItem {
            kind,
            tool,
            turn: self.runs,
            bytes,
        });
    }

    fn produce(&mut self, index: u32, kind: ContextCategory, tool: Option<usize>, bytes: u64) {
        let stamp = CallStamp {
            run: self.runs,
            index,
        };
        let item = PendingItem {
            kind,
            tool,
            turn: self.runs,
            bytes,
        };
        self.produced.push((stamp, item));
    }

    fn tool_slot(&mut self, name: &str) -> usize {
        if let Some(slot) = self.tools.iter().position(|tool| tool.name == name) {
            return slot;
        }
        if self.tools.len() > TRACKED_TOOLS {
            return TRACKED_TOOLS;
        }
        let name = if self.tools.len() < TRACKED_TOOLS {
            name
        } else {
            OTHER_TOOLS
        };
        self.tools.push(ToolUsage {
            name: name.to_owned(),
            calls: 0,
            call_tokens: 0,
            output_tokens: 0,
        });
        self.tools.len() - 1
    }

    /// Accounts for the growth a completed call measured and adds the call to the history.
    fn complete_call(&mut self, index: u32, usage: TokenUsage, reasoning: u64) {
        let stamp = CallStamp {
            run: self.runs,
            index,
        };
        let mut arrived = mem::take(&mut self.arrived);
        // Output can be recorded before its call completes; it belongs to the next measurement.
        let (current, prior): (Vec<_>, Vec<_>) = mem::take(&mut self.produced)
            .into_iter()
            .partition(|(produced_by, _)| *produced_by == stamp);
        self.produced = current;
        let prior = prior.into_iter().map(|(_, item)| item);
        let previous = self.previous.take();
        match previous {
            Some(previous) => {
                let reasoning = previous.reasoning.min(previous.output);
                if reasoning > 0 {
                    self.add(AttributedItem {
                        kind: ContextCategory::Reasoning,
                        tool: None,
                        turn: previous.stamp.run,
                        tokens: reasoning,
                    });
                }
                self.attribute(
                    previous.output - reasoning,
                    prior.collect(),
                    ContextCategory::Assistant,
                );
            }
            None => arrived.extend(prior),
        }

        let held: u64 = self.tokens.iter().sum();
        if let Some(mut growth) = usage.input.checked_sub(held) {
            if previous.is_none() {
                let estimate = arrived.iter().map(|item| item.bytes).sum::<u64>() / BYTES_PER_TOKEN;
                let baseline = growth.saturating_sub(estimate);
                growth -= baseline;
                let kind = if self.compaction_pending {
                    ContextCategory::Compacted
                } else {
                    ContextCategory::Prefix
                };
                self.tokens[kind as usize] += baseline;
                if kind == ContextCategory::Compacted && baseline > 0 {
                    self.items[kind as usize] += 1;
                }
            }
            self.attribute(growth, arrived, ContextCategory::Other);
        } else {
            // The context shrank without a compaction, so no item can own the difference. Scaling
            // keeps every share proportional and the total exact.
            self.attribute(0, arrived, ContextCategory::Other);
            let scaled = apportion(usage.input, &self.tokens);
            self.tokens.copy_from_slice(&scaled);
        }

        self.previous = Some(PreviousCall {
            stamp,
            output: usage.output,
            reasoning,
        });
        self.calls += 1;
        if self.history.len() == HISTORY_CALLS {
            self.history.pop_front();
        }
        self.history.push_back(CallPoint {
            call: self.calls,
            input: usage.input,
            cached: usage.cached_input,
            output: usage.output,
            after_compaction: mem::take(&mut self.compaction_pending),
        });
        self.latest_input = Some(usage.input);
    }

    /// Splits `tokens` among `items` by size, giving any share without a sized owner to `fallback`.
    fn attribute(&mut self, tokens: u64, items: Vec<PendingItem>, fallback: ContextCategory) {
        let weights: Vec<u64> = items.iter().map(|item| item.bytes).collect();
        let shares = apportion(tokens, &weights);
        let mut unowned = tokens;
        for (item, share) in items.into_iter().zip(shares) {
            unowned -= share;
            self.add(AttributedItem {
                kind: item.kind,
                tool: item.tool,
                turn: item.turn,
                tokens: share,
            });
        }
        self.tokens[fallback as usize] += unowned;
    }

    fn add(&mut self, item: AttributedItem) {
        self.tokens[item.kind as usize] += item.tokens;
        self.items[item.kind as usize] += 1;
        if let Some(tool) = item.tool.and_then(|slot| self.tools.get_mut(slot)) {
            match item.kind {
                ContextCategory::ToolCalls => {
                    tool.calls += 1;
                    tool.call_tokens += item.tokens;
                }
                _ => tool.output_tokens += item.tokens,
            }
        }
        if item.tokens == 0 {
            return;
        }
        let position = self
            .largest
            .partition_point(|largest| largest.tokens >= item.tokens);
        if position < LARGEST_ITEMS {
            self.largest.insert(position, item);
            self.largest.truncate(LARGEST_ITEMS);
        }
    }

    /// Starts a new context that keeps only the fixed prefix of the old one. The call history and
    /// the latest input stay: they describe the session, and the next call replaces the input.
    fn compacted(&mut self) {
        let prefix = self.tokens[ContextCategory::Prefix as usize];
        *self = Self {
            runs: self.runs,
            calls: self.calls,
            history: mem::take(&mut self.history),
            latest_input: self.latest_input,
            compaction_pending: true,
            ..Self::default()
        };
        self.tokens[ContextCategory::Prefix as usize] = prefix;
    }

    fn breakdown(&self, input_tokens: u64) -> ContextBreakdown {
        let categories = ContextCategory::ALL
            .into_iter()
            .map(|kind| CategoryUsage {
                kind,
                tokens: self.tokens[kind as usize],
                items: self.items[kind as usize],
            })
            .collect();

        let (tracked, overflow) = self.tools.split_at(self.tools.len().min(TRACKED_TOOLS));
        let mut ranked: Vec<&ToolUsage> = tracked
            .iter()
            .filter(|tool| tool.calls > 0 || tool.output_tokens > 0)
            .collect();
        ranked.sort_by(|left, right| {
            right
                .output_tokens
                .cmp(&left.output_tokens)
                .then_with(|| left.name.cmp(&right.name))
        });
        let spilled = if ranked.len() + overflow.len() > SHOWN_TOOLS {
            ranked.split_off(SHOWN_TOOLS - 1)
        } else {
            Vec::new()
        };
        let mut tools: Vec<ToolUsage> = ranked.into_iter().cloned().collect();
        let merged = spilled.into_iter().chain(overflow).fold(
            ToolUsage {
                name: OTHER_TOOLS.to_owned(),
                calls: 0,
                call_tokens: 0,
                output_tokens: 0,
            },
            |mut merged, tool| {
                merged.calls += tool.calls;
                merged.call_tokens += tool.call_tokens;
                merged.output_tokens += tool.output_tokens;
                merged
            },
        );
        if merged.calls > 0 || merged.output_tokens > 0 {
            tools.push(merged);
        }

        let largest = self
            .largest
            .iter()
            .map(|item| LargestItem {
                kind: item.kind,
                tool: item
                    .tool
                    .and_then(|slot| self.tools.get(slot))
                    .map(|tool| tool.name.clone()),
                turn: item.turn,
                tokens: item.tokens,
            })
            .collect();

        ContextBreakdown {
            input_tokens,
            categories,
            tools,
            largest,
        }
    }
}

/// Splits `total` in proportion to `weights` with largest-remainder rounding, so the shares sum
/// to `total` exactly. Every share is zero when no weight is.
fn apportion(total: u64, weights: &[u64]) -> Vec<u64> {
    let weight_sum: u128 = weights.iter().map(|&weight| u128::from(weight)).sum();
    if weight_sum == 0 {
        return vec![0; weights.len()];
    }
    let mut shares = Vec::with_capacity(weights.len());
    let mut remainders = Vec::with_capacity(weights.len());
    for &weight in weights {
        let exact = u128::from(total) * u128::from(weight);
        // Each share is at most `total`, so it fits in a u64.
        shares.push((exact / weight_sum) as u64);
        remainders.push(exact % weight_sum);
    }
    let leftover = total - shares.iter().sum::<u64>();
    let mut order: Vec<usize> = (0..weights.len()).collect();
    order.sort_by(|&left, &right| remainders[right].cmp(&remainders[left]));
    for &slot in order.iter().take(leftover as usize) {
        shares[slot] += 1;
    }
    shares
}

/// The byte length of a JSON string's text, or the raw length of any other JSON value.
///
/// Every escape sequence stands for fewer bytes than it spells, so subtracting one byte per
/// backslash approximates the decoded length without decoding it. The figure only weights how a
/// measured growth is split among items, which makes that close enough, and counting bytes
/// vectorizes where decoding them does not.
fn json_text_bytes(value: &RawValue) -> u64 {
    let raw = value.get().trim();
    let Some(inner) = raw.strip_prefix('"').and_then(|raw| raw.strip_suffix('"')) else {
        return raw.len() as u64;
    };
    let escapes = inner.bytes().filter(|byte| *byte == b'\\').count();
    (inner.len() - escapes) as u64
}

/// The fields of a record's payload that attribution measures, whichever kind of record it is.
/// They are optional so a record that lacks one is skipped without building a decode error, which
/// costs more than decoding the record.
#[derive(Deserialize)]
struct SizedPayload<'a> {
    instruction_bytes: Option<u64>,
    model_call_index: Option<u32>,
    #[serde(borrow)]
    tool: Option<Cow<'a, str>>,
    #[serde(borrow)]
    text: Option<&'a RawValue>,
    #[serde(borrow)]
    arguments: Option<&'a RawValue>,
    #[serde(borrow)]
    result: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct ApiEvent<'a> {
    direction: &'a str,
    phase: &'a str,
    #[serde(borrow)]
    event: &'a RawValue,
}

#[derive(Deserialize)]
struct ApiRequest<'a> {
    #[serde(borrow)]
    prompt_cache_key: Option<&'a RawValue>,
    #[serde(borrow)]
    previous_response_id: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct ResponseEvent<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    response: Option<Response>,
}

#[derive(Deserialize)]
struct Response {
    usage: Option<Usage>,
}

fn usage_into_tokens(usage: Usage) -> TokenUsage {
    let cached_input = usage
        .input_tokens_details
        .map_or(0, |details| details.cached_tokens);
    TokenUsage {
        input: usage.input_tokens,
        cached_input,
        uncached_input: usage.input_tokens.saturating_sub(cached_input),
        output: usage.output_tokens,
        total: usage.total_tokens,
    }
}

fn raw_value_is_string(value: &RawValue) -> bool {
    value.get().trim_start().starts_with('"')
}

/// The content-free facts a durable transcript keeps from an outbound raw API request.
pub(crate) fn outbound_context_snapshot(record: &TranscriptRecord) -> Option<ContextObserved> {
    let payload = record.decode_payload::<ApiEvent>().ok()?;
    if payload.direction != "outbound" || payload.phase != "generation" {
        return None;
    }
    let request = serde_json::from_str::<ApiRequest>(payload.event.get()).ok()?;
    Some(ContextObserved {
        prompt_cache: request.prompt_cache_key.is_some_and(raw_value_is_string),
        previous_response: request
            .previous_response_id
            .is_some_and(raw_value_is_string),
    })
}

#[cfg(test)]
mod tests {
    use super::{
        CallPoint, CategoryUsage, CompactionDiagnostics, CompactionTrigger, ContextBreakdown,
        ContextCategory, ContextDiagnostics, ContinuationMode, HISTORY_CALLS, OTHER_TOOLS,
        SHOWN_TOOLS, TRACKED_TOOLS, TokenUsage,
    };
    use crate::core::transcript::{
        CompactionFinished, LocalEvent, TranscriptRecord, TurnId, UserSteered, UserSubmitted,
    };
    use nanocodex::agent::events::{AgentEvent, AgentEventKind};
    use serde_json::{
        Value, json,
        value::{RawValue, to_raw_value},
    };
    use std::sync::Arc;

    fn agent(sequence: u64, at: u64, kind: AgentEventKind, payload: Value) -> TranscriptRecord {
        raw_agent(sequence, at, kind, to_raw_value(&payload).unwrap().into())
    }

    fn raw_agent(
        sequence: u64,
        at: u64,
        kind: AgentEventKind,
        payload: Arc<RawValue>,
    ) -> TranscriptRecord {
        TranscriptRecord::from_agent(
            sequence,
            at,
            AgentEvent {
                protocol_version: 1,
                request_id: Arc::from("secret-request-id"),
                seq: sequence,
                kind,
                payload,
            },
        )
    }

    fn api(direction: &str, event: Value) -> Value {
        json!({"direction": direction, "phase": "generation", "event": event})
    }

    fn model_call_completed(call_index: u32, usage: Value) -> Value {
        json!({
            "call_index": call_index,
            "model": "gpt-6.1-sol",
            "response_id": "secret-continuation-token",
            "attempt": 1,
            "connection_generation": 1,
            "status": "completed",
            "duration_ns": 1,
            "time_to_first_event_ns": 1,
            "time_to_first_output_ns": 1,
            "tool_calls": 0,
            "usage": usage
        })
    }

    #[test]
    fn complete_telemetry_projects_only_safe_facts_and_counts() {
        let records = [
            agent(
                1,
                1,
                AgentEventKind::ApiEvent,
                api(
                    "outbound",
                    json!({
                        "prompt_cache_key": "secret-cache-key",
                        "previous_response_id": "secret-response-id",
                        "input": [{"role":"user", "content":"secret prompt"}]
                    }),
                ),
            ),
            agent(
                2,
                2,
                AgentEventKind::ModelCallCompleted,
                model_call_completed(
                    1,
                    json!({
                            "input_tokens": 1_000,
                            "input_tokens_details": {"cached_tokens": 750},
                            "output_tokens": 80,
                            "total_tokens": 1_080
                    }),
                ),
            ),
            agent(
                3,
                100,
                AgentEventKind::ModelCompactionStarted,
                json!({
                    "after_model_call_index": 1,
                    "active_context_tokens": 900,
                    "auto_compact_token_limit": 200_000,
                    "previous_response_id": "secret-response-id"
                }),
            ),
            agent(
                4,
                110,
                AgentEventKind::ModelCompactionCompleted,
                json!({
                    "response_id": "secret-compaction-id"
                }),
            ),
            agent(
                5,
                120,
                AgentEventKind::ModelCallCompleted,
                model_call_completed(
                    1,
                    json!({"input_tokens": 400, "output_tokens": 20, "total_tokens": 420}),
                ),
            ),
        ];
        let diagnostics = ContextDiagnostics::rebuild(records.iter());
        // Comparing the complete public value accounts for every field; each is a count, flag,
        // mode, or tool name, so no request content can be retained.
        assert_eq!(
            diagnostics,
            ContextDiagnostics {
                model_window_tokens: nanocodex::oai::CONTEXT_WINDOW_TOKENS,
                auto_compact_token_limit: Some(200_000),
                active_tokens: Some(420),
                usage: Some(TokenUsage {
                    input: 400,
                    cached_input: 0,
                    uncached_input: 400,
                    output: 20,
                    total: 420,
                }),
                continuation: Some(ContinuationMode::PreviousResponse),
                prompt_cache: Some(true),
                compactions_started: 1,
                compactions_completed: 1,
                last_compaction: Some(CompactionDiagnostics {
                    trigger: CompactionTrigger::Automatic,
                    started_at_unix_ms: 100,
                    completed_at_unix_ms: Some(110),
                    before_tokens: Some(900),
                    after_tokens: Some(400),
                }),
                // The context shrank to below the prefix measured before compaction, so the
                // prefix scales down to the reported input.
                breakdown: Some(ContextBreakdown {
                    input_tokens: 400,
                    categories: categories([(ContextCategory::Prefix, 400, 0)]),
                    tools: Vec::new(),
                    largest: Vec::new(),
                }),
                history: vec![
                    CallPoint {
                        call: 1,
                        input: 1_000,
                        cached: 750,
                        output: 80,
                        after_compaction: false,
                    },
                    CallPoint {
                        call: 2,
                        input: 400,
                        cached: 0,
                        output: 20,
                        after_compaction: true,
                    },
                ],
                awaiting_post_compaction_usage: false,
                manual_compaction: false,
                attribution: diagnostics.attribution.clone(),
            }
        );
    }

    #[test]
    fn partial_and_unavailable_telemetry_remain_explicit() {
        let mut diagnostics = ContextDiagnostics::default();
        diagnostics.observe(&agent(
            1,
            10,
            AgentEventKind::ModelCompactionStarted,
            json!({}),
        ));
        diagnostics.observe(&agent(
            2,
            20,
            AgentEventKind::ModelCompactionCompleted,
            json!({}),
        ));
        diagnostics.observe(&agent(
            3,
            30,
            AgentEventKind::ApiEvent,
            api("outbound", json!({})),
        ));

        assert!(diagnostics.usage.is_none());
        assert_eq!(
            diagnostics.continuation,
            Some(ContinuationMode::FullContext)
        );
        assert_eq!(diagnostics.prompt_cache, Some(false));
        assert_eq!(diagnostics.last_compaction.unwrap().before_tokens, None);
        assert_eq!(diagnostics.last_compaction.unwrap().after_tokens, None);
    }

    #[test]
    fn completed_response_total_remains_available_to_the_composer() {
        let record = agent(
            1,
            1,
            AgentEventKind::ApiEvent,
            api(
                "inbound",
                json!({
                    "type": "response.completed",
                    "response": {"usage": {"total_tokens": 136_000}}
                }),
            ),
        );
        let mut diagnostics = ContextDiagnostics::default();
        let observation = diagnostics.observe(&record);

        assert_eq!(observation.completed_tokens, Some(136_000));
        assert_eq!(diagnostics.usage.unwrap().total, 136_000);
    }

    /// A synthetic session whose prompts, messages, and tool traffic carry `SECRET` text.
    #[derive(Default)]
    struct Session {
        records: Vec<TranscriptRecord>,
    }

    impl Session {
        fn sequence(&self) -> u64 {
            self.records.len() as u64 + 1
        }

        fn agent(&mut self, kind: AgentEventKind, payload: Value) -> &mut Self {
            let sequence = self.sequence();
            self.records.push(agent(sequence, sequence, kind, payload));
            self
        }

        /// Appends a record sharing an existing payload, which keeps building huge sessions cheap.
        fn replay(&mut self, (kind, payload): &(AgentEventKind, Arc<RawValue>)) -> &mut Self {
            let sequence = self.sequence();
            self.records
                .push(raw_agent(sequence, sequence, *kind, Arc::clone(payload)));
            self
        }

        fn local(&mut self, event: LocalEvent) -> &mut Self {
            let sequence = self.sequence();
            self.records
                .push(TranscriptRecord::from_local(sequence, sequence, event).unwrap());
            self
        }

        fn prompt(&mut self, bytes: usize) -> &mut Self {
            let id = TurnId::new(self.sequence());
            self.local(LocalEvent::UserSubmitted(UserSubmitted {
                id,
                text: secret(bytes),
            }))
            .agent(
                AgentEventKind::RunStarted,
                json!({"mode": "responses", "instruction_bytes": bytes}),
            )
        }

        fn steer(&mut self, bytes: usize) -> &mut Self {
            self.local(LocalEvent::UserSteered(UserSteered {
                text: secret(bytes),
            }))
            .agent(
                AgentEventKind::RunSteered,
                json!({"steer_index": 1, "instruction_bytes": bytes}),
            )
        }

        fn call(&mut self, index: u32, input: u64, output: u64, reasoning: u64) -> &mut Self {
            self.agent(
                AgentEventKind::ModelCallCompleted,
                model_call_completed(
                    index,
                    json!({
                        "input_tokens": input,
                        "input_tokens_details": {"cached_tokens": input / 2},
                        "output_tokens": output,
                        "output_tokens_details": {"reasoning_tokens": reasoning},
                        "total_tokens": input + output
                    }),
                ),
            )
        }

        fn message(&mut self, index: u32, bytes: usize) -> &mut Self {
            self.agent(
                AgentEventKind::AssistantMessage,
                json!({"model_call_index": index, "item_id": "secret-item", "text": secret(bytes)}),
            )
        }

        fn tool_call(&mut self, index: u32, tool: &str, bytes: usize) -> &mut Self {
            self.agent(
                AgentEventKind::ToolCall,
                json!({
                    "call_id": "secret-call",
                    "tool": tool,
                    "arguments": secret(bytes),
                    "model_call_index": index
                }),
            )
        }

        fn tool_result(&mut self, tool: &str, bytes: usize) -> &mut Self {
            self.agent(
                AgentEventKind::ToolResult,
                json!({
                    "call_id": "secret-call",
                    "tool": tool,
                    "status": "completed",
                    "duration_ns": 1,
                    "result": secret(bytes),
                    "structured_result": {"output": secret(bytes)}
                }),
            )
        }

        fn compaction(&mut self) -> &mut Self {
            self.agent(
                AgentEventKind::ModelCompactionStarted,
                json!({"after_model_call_index": 1, "active_context_tokens": 1}),
            )
            .agent(AgentEventKind::ModelCompactionCompleted, json!({}))
        }

        fn manual_compaction(&mut self) -> &mut Self {
            self.local(LocalEvent::CompactionStarted)
                .local(LocalEvent::CompactionFinished(CompactionFinished {
                    error: None,
                    duration_ns: 1,
                    terminal_stop: None,
                }))
        }

        fn diagnostics(&self) -> ContextDiagnostics {
            ContextDiagnostics::rebuild(self.records.iter())
        }
    }

    /// Text of exactly `bytes` bytes that must never reach a projection.
    fn secret(bytes: usize) -> String {
        let mut text = "SECRET".repeat(bytes.div_ceil(6));
        text.truncate(bytes);
        text
    }

    /// Every category in display order, with the listed `(kind, tokens, items)` and zeros elsewhere.
    fn categories<const N: usize>(entries: [(ContextCategory, u64, u64); N]) -> Vec<CategoryUsage> {
        ContextCategory::ALL
            .into_iter()
            .map(|kind| {
                let (tokens, items) = entries
                    .iter()
                    .find(|(entry, ..)| *entry == kind)
                    .map_or((0, 0), |&(_, tokens, items)| (tokens, items));
                CategoryUsage {
                    kind,
                    tokens,
                    items,
                }
            })
            .collect()
    }

    /// Returns the breakdown after checking that its categories add up to the measured input.
    fn reconciled(diagnostics: &ContextDiagnostics) -> &ContextBreakdown {
        let breakdown = diagnostics
            .breakdown
            .as_ref()
            .expect("a call reported usage");
        let held: u64 = breakdown
            .categories
            .iter()
            .map(|category| category.tokens)
            .sum();
        assert_eq!(held, breakdown.input_tokens);
        assert_eq!(breakdown.input_tokens, diagnostics.usage.unwrap().input);
        breakdown
    }

    fn largest(breakdown: &ContextBreakdown) -> Vec<(ContextCategory, Option<&str>, u64, u64)> {
        breakdown
            .largest
            .iter()
            .map(|item| (item.kind, item.tool.as_deref(), item.turn, item.tokens))
            .collect()
    }

    /// One prompt, three calls, a tool round trip, and assistant text recorded before its call
    /// completes.
    fn tool_round_trip() -> Session {
        let mut session = Session::default();
        session
            .prompt(400)
            .call(1, 1_100, 300, 100)
            .message(1, 200)
            .tool_call(1, "read", 200)
            .tool_result("read", 800)
            .message(2, 40)
            .call(2, 1_650, 50, 0)
            .call(3, 1_700, 10, 0);
        session
    }

    #[test]
    fn measured_growth_is_split_among_the_items_that_caused_it() {
        let diagnostics = tool_round_trip().diagnostics();
        let breakdown = reconciled(&diagnostics);

        // The first call holds the prefix plus the prompt, estimated from its size. Each later
        // call adds the previous output (reasoning exactly, the rest by size) and the measured
        // growth, which here is the tool result.
        assert_eq!(
            breakdown.categories,
            categories([
                (ContextCategory::Prefix, 1_000, 0),
                (ContextCategory::User, 100, 1),
                (ContextCategory::Assistant, 150, 2),
                (ContextCategory::Reasoning, 100, 1),
                (ContextCategory::ToolCalls, 100, 1),
                (ContextCategory::ToolOutput, 250, 1),
            ])
        );
        assert_eq!(
            breakdown.tools,
            [super::ToolUsage {
                name: "read".to_owned(),
                calls: 1,
                call_tokens: 100,
                output_tokens: 250,
            }]
        );
        assert_eq!(
            largest(breakdown),
            [
                (ContextCategory::ToolOutput, Some("read"), 1, 250),
                (ContextCategory::User, None, 1, 100),
                (ContextCategory::Reasoning, None, 1, 100),
                (ContextCategory::Assistant, None, 1, 100),
                (ContextCategory::ToolCalls, Some("read"), 1, 100),
                (ContextCategory::Assistant, None, 1, 50),
            ]
        );
        assert_eq!(
            diagnostics
                .history
                .iter()
                .map(|point| (point.call, point.input, point.cached, point.output))
                .collect::<Vec<_>>(),
            [
                (1, 1_100, 550, 300),
                (2, 1_650, 825, 50),
                (3, 1_700, 850, 10)
            ]
        );
    }

    #[test]
    fn breakdown_serializes_without_any_content() {
        let mut session = tool_round_trip();
        session
            .steer(64)
            .tool_result("shell", 64)
            .call(4, 1_800, 0, 0);
        let serialized = serde_json::to_string(&session.diagnostics()).unwrap();

        assert!(serialized.contains("\"breakdown\":{"));
        assert!(serialized.contains("\"tool\":\"read\""));
        assert!(!serialized.to_lowercase().contains("secret"));
    }

    #[test]
    fn steering_and_tool_results_share_one_growth_step_by_size() {
        let mut session = Session::default();
        session
            .prompt(400)
            .call(1, 1_100, 0, 0)
            .tool_result("shell", 320)
            .steer(80)
            .call(2, 1_200, 0, 0);
        let diagnostics = session.diagnostics();

        assert_eq!(
            reconciled(&diagnostics).categories,
            categories([
                (ContextCategory::Prefix, 1_000, 0),
                (ContextCategory::User, 120, 2),
                (ContextCategory::ToolOutput, 80, 1),
            ])
        );
    }

    #[test]
    fn compaction_keeps_the_prefix_and_measures_the_summary() {
        let mut session = Session::default();
        session
            .prompt(400)
            .call(1, 1_100, 300, 100)
            .tool_call(1, "read", 40)
            .tool_result("read", 400)
            .call(2, 1_500, 0, 0)
            .compaction()
            .tool_result("shell", 400)
            .call(3, 1_400, 20, 0)
            .call(4, 1_420, 0, 0);
        let diagnostics = session.diagnostics();
        let breakdown = reconciled(&diagnostics);

        // Everything after the prefix that new input cannot explain is the summary. Output with no
        // recorded item stays with the assistant.
        assert_eq!(
            breakdown.categories,
            categories([
                (ContextCategory::Prefix, 1_000, 0),
                (ContextCategory::Assistant, 20, 0),
                (ContextCategory::ToolOutput, 100, 1),
                (ContextCategory::Compacted, 300, 1),
            ])
        );
        assert_eq!(
            breakdown
                .tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            ["shell"]
        );
        assert_eq!(
            largest(breakdown),
            [(ContextCategory::ToolOutput, Some("shell"), 1, 100)]
        );
        assert_eq!(
            diagnostics
                .history
                .iter()
                .map(|point| point.after_compaction)
                .collect::<Vec<_>>(),
            [false, false, true, false]
        );
    }

    #[test]
    fn history_keeps_the_newest_calls_and_flags_compactions() {
        let mut session = Session::default();
        session.prompt(400);
        for call in 1..=130 {
            if call == 125 {
                session.manual_compaction();
            }
            session.call(call, 1_000 + u64::from(call), 1, 0);
        }
        let diagnostics = session.diagnostics();
        reconciled(&diagnostics);

        let history = &diagnostics.history;
        assert_eq!(history.len(), HISTORY_CALLS);
        assert_eq!(history.first().unwrap().call, 11);
        assert_eq!(history.last().unwrap().call, 130);
        assert_eq!(
            history
                .iter()
                .filter(|point| point.after_compaction)
                .map(|point| point.call)
                .collect::<Vec<_>>(),
            [125]
        );
    }

    #[test]
    fn tools_are_ranked_by_output_and_bounded() {
        let mut session = Session::default();
        session.prompt(400).call(1, 1_100, 700, 0);
        let names: Vec<String> = (0..70).map(|tool| format!("tool-{tool:02}")).collect();
        for name in &names {
            session.tool_call(1, name, 40);
        }
        for (rank, name) in names.iter().enumerate() {
            session.tool_result(name, 100 * (rank + 1));
        }
        session.call(2, 1_100 + 700 + 62_125, 0, 0);
        let diagnostics = session.diagnostics();
        let breakdown = reconciled(&diagnostics);

        let tools = &breakdown.tools;
        assert_eq!(tools.len(), SHOWN_TOOLS);
        // Names beyond the tracked limit share the overflow entry from their first record, so the
        // largest named tool is the last tracked one.
        assert_eq!(tools[0].name, "tool-63");
        assert!(
            tools[..SHOWN_TOOLS - 1]
                .windows(2)
                .all(|pair| pair[0].output_tokens > pair[1].output_tokens)
        );
        let merged = tools.last().unwrap();
        assert_eq!(merged.name, OTHER_TOOLS);
        assert_eq!(merged.calls, 70 - (SHOWN_TOOLS as u64 - 1));
        let category = |kind: ContextCategory| {
            breakdown
                .categories
                .iter()
                .find(|category| category.kind == kind)
                .unwrap()
                .tokens
        };
        assert_eq!(
            tools.iter().map(|tool| tool.call_tokens).sum::<u64>(),
            category(ContextCategory::ToolCalls)
        );
        assert_eq!(
            tools.iter().map(|tool| tool.output_tokens).sum::<u64>(),
            category(ContextCategory::ToolOutput)
        );
    }

    #[test]
    fn largest_items_name_their_turn_across_restarted_call_indices() {
        let mut session = Session::default();
        session
            .prompt(400)
            .call(1, 1_100, 0, 0)
            .tool_result("read", 4_000)
            .call(2, 2_100, 0, 0)
            .prompt(40)
            // Each run restarts call indices; this text belongs to the second run's first call.
            .message(1, 40)
            .call(1, 2_110, 10, 0)
            .tool_result("shell", 8_000)
            .call(2, 4_120, 0, 0);
        let diagnostics = session.diagnostics();

        assert_eq!(
            largest(reconciled(&diagnostics)),
            [
                (ContextCategory::ToolOutput, Some("shell"), 2, 2_000),
                (ContextCategory::ToolOutput, Some("read"), 1, 1_000),
                (ContextCategory::User, None, 1, 100),
                (ContextCategory::User, None, 2, 10),
                (ContextCategory::Assistant, None, 2, 10),
            ]
        );
    }

    #[test]
    fn calls_without_usage_leave_the_breakdown_unavailable() {
        let mut session = Session::default();
        session
            .prompt(400)
            .agent(
                AgentEventKind::ModelCallCompleted,
                model_call_completed(1, Value::Null),
            )
            .message(1, 40)
            .tool_call(1, "read", 40)
            .tool_result("read", 400)
            .local(LocalEvent::ContextBudget(super::ContextBudget {
                active_tokens: 5_000,
                window_tokens: 200_000,
            }));
        let diagnostics = session.diagnostics();

        assert_eq!(diagnostics.breakdown, None);
        assert!(diagnostics.history.is_empty());
        assert_eq!(diagnostics.active_tokens, Some(5_000));
    }

    #[test]
    fn restoring_a_long_session_keeps_bounded_state() {
        let mut run = Session::default();
        run.agent(
            AgentEventKind::RunStarted,
            json!({"instruction_bytes": 400}),
        );
        for call in 1..=4 {
            run.message(call, 120)
                .tool_call(call, "shell", 80)
                .tool_result("shell", 2_000);
        }
        let run: Vec<(AgentEventKind, Arc<RawValue>)> = run
            .records
            .iter()
            .map(|record| {
                let payload = RawValue::from_string(record.payload_json().to_owned()).unwrap();
                (record.agent_kind().unwrap(), payload.into())
            })
            .collect();
        let (run_started, outputs) = run.split_first().unwrap();

        let mut session = Session::default();
        let mut input = 10_000;
        while session.records.len() < 200_000 {
            session.replay(run_started);
            for (call, outputs) in (1..=4).zip(outputs.chunks(3)) {
                let output = 50;
                session.call(call, input, output, 10);
                for output in outputs {
                    session.replay(output);
                }
                input += output + 500;
            }
            if input > 250_000 {
                session.compaction();
                input = 12_000;
            }
        }

        let diagnostics = session.diagnostics();

        // Restoring replays every record through one pass, so what it keeps must not grow with
        // the session: the history is capped and no record waits for a call that never came.
        reconciled(&diagnostics);
        assert_eq!(diagnostics.history.len(), HISTORY_CALLS);
        assert!(diagnostics.attribution.arrived.len() <= 12);
        assert!(diagnostics.attribution.produced.len() <= 12);
        assert!(diagnostics.attribution.tools.len() <= TRACKED_TOOLS + 1);
    }
}
