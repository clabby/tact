//! Content-free context diagnostics projected from transcript telemetry.

use crate::tui::transcript::TranscriptRecord;
use serde::Deserialize;
use serde_json::Value;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ContinuationMode {
    FullContext,
    PreviousResponse,
}

pub(crate) const MODEL_WINDOW_TOKENS: u64 = 272_000;
pub(crate) const AUTO_COMPACT_TOKEN_LIMIT: u64 = 244_800;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct TokenUsage {
    pub(crate) input: u64,
    pub(crate) cached_input: u64,
    pub(crate) uncached_input: u64,
    pub(crate) output: u64,
    pub(crate) total: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CompactionDiagnostics {
    pub(crate) trigger: CompactionTrigger,
    pub(crate) started_at_unix_ms: u64,
    pub(crate) completed_at_unix_ms: Option<u64>,
    pub(crate) before_tokens: Option<u64>,
    pub(crate) after_tokens: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CompactionTrigger {
    Automatic,
}

/// A count-only projection that never retains request content or opaque identifiers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ContextDiagnostics {
    pub(crate) model_window_tokens: u64,
    pub(crate) auto_compact_token_limit: u64,
    pub(crate) usage: Option<TokenUsage>,
    pub(crate) continuation: Option<ContinuationMode>,
    pub(crate) prompt_cache: Option<bool>,
    pub(crate) compactions_started: u64,
    pub(crate) compactions_completed: u64,
    pub(crate) last_compaction: Option<CompactionDiagnostics>,
    awaiting_post_compaction_usage: bool,
}

impl Default for ContextDiagnostics {
    fn default() -> Self {
        Self {
            model_window_tokens: MODEL_WINDOW_TOKENS,
            auto_compact_token_limit: AUTO_COMPACT_TOKEN_LIMIT,
            usage: None,
            continuation: None,
            prompt_cache: None,
            compactions_started: 0,
            compactions_completed: 0,
            last_compaction: None,
            awaiting_post_compaction_usage: false,
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
        diagnostics
    }

    pub(crate) fn observe(&mut self, record: &TranscriptRecord) {
        match (record.source(), record.kind()) {
            ("agent", "api.event") => self.observe_api_event(record),
            ("agent", "model.call.completed") => self.observe_model_call_completed(record),
            ("agent", "model.compaction.started") => self.observe_compaction_started(record),
            ("agent", "model.compaction.completed") => self.observe_compaction_completed(record),
            _ => {}
        }
    }

    fn observe_api_event(&mut self, record: &TranscriptRecord) {
        let Ok(payload) = record.decode_payload::<ApiEvent>() else {
            return;
        };
        if payload.phase != "generation" {
            return;
        }
        match payload.direction.as_str() {
            "outbound" => self.observe_request(&payload.event),
            "inbound" => self.observe_response_event(&payload.event),
            _ => {}
        }
    }

    fn observe_request(&mut self, request: &Value) {
        self.prompt_cache = Some(
            request
                .get("prompt_cache_key")
                .is_some_and(Value::is_string),
        );
        self.continuation = Some(
            if request
                .get("previous_response_id")
                .is_some_and(Value::is_string)
            {
                ContinuationMode::PreviousResponse
            } else {
                ContinuationMode::FullContext
            },
        );
    }

    fn observe_response_event(&mut self, event: &Value) {
        if event.get("type").and_then(Value::as_str) != Some("response.completed") {
            return;
        }
        let usage = event
            .get("response")
            .and_then(|response| response.get("usage"))
            .and_then(usage_from_value);
        self.set_usage(usage);
    }

    fn observe_model_call_completed(&mut self, record: &TranscriptRecord) {
        let Ok(payload) = record.decode_payload::<ModelCallCompleted>() else {
            return;
        };
        self.set_usage(payload.usage.map(Usage::into_tokens));
    }

    fn set_usage(&mut self, usage: Option<TokenUsage>) {
        let Some(usage) = usage else {
            return;
        };
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
        if let Some(limit) = payload.and_then(|payload| payload.auto_compact_token_limit) {
            self.auto_compact_token_limit = limit;
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
        if let Some(compaction) = &mut self.last_compaction {
            compaction.completed_at_unix_ms = Some(record.recorded_at_unix_ms());
            self.awaiting_post_compaction_usage = true;
        }
    }
}

#[derive(Deserialize)]
struct ApiEvent {
    direction: String,
    phase: String,
    event: Value,
}

#[derive(Deserialize)]
struct ModelCallCompleted {
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct CompactionStarted {
    active_context_tokens: u64,
    #[serde(default)]
    auto_compact_token_limit: Option<u64>,
}

#[derive(Clone, Copy, Deserialize)]
struct Usage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    input_tokens_details: Option<InputTokenDetails>,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    total_tokens: u64,
}

impl Usage {
    fn into_tokens(self) -> TokenUsage {
        let cached_input = self
            .input_tokens_details
            .map_or(0, |details| details.cached_tokens);
        TokenUsage {
            input: self.input_tokens,
            cached_input,
            uncached_input: self.input_tokens.saturating_sub(cached_input),
            output: self.output_tokens,
            total: self.total_tokens,
        }
    }
}

#[derive(Clone, Copy, Deserialize)]
struct InputTokenDetails {
    #[serde(default)]
    cached_tokens: u64,
}

pub(crate) fn completed_transcript_tokens(record: &TranscriptRecord) -> Option<u64> {
    if record.source() != "agent" || record.kind() != "api.event" {
        return None;
    }
    let payload = record.decode_payload::<ApiEvent>().ok()?;
    if payload.direction != "inbound" || payload.phase != "generation" {
        return None;
    }
    let response = payload.event.get("response")?;
    (payload.event.get("type")?.as_str()? == "response.completed")
        .then(|| response.get("usage"))
        .flatten()?
        .get("total_tokens")?
        .as_u64()
}

fn usage_from_value(value: &Value) -> Option<TokenUsage> {
    serde_json::from_value::<Usage>(value.clone())
        .ok()
        .map(Usage::into_tokens)
}

#[cfg(test)]
mod tests {
    use super::{ContextDiagnostics, ContinuationMode, completed_transcript_tokens};
    use crate::tui::transcript::TranscriptRecord;
    use nanocodex::{AgentEvent, AgentEventKind};
    use serde_json::{Value, json, value::to_raw_value};
    use std::sync::Arc;

    fn agent(sequence: u64, at: u64, kind: AgentEventKind, payload: Value) -> TranscriptRecord {
        TranscriptRecord::from_agent(
            sequence,
            at,
            AgentEvent {
                protocol_version: 1,
                request_id: Arc::from("secret-request-id"),
                seq: sequence,
                kind,
                payload: to_raw_value(&payload).unwrap(),
            },
        )
    }

    fn api(direction: &str, event: Value) -> Value {
        json!({"direction": direction, "phase": "generation", "event": event})
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
                json!({
                    "response_id": "secret-continuation-token",
                    "usage": {
                        "input_tokens": 1_000,
                        "input_tokens_details": {"cached_tokens": 750},
                        "output_tokens": 80,
                        "total_tokens": 1_080
                    }
                }),
            ),
            agent(
                3,
                100,
                AgentEventKind::ModelCompactionStarted,
                json!({
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
                json!({
                    "usage": {"input_tokens": 400, "output_tokens": 20, "total_tokens": 420}
                }),
            ),
        ];
        let diagnostics = ContextDiagnostics::rebuild(records.iter());

        assert_eq!(
            diagnostics.continuation,
            Some(ContinuationMode::PreviousResponse)
        );
        assert_eq!(diagnostics.prompt_cache, Some(true));
        assert_eq!(diagnostics.usage.unwrap().total, 420);
        assert_eq!(diagnostics.compactions_started, 1);
        assert_eq!(diagnostics.compactions_completed, 1);
        assert_eq!(diagnostics.model_window_tokens, 272_000);
        assert_eq!(diagnostics.auto_compact_token_limit, 200_000);
        assert_eq!(
            diagnostics.last_compaction.unwrap().before_tokens,
            Some(900)
        );
        assert_eq!(diagnostics.last_compaction.unwrap().after_tokens, Some(400));
        let debug = format!("{diagnostics:?}");
        for secret in [
            "secret-cache-key",
            "secret prompt",
            "secret-response-id",
            "secret-continuation-token",
        ] {
            assert!(!debug.contains(secret));
        }
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
        assert_eq!(completed_transcript_tokens(&record), Some(136_000));
    }
}
