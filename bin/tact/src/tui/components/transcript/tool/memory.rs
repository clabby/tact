//! Presenter for the `memory` tool's scan, read, put, and delete operations.
//!
//! The presenter parses the call's arguments and result into typed operations and outcomes, and
//! falls back to the generic tool view when either has an unrecognized shape. Results that report
//! their backend title the row with the operation, backend, and key or query; results without a
//! backend and calls still in flight use the plain operation title with the subject beside it.
//!
//! Memory content never appears in the collapsed summary. Expanded scans show at most the
//! production scan limit of candidates, each limited to its key, score, and clipped preview.
//! Expanded reads and stores show each record's key, content, and a fixed set of metadata fields;
//! an overwrite that reports its previous content shows a line diff instead. Expanded details of
//! completed results are selectable text.

use super::{Presentation, generic};
use crate::{app::theme::Theme, core::transcript::ToolEntry, tui::components::fit::clip_to_width};
use ratatui::{
    style::{Color, Style},
    text::Line,
};
use serde_json::{Map, Value};
use tact_memory::MemoryLimits;

const MAX_SCAN_CANDIDATES: usize = MemoryLimits::PRODUCTION.scan_results;
const MAX_PREVIEW_WIDTH: u16 = 240;

pub(super) fn present(tool: &ToolEntry, width: u16, theme: &Theme, expanded: bool) -> Presentation {
    let Some(operation) = Operation::parse(&tool.arguments) else {
        return generic(tool, width, theme, expanded);
    };
    let Some(result) = ResultValue::parse(tool, operation.name()) else {
        return generic(tool, width, theme, expanded);
    };

    match operation {
        Operation::Scan { query } => scan(query, result, width, theme, expanded),
        Operation::Read { keys } => read(keys, result, width, theme, expanded),
        Operation::Put { content, replace } => {
            put(content, replace, result, width, theme, expanded)
        }
        Operation::Delete { key } => delete(key, result, width, theme, expanded),
    }
}

enum Operation<'a> {
    Scan {
        query: &'a str,
    },
    Read {
        keys: Vec<MemoryKey>,
    },
    Put {
        content: &'a str,
        replace: Option<MemoryKey>,
    },
    Delete {
        key: MemoryKey,
    },
}

impl<'a> Operation<'a> {
    fn parse(arguments: &'a Value) -> Option<Self> {
        let operation = arguments.get("operation")?.as_str()?;
        match operation {
            "scan" => Some(Self::Scan {
                query: arguments.get("query")?.as_str()?,
            }),
            "read" => Some(Self::Read {
                keys: parse_keys(arguments.get("keys").or_else(|| arguments.get("ids"))?)?,
            }),
            "put" => Some(Self::Put {
                content: arguments.get("content")?.as_str()?,
                replace: match arguments.get("replace") {
                    Some(replace) => Some(MemoryKey::parse_versioned(replace)?),
                    None => None,
                },
            }),
            "delete" => Some(Self::Delete {
                key: MemoryKey::parse_versioned(arguments)?,
            }),
            _ => None,
        }
    }

    const fn name(&self) -> &'static str {
        match self {
            Self::Scan { .. } => "scan",
            Self::Read { .. } => "read",
            Self::Put { .. } => "put",
            Self::Delete { .. } => "delete",
        }
    }
}

enum ResultValue<'a> {
    Pending,
    Failed,
    Scan {
        backend: Option<Backend>,
        abstained: bool,
        candidates: Vec<Candidate<'a>>,
    },
    Read {
        backend: Option<Backend>,
        memories: Vec<Memory<'a>>,
    },
    Put {
        backend: Option<Backend>,
        memory: Memory<'a>,
        replaced: bool,
        previous_content: Option<&'a str>,
    },
    Delete {
        backend: Option<Backend>,
        key: MemoryKey,
    },
}

impl<'a> ResultValue<'a> {
    fn parse(tool: &'a ToolEntry, expected_operation: &str) -> Option<Self> {
        let Some(result) = tool.result.as_ref() else {
            return Some(Self::Pending);
        };
        if tool.state == crate::core::transcript::ToolState::Failed
            && result.get("error").and_then(Value::as_str).is_some()
        {
            return Some(Self::Failed);
        }
        if result.get("operation").and_then(Value::as_str) != Some(expected_operation) {
            return None;
        }

        match expected_operation {
            "scan" => Some(Self::Scan {
                backend: Backend::parse(result.get("backend")),
                abstained: result.get("abstained")?.as_bool()?,
                candidates: result
                    .get("candidates")?
                    .as_array()?
                    .iter()
                    .map(Candidate::parse)
                    .collect::<Option<Vec<_>>>()?,
            }),
            "read" => Some(Self::Read {
                backend: Backend::parse(result.get("backend")),
                memories: result
                    .get("memories")?
                    .as_array()?
                    .iter()
                    .map(Memory::parse)
                    .collect::<Option<Vec<_>>>()?,
            }),
            "put" => Some(Self::Put {
                backend: Backend::parse(result.get("backend")),
                memory: Memory::parse(result.get("memory")?)?,
                replaced: was_replaced(result.get("replaced")?),
                previous_content: result.get("previous_content").and_then(Value::as_str),
            }),
            "delete" => Some(Self::Delete {
                backend: Backend::parse(result.get("backend")),
                key: MemoryKey::parse(result)?,
            }),
            _ => None,
        }
    }
}

#[derive(Clone)]
enum Backend {
    Local,
    Remote,
}

impl Backend {
    fn parse(value: Option<&Value>) -> Option<Self> {
        match value?.get("source")?.as_str()? {
            "local" => Some(Self::Local),
            "remote" => Some(Self::Remote),
            _ => None,
        }
    }

    const fn label(&self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Remote => "remote",
        }
    }
}

fn completed_presentation(
    backend: Option<&Backend>,
    operation: &'static str,
    legacy: &'static str,
    subject: String,
) -> Presentation {
    let Some(backend) = backend else {
        return Presentation::new(legacy, subject);
    };
    let title = if subject.is_empty() {
        format!("Memory {operation} · {}", backend.label())
    } else {
        format!("Memory {operation} · {} · {subject}", backend.label())
    };
    Presentation::new(title, "")
}

#[derive(Clone)]
struct MemoryKey {
    id: String,
    version: Option<u64>,
    namespace: Option<String>,
}

impl MemoryKey {
    fn parse(value: &Value) -> Option<Self> {
        if !value.is_object() {
            return Some(Self {
                id: parse_id(value)?,
                version: None,
                namespace: None,
            });
        }
        let value = value
            .get("key")
            .and_then(Value::as_object)
            .or_else(|| value.as_object())?;
        Some(Self {
            id: parse_id(value.get("id")?)?,
            version: match value.get("version") {
                Some(version) => Some(version.as_u64()?),
                None => None,
            },
            namespace: match value.get("namespace") {
                Some(namespace) => Some(namespace.as_str()?.to_owned()),
                None => None,
            },
        })
    }

    fn parse_versioned(value: &Value) -> Option<Self> {
        let key = Self::parse(value)?;
        key.version?;
        Some(key)
    }

    fn display(&self) -> String {
        let id = self.namespace.as_ref().map_or_else(
            || self.id.clone(),
            |namespace| format!("{namespace}:{}", self.id),
        );
        self.version
            .as_ref()
            .map_or(id.clone(), |version| format!("{id}@v{version}"))
    }
}

struct Candidate<'a> {
    key: MemoryKey,
    preview: &'a str,
    score: f64,
}

impl<'a> Candidate<'a> {
    fn parse(value: &'a Value) -> Option<Self> {
        let score = value.get("score")?.as_f64()?;
        if !score.is_finite() {
            return None;
        }
        Some(Self {
            key: MemoryKey::parse(value)?,
            preview: value.get("preview")?.as_str()?,
            score,
        })
    }
}

struct Memory<'a> {
    key: MemoryKey,
    content: &'a str,
    fields: &'a Map<String, Value>,
}

impl<'a> Memory<'a> {
    fn parse(value: &'a Value) -> Option<Self> {
        Some(Self {
            key: MemoryKey::parse(value)?,
            content: value.get("content")?.as_str()?,
            fields: value.as_object()?,
        })
    }
}

fn scan(
    query: &str,
    result: ResultValue<'_>,
    width: u16,
    theme: &Theme,
    expanded: bool,
) -> Presentation {
    let ResultValue::Scan {
        backend,
        abstained,
        candidates,
    } = result
    else {
        return Presentation::new("Memory scan", query);
    };
    let outcome = if abstained {
        "abstained".to_owned()
    } else {
        count_label(candidates.len(), "candidate", "candidates")
    };
    let presentation =
        completed_presentation(backend.as_ref(), "scan", "Memory scan", query.to_owned())
            .outcome(outcome);
    if !expanded {
        return presentation;
    }

    let shown = candidates.len().min(MAX_SCAN_CANDIDATES);
    let mut presentation = presentation;
    for candidate in candidates.iter().take(shown) {
        let label = format!(
            "{} · score {}",
            candidate.key.display(),
            format_score(candidate.score)
        );
        presentation =
            presentation.selectable_plain(&label, width, Style::default().fg(theme.accent()));
        let preview = clip_to_width(candidate.preview, usize::from(MAX_PREVIEW_WIDTH));
        presentation =
            presentation.selectable_plain(preview, width, Style::default().fg(theme.text()));
    }
    let footer = if abstained {
        "memory scan abstained".to_owned()
    } else if shown < candidates.len() {
        format!("{shown} of {} candidates", candidates.len())
    } else {
        count_label(candidates.len(), "candidate", "candidates")
    };
    presentation.footer(footer)
}

fn read(
    keys: Vec<MemoryKey>,
    result: ResultValue<'_>,
    width: u16,
    theme: &Theme,
    expanded: bool,
) -> Presentation {
    let subject = keys
        .iter()
        .map(MemoryKey::display)
        .collect::<Vec<_>>()
        .join(", ");
    let ResultValue::Read { backend, memories } = result else {
        return Presentation::new("Memory read", subject);
    };
    let count = count_label(memories.len(), "memory", "memories");
    let presentation =
        completed_presentation(backend.as_ref(), "read", "Memory read", subject).outcome(&count);
    if !expanded {
        return presentation;
    }

    let mut presentation = presentation;
    for memory in &memories {
        presentation = selectable_memory_details(presentation, memory, width, theme);
    }
    presentation.footer(count)
}

fn put(
    content: &str,
    replace: Option<MemoryKey>,
    result: ResultValue<'_>,
    width: u16,
    theme: &Theme,
    expanded: bool,
) -> Presentation {
    let ResultValue::Put {
        backend,
        memory,
        replaced,
        previous_content,
    } = result
    else {
        let title = if replace.is_some() {
            "Memory replace"
        } else {
            "Memory store"
        };
        let subject = replace
            .as_ref()
            .map_or_else(String::new, MemoryKey::display);
        let presentation = Presentation::new(title, subject);
        if !expanded {
            return presentation;
        }
        return presentation
            .unselectable_details(wrap(content, width, Style::default().fg(theme.text())))
            .footer("memory content");
    };

    let legacy = if replaced {
        "Memory replaced"
    } else {
        "Memory stored"
    };
    let operation = if replaced { "replace" } else { "store" };
    let presentation =
        completed_presentation(backend.as_ref(), operation, legacy, memory.key.display());
    if !expanded {
        return presentation;
    }
    if let Some(previous_content) = previous_content.filter(|_| replaced) {
        let mut presentation = presentation.selectable_plain(
            memory.key.display(),
            width,
            Style::default().fg(theme.accent()),
        );
        let before: Vec<_> = previous_content.split('\n').collect();
        let after: Vec<_> = memory.content.split('\n').collect();
        let prefix = before
            .iter()
            .zip(&after)
            .take_while(|(a, b)| a == b)
            .count();
        let suffix = before[prefix..]
            .iter()
            .rev()
            .zip(after[prefix..].iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
        for line in &before[..prefix] {
            presentation = presentation.selectable_plain(
                format!("  {line}"),
                width,
                Style::default().fg(theme.text()),
            );
        }
        for line in &before[prefix..before.len() - suffix] {
            presentation = presentation.selectable_plain(
                format!("- {line}"),
                width,
                Style::default().fg(Color::Red),
            );
        }
        for line in &after[prefix..after.len() - suffix] {
            presentation = presentation.selectable_plain(
                format!("+ {line}"),
                width,
                Style::default().fg(Color::Green),
            );
        }
        for line in &before[before.len() - suffix..] {
            presentation = presentation.selectable_plain(
                format!("  {line}"),
                width,
                Style::default().fg(theme.text()),
            );
        }
        return presentation;
    }
    selectable_memory_details(presentation, &memory, width, theme).footer("memory record")
}

fn delete(
    key: MemoryKey,
    result: ResultValue<'_>,
    width: u16,
    theme: &Theme,
    expanded: bool,
) -> Presentation {
    let (backend, key, legacy) = match result {
        ResultValue::Delete { backend, key } => (backend, key, "Memory deleted"),
        _ => (None, key, "Memory delete"),
    };
    let presentation = completed_presentation(backend.as_ref(), "delete", legacy, key.display());
    if !expanded {
        return presentation;
    }
    let key = key.display();
    presentation
        .selectable_plain(&key, width, Style::default().fg(theme.accent()))
        .footer("memory key")
}

fn selectable_memory_details(
    presentation: Presentation,
    memory: &Memory<'_>,
    width: u16,
    theme: &Theme,
) -> Presentation {
    let key = memory.key.display();
    let mut presentation =
        presentation.selectable_plain(&key, width, Style::default().fg(theme.accent()));
    presentation =
        presentation.selectable_plain(memory.content, width, Style::default().fg(theme.text()));
    if let Some(metadata) = selected_metadata(memory.fields) {
        presentation =
            presentation.selectable_plain(&metadata, width, Style::default().fg(theme.muted()));
    }
    presentation
}

fn selected_metadata(fields: &Map<String, Value>) -> Option<String> {
    let metadata = [
        ("created_at_ms", "created"),
        ("updated_at_ms", "updated"),
        ("last_scanned_at_ms", "last scanned"),
        ("scan_count", "scans"),
        ("last_used_at_ms", "last used"),
        ("use_count", "uses"),
        ("probation_until_ms", "probation until"),
    ]
    .into_iter()
    .filter_map(|(field, label)| scalar(fields.get(field)?).map(|value| format!("{label} {value}")))
    .collect::<Vec<_>>();
    (!metadata.is_empty()).then(|| metadata.join(" · "))
}

fn scalar(value: &Value) -> Option<String> {
    match value {
        Value::Number(number) => Some(number.to_string()),
        Value::String(text) => Some(text.clone()),
        Value::Bool(value) => Some(value.to_string()),
        Value::Null | Value::Array(_) | Value::Object(_) => None,
    }
}

fn parse_keys(value: &Value) -> Option<Vec<MemoryKey>> {
    let keys = value
        .as_array()?
        .iter()
        .map(MemoryKey::parse)
        .collect::<Option<Vec<_>>>()?;
    (!keys.is_empty()).then_some(keys)
}

fn parse_id(value: &Value) -> Option<String> {
    if let Some(id) = value.as_i64() {
        return Some(id.to_string());
    }
    if let Some(id) = value.as_u64() {
        return Some(id.to_string());
    }
    value
        .as_str()
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

fn was_replaced(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => false,
        Value::Bool(true)
        | Value::Number(_)
        | Value::String(_)
        | Value::Array(_)
        | Value::Object(_) => true,
    }
}

fn wrap(text: &str, width: u16, style: Style) -> Vec<Line<'static>> {
    super::super::markdown::wrap_plain(text, width, style)
}

fn count_label(count: usize, singular: &str, plural: &str) -> String {
    let label = if count == 1 { singular } else { plural };
    format!("{count} {label}")
}

fn format_score(score: f64) -> String {
    format!("{score:.3}")
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::super::{
        fixtures::{assert_summary, collapsed, entry, expanded, rendered},
        render, render_expanded,
    };
    use crate::{
        app::theme::Theme,
        core::transcript::{ToolEntry, ToolState},
    };
    use ratatui::style::Color;
    use serde_json::{Value, json};

    fn memory(arguments: Value, state: ToolState, result: Option<Value>) -> ToolEntry {
        ToolEntry {
            state,
            result,
            ..entry("memory", arguments)
        }
    }

    fn record(id: i64, version: u64, content: &str) -> Value {
        json!({
            "key": {"id": id, "version": version},
            "content": content,
            "created_at_ms": 10,
            "updated_at_ms": 20,
            "last_scanned_at_ms": null,
            "scan_count": 2,
            "last_used_at_ms": 30,
            "use_count": 1,
            "probation_until_ms": null
        })
    }

    const RECORD_METADATA: &str = "created 10 · updated 20 · scans 2 · last used 30 · uses 1";

    #[test]
    fn summaries_cover_operations_states_and_results() {
        let cases = [
            (
                memory(
                    json!({"operation": "scan", "query": "Rust style"}),
                    ToolState::Running,
                    None,
                ),
                ("Memory scan", "Rust style", None),
            ),
            (
                memory(
                    json!({"operation": "scan", "query": "Rust style", "limit": 4}),
                    ToolState::Succeeded,
                    Some(json!({"operation": "scan", "abstained": true, "candidates": []})),
                ),
                ("Memory scan", "Rust style", Some("abstained")),
            ),
            (
                memory(
                    json!({"operation": "read", "keys": [{"id": 7, "version": 2}]}),
                    ToolState::Succeeded,
                    Some(
                        json!({"operation": "read", "memories": [record(7, 2, "Use early returns.")]}),
                    ),
                ),
                ("Memory read", "7@v2", Some("1 memory")),
            ),
            (
                memory(
                    json!({"operation": "read", "keys": [
                        {"id": 7, "version": 1},
                        {"id": 8, "version": 1},
                        {"id": 9, "version": 1}
                    ]}),
                    ToolState::Succeeded,
                    Some(json!({
                        "operation": "read",
                        "memories": [
                            record(7, 1, "First."),
                            record(8, 1, "Second."),
                            record(9, 1, "Third."),
                        ]
                    })),
                ),
                ("Memory read", "7@v1, 8@v1, 9@v1", Some("3 memories")),
            ),
            (
                memory(
                    json!({"operation": "put", "content": "Use early returns."}),
                    ToolState::Succeeded,
                    Some(
                        json!({"operation": "put", "memory": record(7, 1, "Use early returns."), "replaced": null}),
                    ),
                ),
                ("Memory stored", "7@v1", None),
            ),
            (
                memory(
                    json!({"operation": "put", "content": "Use explicit flow.", "replace": {"id": 7, "version": 1}}),
                    ToolState::Succeeded,
                    Some(
                        json!({"operation": "put", "memory": record(7, 2, "Use explicit flow."), "replaced": {"id": 7, "version": 1}}),
                    ),
                ),
                ("Memory replaced", "7@v2", None),
            ),
            (
                memory(
                    json!({"operation": "delete", "key": {"id": 7, "version": 2}}),
                    ToolState::Succeeded,
                    Some(json!({
                        "operation": "delete",
                        "key": {"id": 7, "version": 2}
                    })),
                ),
                ("Memory deleted", "7@v2", None),
            ),
        ];

        for (tool, expected) in cases {
            assert_summary(&tool, expected);
        }
    }

    #[test]
    fn failed_calls_keep_the_requested_operation_and_show_the_error() {
        let tool = memory(
            json!({"operation": "delete", "key": {"id": 7, "version": 2}}),
            ToolState::Failed,
            Some(json!({"error": "version conflict"})),
        );

        assert_summary(&tool, ("Memory delete", "7@v2", None));
        assert_eq!(
            rendered(&render(&tool, 100, &Theme::default())),
            ["  ▶ × Memory delete  7@v2 · version conflict"]
        );
    }

    #[test]
    fn shared_memory_summaries_include_the_author_namespace() {
        let tool = memory(
            json!({
                "operation": "read",
                "keys": [{"namespace": "alice", "id": 7, "version": 2}]
            }),
            ToolState::Succeeded,
            Some(json!({
                "operation": "read",
                "memories": [{
                    "key": {"namespace": "alice", "id": 7, "version": 2},
                    "content": "Use shared invariants.",
                    "created_at_ms": 10,
                    "updated_at_ms": 20,
                    "last_scanned_at_ms": null,
                    "scan_count": 2,
                    "last_used_at_ms": 30,
                    "use_count": 1,
                    "probation_until_ms": null
                }]
            })),
        );

        assert_summary(&tool, ("Memory read", "alice:7@v2", Some("1 memory")));
    }

    #[test]
    fn completed_results_label_the_backend_even_when_empty_or_abstaining() {
        let cases = [
            (
                memory(
                    json!({"operation": "scan", "query": "style"}),
                    ToolState::Succeeded,
                    Some(json!({
                        "operation": "scan",
                        "backend": {"source": "local", "namespace": null, "role": null},
                        "abstained": true,
                        "candidates": []
                    })),
                ),
                ("Memory scan · local · style", "", Some("abstained")),
            ),
            (
                memory(
                    json!({"operation": "read", "keys": [{"namespace": "alice", "id": 7, "version": 2}]}),
                    ToolState::Succeeded,
                    Some(json!({
                        "operation": "read",
                        "backend": {"source": "remote", "namespace": "bob", "role": "reader"},
                        "memories": []
                    })),
                ),
                ("Memory read · remote · alice:7@v2", "", Some("0 memories")),
            ),
        ];

        for (tool, expected) in cases {
            assert_summary(&tool, expected);
        }
    }

    #[test]
    fn completed_memory_store_uses_operation_source_and_attributed_key() {
        let tool = memory(
            json!({"operation": "put", "content": "Keep rendering concise."}),
            ToolState::Succeeded,
            Some(json!({
                "operation": "put",
                "backend": {"source": "remote", "namespace": "ben", "role": "writer"},
                "memory": {
                    "key": {"namespace": "ben", "id": 1, "version": 1},
                    "content": "Keep rendering concise.",
                    "created_at_ms": 10,
                    "updated_at_ms": 10,
                    "last_scanned_at_ms": null,
                    "scan_count": 0,
                    "last_used_at_ms": null,
                    "use_count": 0,
                    "probation_until_ms": null
                },
                "replaced": null
            })),
        );

        assert_summary(&tool, ("Memory store · remote · ben:1@v1", "", None));
    }

    #[test]
    fn backendless_legacy_results_and_pending_calls_keep_generic_titles() {
        let legacy = memory(
            json!({"operation": "scan", "query": "style"}),
            ToolState::Succeeded,
            Some(json!({"operation": "scan", "abstained": false, "candidates": []})),
        );
        let pending = memory(
            json!({"operation": "scan", "query": "style"}),
            ToolState::Running,
            None,
        );

        assert_summary(&legacy, ("Memory scan", "style", Some("0 candidates")));
        assert_summary(&pending, ("Memory scan", "style", None));
    }

    #[test]
    fn legacy_exact_operation_arguments_remain_renderable() {
        let read = memory(
            json!({"operation": "read", "ids": [7]}),
            ToolState::Running,
            None,
        );
        let delete = memory(
            json!({"operation": "delete", "id": 7, "version": 2}),
            ToolState::Running,
            None,
        );

        assert_summary(&read, ("Memory read", "7", None));
        assert_summary(&delete, ("Memory delete", "7@v2", None));
    }

    #[test]
    fn delete_results_accept_remote_flat_and_nested_keys() {
        let cases = [
            (
                json!({
                    "operation": "delete",
                    "backend": {"source": "remote", "namespace": "alice", "role": "writer"},
                    "namespace": "alice",
                    "id": 7
                }),
                "Memory delete · remote · alice:7",
            ),
            (
                json!({
                    "operation": "delete",
                    "backend": {"source": "remote", "namespace": "alice", "role": "writer"},
                    "key": {"namespace": "alice", "id": 7, "version": 2}
                }),
                "Memory delete · remote · alice:7@v2",
            ),
        ];

        for (result, title) in cases {
            let tool = memory(
                json!({
                    "operation": "delete",
                    "key": {"namespace": "alice", "id": 7, "version": 2}
                }),
                ToolState::Succeeded,
                Some(result),
            );
            assert_summary(&tool, (title, "", None));
        }
    }

    #[test]
    fn malformed_and_future_shapes_use_generic_presentation() {
        let cases = [
            memory(json!({"operation": "scan"}), ToolState::Running, None),
            memory(
                json!({"operation": "archive", "id": 7}),
                ToolState::Running,
                None,
            ),
            memory(
                json!({"operation": "scan", "query": "style"}),
                ToolState::Succeeded,
                Some(json!({"operation": "scan", "candidates": "invalid"})),
            ),
        ];

        for tool in cases {
            assert_eq!(
                expanded(&tool, 74).footer.as_deref(),
                Some("arguments and result"),
                "{}",
                tool.arguments
            );
        }
    }

    #[test]
    fn collapsed_put_and_read_never_reveal_memory_content() {
        let secret = "private atomic memory contents";
        let cases = [
            (
                memory(
                    json!({"operation": "put", "content": secret}),
                    ToolState::Running,
                    None,
                ),
                ("Memory store", "", None),
            ),
            (
                memory(
                    json!({"operation": "put", "content": secret}),
                    ToolState::Succeeded,
                    Some(
                        json!({"operation": "put", "memory": record(1, 1, secret), "replaced": false}),
                    ),
                ),
                ("Memory stored", "1@v1", None),
            ),
            (
                memory(
                    json!({"operation": "read", "keys": [{"id": 1, "version": 1}]}),
                    ToolState::Succeeded,
                    Some(json!({"operation": "read", "memories": [record(1, 1, secret)]})),
                ),
                ("Memory read", "1@v1", Some("1 memory")),
            ),
        ];

        for (tool, expected) in cases {
            assert_summary(&tool, expected);
            let presentation = collapsed(&tool);
            assert!(presentation.details.is_empty());
            assert!(presentation.selection_source.is_empty());
        }
    }

    #[test]
    fn expanded_scan_is_bounded_and_whitelists_candidate_fields() {
        let candidates = (0..12)
            .map(|id| {
                json!({
                    "key": {"id": id, "version": 3},
                    "preview": format!("preview-{id} {}", "x".repeat(400)),
                    "score": 0.875,
                    "content": "must not be shown"
                })
            })
            .collect::<Vec<_>>();
        let tool = memory(
            json!({"operation": "scan", "query": "style"}),
            ToolState::Succeeded,
            Some(json!({"operation": "scan", "abstained": false, "candidates": candidates})),
        );

        let presentation = expanded(&tool, 74);

        let expected = (0..10)
            .flat_map(|id| {
                [
                    format!("{id}@v3 · score 0.875"),
                    format!("preview-{id} {}", "x".repeat(230)),
                ]
            })
            .collect::<Vec<_>>();
        assert_eq!(
            presentation
                .selection_source
                .split('\n')
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(presentation.footer.as_deref(), Some("10 of 12 candidates"));
    }

    #[test]
    fn expanded_read_and_put_show_atomic_content_and_selected_metadata() {
        let cases = [
            (
                memory(
                    json!({"operation": "read", "keys": [{"id": 9, "version": 4}]}),
                    ToolState::Succeeded,
                    Some(
                        json!({"operation": "read", "memories": [record(9, 4, "Use explicit data flow.")]}),
                    ),
                ),
                "1 memory",
            ),
            (
                memory(
                    json!({"operation": "put", "content": "Use explicit data flow."}),
                    ToolState::Succeeded,
                    Some(
                        json!({"operation": "put", "memory": record(9, 4, "Use explicit data flow."), "replaced": false}),
                    ),
                ),
                "memory record",
            ),
        ];

        for (tool, footer) in cases {
            let presentation = expanded(&tool, 94);
            assert_eq!(
                presentation.selection_source,
                format!("9@v4\nUse explicit data flow.\n{RECORD_METADATA}")
            );
            assert_eq!(presentation.footer.as_deref(), Some(footer));
        }
    }

    #[test]
    fn overwrite_shows_a_selectable_colored_diff_only_when_expanded() {
        let tool = memory(
            json!({"operation": "put", "content": "Use explicit flow.\nKeep context.", "replace": {"id": 7, "version": 1}}),
            ToolState::Succeeded,
            Some(json!({
                "operation": "put",
                "backend": {"source": "local", "namespace": null, "role": null},
                "memory": record(7, 2, "Use explicit flow.\nKeep context."),
                "replaced": true,
                "previous_content": "Use early returns.\nKeep context."
            })),
        );
        let theme = Theme::default();

        assert_summary(&tool, ("Memory replace · local · 7@v2", "", None));
        assert!(collapsed(&tool).details.is_empty());

        let presentation = expanded(&tool, 74);
        assert_eq!(
            presentation.selection_source,
            "7@v2\n- Use early returns.\n+ Use explicit flow.\n  Keep context."
        );
        let colors = presentation
            .details
            .iter()
            .map(|line| line.spans.first().and_then(|span| span.style.fg))
            .collect::<Vec<_>>();
        assert_eq!(
            colors,
            [
                Some(theme.accent()),
                Some(Color::Red),
                Some(Color::Green),
                Some(theme.text()),
            ]
        );
        assert_eq!(
            rendered(&render_expanded(&tool, 80, &theme)),
            [
                "  ▼ ✓ Memory replace · local · 7@v2",
                "    │ 7@v2",
                "    │ - Use early returns.",
                "    │ + Use explicit flow.",
                "    │ Keep context.",
                "    └ details",
            ]
        );
        for width in 1..=12 {
            assert!(
                render_expanded(&tool, width, &theme)
                    .iter()
                    .all(|line| line.width() <= usize::from(width))
            );
        }
    }

    #[test]
    fn delete_details_show_only_the_key_when_expanded() {
        let tool = memory(
            json!({"operation": "delete", "key": {"id": 3, "version": 8}}),
            ToolState::Succeeded,
            Some(json!({
                "operation": "delete",
                "key": {"id": 3, "version": 8}
            })),
        );

        assert_eq!(
            rendered(&render(&tool, 80, &Theme::default())),
            ["  ▶ ✓ Memory deleted  3@v8"]
        );
        assert_eq!(
            rendered(&render_expanded(&tool, 80, &Theme::default())),
            [
                "  ▼ ✓ Memory deleted  3@v8",
                "    │ 3@v8",
                "    └ memory key"
            ]
        );
    }

    #[test]
    fn memory_rendering_never_exceeds_narrow_widths() {
        let tool = memory(
            json!({"operation": "read", "keys": [{"id": 12345, "version": 7}]}),
            ToolState::Succeeded,
            Some(
                json!({"operation": "read", "memories": [record(12345, 7, "wide content that must wrap safely")] }),
            ),
        );

        for width in 1..=12 {
            for lines in [
                render(&tool, width, &Theme::default()),
                render_expanded(&tool, width, &Theme::default()),
            ] {
                assert!(!lines.is_empty());
                assert!(lines.iter().all(|line| line.width() <= usize::from(width)));
            }
        }
    }
}
