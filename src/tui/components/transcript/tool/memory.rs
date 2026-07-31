use super::{Presentation, generic};
use crate::tui::{theme::Theme, transcript::ToolEntry};
use ratatui::{style::Style, text::Line};
use serde_json::{Map, Value};

const MAX_SCAN_CANDIDATES: usize = 8;
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
                keys: parse_keys(arguments.get("ids")?)?,
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
        abstained: bool,
        candidates: Vec<Candidate<'a>>,
    },
    Read {
        memories: Vec<Memory<'a>>,
    },
    Put {
        memory: Memory<'a>,
        replaced: bool,
    },
    Delete {
        key: MemoryKey,
    },
}

impl<'a> ResultValue<'a> {
    fn parse(tool: &'a ToolEntry, expected_operation: &str) -> Option<Self> {
        let Some(result) = tool.result.as_ref() else {
            return Some(Self::Pending);
        };
        if tool.state == crate::tui::transcript::ToolState::Failed
            && result.get("error").and_then(Value::as_str).is_some()
        {
            return Some(Self::Failed);
        }
        if result.get("operation").and_then(Value::as_str) != Some(expected_operation) {
            return None;
        }

        match expected_operation {
            "scan" => Some(Self::Scan {
                abstained: result.get("abstained")?.as_bool()?,
                candidates: result
                    .get("candidates")?
                    .as_array()?
                    .iter()
                    .map(Candidate::parse)
                    .collect::<Option<Vec<_>>>()?,
            }),
            "read" => Some(Self::Read {
                memories: result
                    .get("memories")?
                    .as_array()?
                    .iter()
                    .map(Memory::parse)
                    .collect::<Option<Vec<_>>>()?,
            }),
            "put" => Some(Self::Put {
                memory: Memory::parse(result.get("memory")?)?,
                replaced: was_replaced(result.get("replaced")?),
            }),
            "delete" => Some(Self::Delete {
                key: MemoryKey::parse(result)?,
            }),
            _ => None,
        }
    }
}

#[derive(Clone)]
struct MemoryKey {
    id: String,
    version: Option<u64>,
}

impl MemoryKey {
    fn parse(value: &Value) -> Option<Self> {
        if !value.is_object() {
            return Some(Self {
                id: parse_id(value)?,
                version: None,
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
        })
    }

    fn parse_versioned(value: &Value) -> Option<Self> {
        let key = Self::parse(value)?;
        key.version?;
        Some(key)
    }

    fn display(&self) -> String {
        self.version.as_ref().map_or_else(
            || self.id.clone(),
            |version| format!("{}@v{version}", self.id),
        )
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
    let presentation = Presentation::new("Memory scan", query).outcome(outcome);
    if !expanded {
        return presentation;
    }

    let shown = candidates.len().min(MAX_SCAN_CANDIDATES);
    let mut details = Vec::new();
    for candidate in candidates.iter().take(shown) {
        details.extend(wrap(
            &format!(
                "{} · score {}",
                candidate.key.display(),
                format_score(candidate.score)
            ),
            width,
            Style::default().fg(theme.accent()),
        ));
        let preview = super::truncate(candidate.preview, MAX_PREVIEW_WIDTH);
        details.extend(wrap(&preview, width, Style::default().fg(theme.text())));
    }
    let footer = if abstained {
        "memory scan abstained".to_owned()
    } else if shown < candidates.len() {
        format!("{shown} of {} candidates", candidates.len())
    } else {
        count_label(candidates.len(), "candidate", "candidates")
    };
    presentation.details(details).footer(footer)
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
    let ResultValue::Read { memories } = result else {
        return Presentation::new("Memory read", subject);
    };
    let count = count_label(memories.len(), "memory", "memories");
    let presentation = Presentation::new("Memory read", subject).outcome(&count);
    if !expanded {
        return presentation;
    }

    let mut details = Vec::new();
    for memory in &memories {
        details.extend(memory_details(memory, width, theme));
    }
    presentation.details(details).footer(count)
}

fn put(
    content: &str,
    replace: Option<MemoryKey>,
    result: ResultValue<'_>,
    width: u16,
    theme: &Theme,
    expanded: bool,
) -> Presentation {
    let ResultValue::Put { memory, replaced } = result else {
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
            .details(wrap(content, width, Style::default().fg(theme.text())))
            .footer("memory content");
    };

    let title = if replaced {
        "Memory replaced"
    } else {
        "Memory stored"
    };
    let presentation = Presentation::new(title, memory.key.display());
    if !expanded {
        return presentation;
    }
    presentation
        .details(memory_details(&memory, width, theme))
        .footer("memory record")
}

fn delete(
    key: MemoryKey,
    result: ResultValue<'_>,
    width: u16,
    theme: &Theme,
    expanded: bool,
) -> Presentation {
    let (title, key) = match result {
        ResultValue::Delete { key } => ("Memory deleted", key),
        _ => ("Memory delete", key),
    };
    let presentation = Presentation::new(title, key.display());
    if !expanded {
        return presentation;
    }
    presentation
        .details(wrap(
            &key.display(),
            width,
            Style::default().fg(theme.accent()),
        ))
        .footer("memory key")
}

fn memory_details(memory: &Memory<'_>, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    let mut details = wrap(
        &memory.key.display(),
        width,
        Style::default().fg(theme.accent()),
    );
    details.extend(wrap(
        memory.content,
        width,
        Style::default().fg(theme.text()),
    ));
    if let Some(metadata) = selected_metadata(memory.fields) {
        details.extend(wrap(&metadata, width, Style::default().fg(theme.muted())));
    }
    details
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
    use super::super::{render, render_expanded};
    use crate::tui::{
        theme::Theme,
        transcript::{ToolEntry, ToolState},
    };
    use serde_json::{Value, json};

    fn memory(arguments: Value, state: ToolState, result: Option<Value>) -> ToolEntry {
        ToolEntry {
            name: "memory".to_owned(),
            arguments,
            started_at_unix_ms: 0,
            state,
            duration_ns: None,
            result,
            metadata: None,
            substeps: Vec::new(),
            child_count: 0,
        }
    }

    fn text(lines: &[ratatui::text::Line<'_>]) -> String {
        lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
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

    #[test]
    fn summaries_cover_operations_states_and_results() {
        let cases = [
            (
                memory(
                    json!({"operation": "scan", "query": "Rust style"}),
                    ToolState::Running,
                    None,
                ),
                "Memory scan  Rust style",
            ),
            (
                memory(
                    json!({"operation": "scan", "query": "Rust style", "limit": 4}),
                    ToolState::Succeeded,
                    Some(json!({"operation": "scan", "abstained": true, "candidates": []})),
                ),
                "Memory scan  Rust style · abstained",
            ),
            (
                memory(
                    json!({"operation": "read", "ids": [{"id": 7, "version": 2}]}),
                    ToolState::Succeeded,
                    Some(
                        json!({"operation": "read", "memories": [record(7, 2, "Use early returns.")]}),
                    ),
                ),
                "Memory read  7@v2 · 1 memory",
            ),
            (
                memory(
                    json!({"operation": "read", "ids": [7, 8, 9]}),
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
                "Memory read  7, 8, 9 · 3 memories",
            ),
            (
                memory(
                    json!({"operation": "put", "content": "Use early returns."}),
                    ToolState::Succeeded,
                    Some(
                        json!({"operation": "put", "memory": record(7, 1, "Use early returns."), "replaced": null}),
                    ),
                ),
                "Memory stored  7@v1",
            ),
            (
                memory(
                    json!({"operation": "put", "content": "Use explicit flow.", "replace": {"id": 7, "version": 1}}),
                    ToolState::Succeeded,
                    Some(
                        json!({"operation": "put", "memory": record(7, 2, "Use explicit flow."), "replaced": {"id": 7, "version": 1}}),
                    ),
                ),
                "Memory replaced  7@v2",
            ),
            (
                memory(
                    json!({"operation": "delete", "id": 7, "version": 2}),
                    ToolState::Succeeded,
                    Some(json!({"operation": "delete", "id": 7})),
                ),
                "Memory deleted  7",
            ),
            (
                memory(
                    json!({"operation": "delete", "id": 7, "version": 2}),
                    ToolState::Failed,
                    Some(json!({"error": "version conflict"})),
                ),
                "Memory delete  7@v2 · version conflict",
            ),
        ];

        for (tool, expected) in cases {
            let rendered = text(&render(&tool, 100, &Theme::default()));
            assert!(
                rendered.contains(expected),
                "expected {expected:?} in {rendered:?}"
            );
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
            let rendered = text(&render_expanded(&tool, 80, &Theme::default()));
            assert!(rendered.contains("arguments and result"), "{rendered}");
        }
    }

    #[test]
    fn collapsed_put_and_read_never_reveal_memory_content() {
        let secret = "private atomic memory contents";
        let cases = [
            memory(
                json!({"operation": "put", "content": secret}),
                ToolState::Running,
                None,
            ),
            memory(
                json!({"operation": "put", "content": secret}),
                ToolState::Succeeded,
                Some(
                    json!({"operation": "put", "memory": record(1, 1, secret), "replaced": false}),
                ),
            ),
            memory(
                json!({"operation": "read", "ids": [1]}),
                ToolState::Succeeded,
                Some(json!({"operation": "read", "memories": [record(1, 1, secret)]})),
            ),
        ];

        for tool in cases {
            let rendered = text(&render(&tool, 100, &Theme::default()));
            assert!(!rendered.contains(secret), "{rendered}");
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

        let rendered = text(&render_expanded(&tool, 80, &Theme::default()));

        assert!(rendered.contains("0@v3 · score 0.875"));
        assert!(rendered.contains("7@v3 · score 0.875"));
        assert!(!rendered.contains("8@v3 · score"));
        assert!(!rendered.contains("must not be shown"));
        assert!(rendered.contains("8 of 12 candidates"));
        assert!(!rendered.contains(&"x".repeat(241)));
    }

    #[test]
    fn expanded_read_and_put_show_atomic_content_and_selected_metadata() {
        let cases = [
            memory(
                json!({"operation": "read", "ids": [9]}),
                ToolState::Succeeded,
                Some(
                    json!({"operation": "read", "memories": [record(9, 4, "Use explicit data flow.")]}),
                ),
            ),
            memory(
                json!({"operation": "put", "content": "Use explicit data flow."}),
                ToolState::Succeeded,
                Some(
                    json!({"operation": "put", "memory": record(9, 4, "Use explicit data flow."), "replaced": false}),
                ),
            ),
        ];

        for tool in cases {
            let rendered = text(&render_expanded(&tool, 100, &Theme::default()));
            assert!(rendered.contains("Use explicit data flow."), "{rendered}");
            assert!(rendered.contains("created 10 · updated 20"), "{rendered}");
            assert!(rendered.contains("scans 2"), "{rendered}");
            assert!(rendered.contains("uses 1"), "{rendered}");
        }
    }

    #[test]
    fn expansion_controls_and_delete_details_are_semantic() {
        let tool = memory(
            json!({"operation": "delete", "id": 3, "version": 8}),
            ToolState::Succeeded,
            Some(json!({"operation": "delete", "id": 3})),
        );
        let collapsed = text(&render(&tool, 80, &Theme::default()));
        let expanded = text(&render_expanded(&tool, 80, &Theme::default()));

        assert!(collapsed.contains("▶"));
        assert!(!collapsed.contains("memory key"));
        assert!(expanded.contains("▼"));
        assert!(expanded.contains("└ memory key"));
        assert!(!expanded.contains("content"));
    }

    #[test]
    fn memory_rendering_never_exceeds_narrow_widths() {
        let tool = memory(
            json!({"operation": "read", "ids": [{"id": 12345, "version": 7}]}),
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
