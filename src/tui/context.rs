//! Context-window usage extracted from raw Responses events.

use crate::tui::transcript::TranscriptRecord;
use serde::Deserialize;

#[derive(Deserialize)]
struct ApiEvent {
    direction: String,
    phase: String,
    event: ResponseEvent,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum ResponseEvent {
    #[serde(rename = "response.completed")]
    Completed { response: CompletedResponse },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct CompletedResponse {
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Usage {
    total_tokens: u64,
}

pub(crate) fn completed_transcript_tokens(record: &TranscriptRecord) -> Option<u64> {
    if record.source() != "agent" || record.kind() != "api.event" {
        return None;
    }

    completed_tokens(record.decode_payload::<ApiEvent>().ok()?)
}

fn completed_tokens(event: ApiEvent) -> Option<u64> {
    if event.direction != "inbound" || event.phase != "generation" {
        return None;
    }

    match event.event {
        ResponseEvent::Completed { response } => response.usage.map(|usage| usage.total_tokens),
        ResponseEvent::Other => None,
    }
}

#[cfg(test)]
mod tests {
    use super::completed_transcript_tokens;
    use crate::tui::transcript::TranscriptRecord;
    use nanocodex::{AgentEvent, AgentEventKind};
    use serde_json::value::to_raw_value;
    use std::sync::Arc;

    fn record(kind: AgentEventKind, payload: serde_json::Value) -> TranscriptRecord {
        TranscriptRecord::from_agent(
            1,
            1,
            AgentEvent {
                protocol_version: 1,
                request_id: Arc::from("test"),
                seq: 1,
                kind,
                payload: to_raw_value(&payload).unwrap(),
            },
        )
    }

    #[test]
    fn only_inbound_generation_completions_update_context() {
        let completed = serde_json::json!({
            "direction": "inbound",
            "phase": "generation",
            "event": {
                "type": "response.completed",
                "response": { "usage": { "total_tokens": 136_000 } }
            }
        });
        assert_eq!(
            completed_transcript_tokens(&record(AgentEventKind::ApiEvent, completed.clone())),
            Some(136_000)
        );

        for (direction, phase) in [
            ("outbound", "generation"),
            ("inbound", "warmup"),
            ("outbound", "warmup"),
        ] {
            let mut payload = completed.clone();
            payload["direction"] = direction.into();
            payload["phase"] = phase.into();
            assert_eq!(
                completed_transcript_tokens(&record(AgentEventKind::ApiEvent, payload)),
                None
            );
        }

        assert_eq!(
            completed_transcript_tokens(&record(AgentEventKind::AssistantMessage, completed)),
            None
        );
    }

    #[test]
    fn completion_without_usage_is_ignored() {
        let payload = serde_json::json!({
            "direction": "inbound",
            "phase": "generation",
            "event": {
                "type": "response.completed",
                "response": { "usage": null }
            }
        });

        assert_eq!(
            completed_transcript_tokens(&record(AgentEventKind::ApiEvent, payload)),
            None
        );
    }
}
