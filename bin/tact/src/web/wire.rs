//! JSON shapes of the web protocol (see `docs/web.md`) and the projection of
//! transcript entries onto them.

use crate::{
    app::config::{ReasoningEffort, ReasoningMode, Speed},
    core::subagent_roster::SubagentRoster,
    tui::{
        context::ContextBudget,
        transcript::{
            DirectedMessageEntry, EntryKind, ToolEntry, ToolState, TranscriptEntry, TransientStatus,
        },
    },
    web::bridge::{DraftImage, Origin, QueuedPrompt},
};
use serde::Serialize;
use serde_json::Value;
use std::sync::Arc;
use tact_subagents::{MessageDeliveryState, MessageSender};

/// Version of the browser protocol. Bundles declare the range they speak in their manifest.
pub(super) const PROTOCOL_VERSION: u32 = 9;
/// Longest string sent to a browser in an entry or a tool detail.
const MAX_STRING_BYTES: usize = 256 * 1024;
const MAX_SUMMARY_CHARS: usize = 200;

/// One complete Server-Sent Events message, serialized once and shared by every client.
pub(super) type Frame = Arc<str>;

pub(super) fn frame(event: &str, data: &impl Serialize) -> Frame {
    let data = serde_json::to_string(data).expect("wire types serialize to JSON");
    format!("event: {event}\ndata: {data}\n\n").into()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum SummaryState {
    Idle,
    Running,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct SessionSummary {
    pub(super) id: String,
    pub(super) title: String,
    pub(super) model: String,
    pub(super) state: SummaryState,
    pub(super) unread: bool,
    pub(super) has_draft: bool,
    pub(super) last_activity_unix_ms: u64,
}

#[derive(Serialize)]
pub(super) struct SessionSnapshot {
    pub(super) session: String,
    pub(super) title: String,
    pub(super) model: String,
    pub(super) effort: ReasoningEffort,
    pub(super) reasoning_mode: ReasoningMode,
    pub(super) speed: Speed,
    pub(super) entries: Vec<WireEntry>,
    pub(super) status: Option<WireStatus>,
    pub(super) queue: Vec<WireQueued>,
    pub(super) draft: WireDraft,
    pub(super) running: bool,
    pub(super) context: Option<ContextBudget>,
    pub(super) subagents: SubagentRoster,
}

#[derive(Serialize)]
pub(super) struct WireDraft {
    pub(super) rev: u64,
    pub(super) text: String,
    pub(super) images: Vec<WireImage>,
}

#[derive(Serialize)]
pub(super) struct WireImage {
    marker: String,
    data_url: String,
}

impl From<&DraftImage> for WireImage {
    fn from(image: &DraftImage) -> Self {
        Self {
            marker: image.marker.clone(),
            data_url: image.data_url.clone(),
        }
    }
}

#[derive(Serialize)]
pub(super) struct WireQueued {
    id: u64,
    text: String,
    steering: bool,
}

impl From<&QueuedPrompt> for WireQueued {
    fn from(prompt: &QueuedPrompt) -> Self {
        Self {
            id: prompt.id,
            text: cap(&prompt.text),
            steering: prompt.steering,
        }
    }
}

pub(super) fn origin_label(origin: Origin) -> String {
    match origin {
        Origin::Terminal => "terminal".to_owned(),
        Origin::Web(client) => format!("web:{client}"),
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum WireStatus {
    Thinking,
    Responding,
    Warming,
    WaitingForBackgroundWork,
    Compacting,
    Connecting,
    Reconnecting,
    Tool {
        name: String,
    },
    Retrying {
        delay_ns: u64,
        next_attempt: u32,
        max_attempts: u32,
    },
    Error {
        message: String,
    },
}

impl From<&TransientStatus> for WireStatus {
    fn from(status: &TransientStatus) -> Self {
        match status {
            TransientStatus::Thinking => Self::Thinking,
            TransientStatus::Responding => Self::Responding,
            TransientStatus::Warming => Self::Warming,
            TransientStatus::WaitingForBackgroundWork => Self::WaitingForBackgroundWork,
            TransientStatus::Compacting => Self::Compacting,
            TransientStatus::Connecting => Self::Connecting,
            TransientStatus::Reconnecting => Self::Reconnecting,
            TransientStatus::Tool(name) => Self::Tool { name: cap(name) },
            TransientStatus::Retrying {
                delay_ns,
                next_attempt,
                max_attempts,
            } => Self::Retrying {
                delay_ns: *delay_ns,
                next_attempt: *next_attempt,
                max_attempts: *max_attempts,
            },
            TransientStatus::Error(message) => Self::Error {
                message: cap(message),
            },
        }
    }
}

/// An entry with a stable `id` and a `revision` that grows on every change.
#[derive(Serialize)]
pub(super) struct WireEntry {
    pub(super) id: usize,
    pub(super) revision: u64,
    parent: Option<usize>,
    #[serde(flatten)]
    body: WireBody,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum WireToolState {
    Running,
    Succeeded,
    Failed,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum WireBody {
    User {
        text: String,
        /** Attachments: the i-th replaces the i-th "[Image #N]" marker; bytes are served separately. */
        images: usize,
    },
    Assistant {
        text: String,
        complete: bool,
        /// The transcript model does not record the message phase, so this is always false.
        commentary: bool,
    },
    Reasoning {
        text: String,
    },
    Tool {
        name: String,
        summary: String,
        state: WireToolState,
        duration_ns: Option<u64>,
        substeps: Vec<String>,
        child_count: usize,
        has_detail: bool,
    },
    DirectedMessage {
        from: String,
        to: String,
        body: String,
        delivery: String,
    },
    ForkedFrom {
        session: String,
    },
    EffortChanged {
        to: ReasoningEffort,
    },
    FastModeChanged {
        enabled: bool,
    },
    ReflectionStarted,
    Interrupted {
        count: usize,
    },
    ContextCompacted {
        duration_ns: u64,
    },
    TurnCompleted {
        duration_ns: u64,
    },
    CompactionFailed {
        message: String,
    },
    Error {
        message: String,
    },
}

impl WireEntry {
    pub(super) fn new(entry: &TranscriptEntry) -> Self {
        let body = match &entry.kind {
            EntryKind::User { text, images } => WireBody::User {
                text: cap(text),
                images: images.len(),
            },
            EntryKind::Assistant { text, complete } => WireBody::Assistant {
                text: cap(text),
                complete: *complete,
                commentary: false,
            },
            EntryKind::Reasoning { text } => WireBody::Reasoning { text: cap(text) },
            EntryKind::Tool(tool) => WireBody::tool(tool),
            EntryKind::DirectedMessage(message) => WireBody::directed(message),
            EntryKind::ForkedFrom { session_id } => WireBody::ForkedFrom {
                session: session_id.clone(),
            },
            EntryKind::EffortChanged { to } => WireBody::EffortChanged { to: *to },
            EntryKind::SpeedChanged { speed } => WireBody::FastModeChanged {
                enabled: *speed == Speed::Fast,
            },
            EntryKind::ReflectionStarted => WireBody::ReflectionStarted,
            EntryKind::Interrupted { count } => WireBody::Interrupted { count: *count },
            EntryKind::ContextCompacted { duration_ns } => WireBody::ContextCompacted {
                duration_ns: *duration_ns,
            },
            EntryKind::TurnCompleted { duration_ns } => WireBody::TurnCompleted {
                duration_ns: *duration_ns,
            },
            EntryKind::ContextCompactionFailed { message } => WireBody::CompactionFailed {
                message: cap(message),
            },
            EntryKind::Error { message } => WireBody::Error {
                message: cap(message),
            },
        };
        Self {
            id: entry.id.index(),
            revision: entry.revision,
            parent: entry.parent.map(|parent| parent.index()),
            body,
        }
    }
}

impl WireBody {
    fn tool(tool: &ToolEntry) -> Self {
        Self::Tool {
            name: tool.name.clone(),
            summary: tool_summary(tool),
            state: match tool.state {
                ToolState::Running => WireToolState::Running,
                ToolState::Succeeded => WireToolState::Succeeded,
                ToolState::Failed => WireToolState::Failed,
            },
            duration_ns: tool.duration_ns,
            substeps: tool.substeps.iter().map(|step| cap(step)).collect(),
            child_count: tool.child_count,
            has_detail: true,
        }
    }

    fn directed(message: &DirectedMessageEntry) -> Self {
        let label = |sender: &MessageSender| match sender {
            MessageSender::Root => "root".to_owned(),
            MessageSender::Agent { agent_id } => format!("agent {agent_id}"),
        };
        let latest = message.thread.messages.last();
        let delivery = latest
            .and_then(|latest| message.delivery(latest.id))
            .map_or("unknown", |state| match state {
                MessageDeliveryState::Admitted { .. } => "admitted",
                MessageDeliveryState::Delivered { .. } => "delivered",
                MessageDeliveryState::Failed { .. } => "failed",
            });
        let [first, second] = &message.thread.participants;
        let (from, to) = if *first == message.perspective {
            (label(first), label(second))
        } else {
            (label(second), label(first))
        };
        Self::DirectedMessage {
            from: latest.map_or(from, |latest| label(&latest.from)),
            to,
            body: latest.map_or_else(String::new, |latest| cap(&latest.body)),
            delivery: delivery.to_owned(),
        }
    }
}

/// The arguments, result, and metadata of one tool call, with every string capped.
#[derive(Serialize)]
pub(super) struct ToolDetail {
    arguments: Value,
    result: Option<Value>,
    metadata: Option<Value>,
}

impl ToolDetail {
    pub(super) fn new(tool: &ToolEntry) -> Self {
        Self {
            arguments: capped_value(&tool.arguments),
            result: tool.result.as_ref().map(capped_value),
            metadata: tool.metadata.as_ref().map(capped_value),
        }
    }
}

fn capped_value(value: &Value) -> Value {
    match value {
        Value::String(text) => Value::String(cap(text)),
        Value::Array(items) => Value::Array(items.iter().map(capped_value).collect()),
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, value)| (key.clone(), capped_value(value)))
                .collect(),
        ),
        scalar => scalar.clone(),
    }
}

/// Truncates at a character boundary so no single string exceeds [`MAX_STRING_BYTES`].
pub(super) fn cap(text: &str) -> String {
    if text.len() <= MAX_STRING_BYTES {
        return text.to_owned();
    }
    let mut end = MAX_STRING_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… [truncated]", &text[..end])
}

/// A single line naming what a tool call operates on, mirroring the terminal's tool rows.
fn tool_summary(tool: &ToolEntry) -> String {
    let arguments = &tool.arguments;
    let text = |key: &str| arguments.get(key).and_then(Value::as_str);
    let subject = match tool.name.as_str() {
        "exec_command" => text("cmd").map(str::to_owned),
        "write_stdin" => Some(match text("chars").filter(|chars| !chars.is_empty()) {
            Some(chars) => format!("stdin: {chars}"),
            None => "poll output".to_owned(),
        }),
        "apply_patch" => Some(patch_files(arguments)).filter(|files| !files.is_empty()),
        "web__run" => ["search_query", "open", "find", "click", "image_query"]
            .into_iter()
            .find_map(|operation| {
                let first = arguments.get(operation)?.get(0)?;
                ["q", "ref_id", "pattern"]
                    .into_iter()
                    .find_map(|key| first.get(key)?.as_str())
                    .map(str::to_owned)
            }),
        "send_agent_message" => arguments
            .get("agent_id")
            .and_then(Value::as_u64)
            .map(|id| format!("→ #{id}")),
        "update_plan" => arguments
            .get("plan")
            .and_then(Value::as_array)
            .map(|steps| format!("{} steps", steps.len())),
        "exec" => Some("code".to_owned()),
        "wait" => Some("background work".to_owned()),
        "memory" => memory_summary(tool),
        _ => None,
    }
    .or_else(|| {
        ["path", "query", "prompt", "url", "name"]
            .into_iter()
            .find_map(|key| text(key).map(str::to_owned))
    })
    .unwrap_or_else(|| {
        let count = arguments.as_object().map_or(0, serde_json::Map::len);
        format!("{count} arguments")
    });
    first_line(&subject)
}

/// Names a memory call's operation, backend, subject and outcome, mirroring the terminal's memory
/// rows: `scan · local · rust style · 3 candidates`. Parts the call does not have yet (the
/// backend and outcome before it finishes) are left out.
fn memory_summary(tool: &ToolEntry) -> Option<String> {
    let arguments = &tool.arguments;
    let result = tool.result.as_ref();
    let operation = arguments.get("operation")?.as_str()?;
    let count = |key: &str, singular: &str, plural: &str| {
        let total = result?.get(key)?.as_array()?.len();
        Some(format!(
            "{total} {}",
            if total == 1 { singular } else { plural }
        ))
    };
    let keys = |value: Option<&Value>| {
        value.and_then(Value::as_array).map(|keys| {
            keys.iter()
                .filter_map(memory_key)
                .collect::<Vec<_>>()
                .join(", ")
        })
    };
    let (name, subject, outcome) = match operation {
        "scan" => (
            "scan",
            arguments.get("query")?.as_str().map(str::to_owned),
            if result.and_then(|result| result.get("abstained")?.as_bool()) == Some(true) {
                Some("abstained".to_owned())
            } else {
                count("candidates", "candidate", "candidates")
            },
        ),
        "read" => (
            "read",
            keys(arguments.get("keys").or_else(|| arguments.get("ids"))),
            count("memories", "memory", "memories"),
        ),
        "put" => {
            let replaced = arguments.get("replace").and_then(memory_key);
            let stored = result
                .and_then(|result| result.get("memory"))
                .and_then(memory_key);
            (
                if replaced.is_some() {
                    "replace"
                } else {
                    "store"
                },
                stored.or(replaced),
                None,
            )
        }
        "delete" => ("delete", memory_key(arguments), None),
        _ => return None,
    };
    let backend = result
        .and_then(|result| result.get("backend")?.get("source")?.as_str())
        .map(str::to_owned);
    let parts: Vec<String> = [Some(name.to_owned()), backend, subject, outcome]
        .into_iter()
        .flatten()
        .filter(|part| !part.is_empty())
        .collect();
    Some(parts.join(" · "))
}

/// Formats a memory key (or a record holding one) as `namespace:id@vN`.
fn memory_key(value: &Value) -> Option<String> {
    let key = value.get("key").unwrap_or(value);
    let id = match key.get("id")? {
        Value::Number(number) => number.to_string(),
        Value::String(text) if !text.is_empty() => text.clone(),
        _ => return None,
    };
    let namespace = key
        .get("namespace")
        .and_then(Value::as_str)
        .map(|namespace| format!("{namespace}:"))
        .unwrap_or_default();
    let version = key
        .get("version")
        .and_then(Value::as_u64)
        .map(|version| format!("@v{version}"))
        .unwrap_or_default();
    Some(format!("{namespace}{id}{version}"))
}

/// Lists the files an `apply_patch` envelope touches.
fn patch_files(arguments: &Value) -> String {
    let patch = arguments
        .as_str()
        .or_else(|| arguments.get("input").and_then(Value::as_str))
        .unwrap_or_default();
    patch
        .lines()
        .filter_map(|line| {
            ["*** Add File: ", "*** Update File: ", "*** Delete File: "]
                .into_iter()
                .find_map(|prefix| line.strip_prefix(prefix))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default().trim();
    let mut chars = line.chars();
    let head = chars.by_ref().take(MAX_SUMMARY_CHARS).collect::<String>();
    if chars.next().is_some() || text.trim().lines().nth(1).is_some() {
        return format!("{head} …");
    }
    head
}

#[cfg(test)]
mod tests {
    use super::{MAX_STRING_BYTES, cap, tool_summary};
    use crate::tui::transcript::{ToolEntry, ToolState};
    use serde_json::{Value, json};

    fn tool(name: &str, arguments: Value) -> ToolEntry {
        ToolEntry {
            name: name.to_owned(),
            arguments,
            started_at_unix_ms: 0,
            state: ToolState::Succeeded,
            duration_ns: None,
            result: None,
            metadata: None,
            substeps: Vec::new(),
            child_count: 0,
        }
    }

    #[test]
    fn tool_summaries_name_the_subject_in_one_line() {
        let cases = [
            (
                "exec_command",
                json!({"cmd": "cargo test\nsecond"}),
                "cargo test …",
            ),
            (
                "apply_patch",
                json!(
                    "*** Begin Patch\n*** Update File: src/a.rs\n*** Add File: b.rs\n*** End Patch"
                ),
                "src/a.rs, b.rs",
            ),
            (
                "web__run",
                json!({"search_query": [{"q": "rust sse"}]}),
                "rust sse",
            ),
            ("view_image", json!({"path": "/tmp/a.png"}), "/tmp/a.png"),
            ("mystery", json!({"a": 1, "b": 2}), "2 arguments"),
        ];
        for (name, arguments, expected) in cases {
            assert_eq!(tool_summary(&tool(name, arguments)), expected, "{name}");
        }
    }

    #[test]
    fn memory_summaries_name_the_operation_backend_subject_and_outcome() {
        let finished = |arguments: Value, result: Value| ToolEntry {
            result: Some(result),
            ..tool("memory", arguments)
        };
        let cases = [
            (
                tool(
                    "memory",
                    json!({"operation": "scan", "query": "rust style"}),
                ),
                "scan · rust style",
            ),
            (
                finished(
                    json!({"operation": "scan", "query": "rust style"}),
                    json!({"backend": {"source": "local"}, "abstained": false, "candidates": [{}, {}]}),
                ),
                "scan · local · rust style · 2 candidates",
            ),
            (
                finished(
                    json!({"operation": "scan", "query": "x"}),
                    json!({"abstained": true, "candidates": []}),
                ),
                "scan · x · abstained",
            ),
            (
                finished(
                    json!({"operation": "read", "keys": [{"id": 7, "version": 2}, {"namespace": "alice", "id": 8, "version": 1}]}),
                    json!({"backend": {"source": "remote"}, "memories": [{}, {}]}),
                ),
                "read · remote · 7@v2, alice:8@v1 · 2 memories",
            ),
            (
                finished(
                    json!({"operation": "put", "content": "c", "replace": {"id": 7, "version": 1}}),
                    json!({"memory": {"key": {"id": 7, "version": 2}}, "replaced": true}),
                ),
                "replace · 7@v2",
            ),
            (
                tool("memory", json!({"operation": "put", "content": "c"})),
                "store",
            ),
            (
                tool(
                    "memory",
                    json!({"operation": "delete", "key": {"id": 7, "version": 2}}),
                ),
                "delete · 7@v2",
            ),
        ];
        for (entry, expected) in cases {
            assert_eq!(tool_summary(&entry), expected);
        }
    }

    #[test]
    fn oversized_strings_are_truncated_on_a_character_boundary() {
        let text = "é".repeat(MAX_STRING_BYTES);
        let capped = cap(&text);
        assert!(capped.len() < MAX_STRING_BYTES + 32);
        assert!(capped.ends_with("[truncated]"));
        assert_eq!(cap("short"), "short");
    }
}
