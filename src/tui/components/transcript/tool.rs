mod code;
mod media;
mod patch;
mod plan;
mod shell;
mod web;

use super::markdown::{sanitize, wrap_plain, wrap_spans};
use crate::tui::{
    format::{format_duration, humanize_tool},
    theme::Theme,
    transcript::{ToolEntry, ToolState},
};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use serde_json::Value;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub(super) fn render(tool: &ToolEntry, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    render_state(tool, width, theme, false)
}

pub(super) fn render_expanded(tool: &ToolEntry, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    render_state(tool, width, theme, true)
}

fn render_state(tool: &ToolEntry, width: u16, theme: &Theme, expanded: bool) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let detail_width = width.saturating_sub(6).max(1);
    let presentation = match tool.name.as_str() {
        "exec_command" | "write_stdin" => shell::present(tool, detail_width, theme, expanded),
        "update_plan" => plan::present(tool, detail_width, theme, expanded),
        "apply_patch" => patch::present(tool, detail_width, theme, expanded),
        "web__run" => web::present(tool, detail_width, theme, expanded),
        "view_image" | "image_gen__imagegen" => media::present(tool, detail_width, theme, expanded),
        "exec" | "wait" => code::present(tool, detail_width, theme, expanded),
        _ => generic(tool, detail_width, theme, expanded),
    };
    let mut lines = summary_lines(tool, &presentation, width, theme, expanded);
    if !expanded {
        return lines;
    }
    append_details(&mut lines, presentation, width, theme);
    lines
}

pub(super) struct Presentation {
    title: String,
    subject: Subject,
    outcome: Option<String>,
    details: Vec<Line<'static>>,
    footer: Option<String>,
}

enum Subject {
    Plain(String),
    Styled(Vec<Span<'static>>),
}

impl Presentation {
    pub(super) fn new(title: impl Into<String>, subject: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            subject: Subject::Plain(subject.into()),
            outcome: None,
            details: Vec::new(),
            footer: None,
        }
    }

    pub(super) fn styled_subject(title: impl Into<String>, subject: Vec<Span<'static>>) -> Self {
        Self {
            title: title.into(),
            subject: Subject::Styled(subject),
            outcome: None,
            details: Vec::new(),
            footer: None,
        }
    }

    pub(super) fn outcome(mut self, outcome: impl Into<String>) -> Self {
        self.outcome = Some(outcome.into());
        self
    }

    pub(super) fn details(mut self, details: Vec<Line<'static>>) -> Self {
        self.details = details;
        self
    }

    pub(super) fn footer(mut self, footer: impl Into<String>) -> Self {
        self.footer = Some(footer.into());
        self
    }
}

fn summary_lines(
    tool: &ToolEntry,
    presentation: &Presentation,
    width: u16,
    theme: &Theme,
    expanded: bool,
) -> Vec<Line<'static>> {
    let border = Style::default().fg(theme.border());
    let status = status_style(tool.state, theme);
    let prefix = vec![
        Span::raw("  "),
        Span::styled(if expanded { "▼ " } else { "▶ " }, border),
        Span::styled(format!("{} ", status_symbol(tool.state)), status),
    ];
    let mut content = Vec::new();
    append_span(
        &mut content,
        &presentation.title,
        Style::default()
            .fg(theme.text())
            .add_modifier(Modifier::BOLD),
    );
    push_subject(&mut content, &presentation.subject, theme);
    if let Some(outcome) = &presentation.outcome {
        append_span(
            &mut content,
            &format!(" · {outcome}"),
            Style::default().fg(theme.muted()),
        );
    }
    if tool.state == ToolState::Failed
        && let Some(error) = first_error_line(tool.result.as_ref())
    {
        append_span(
            &mut content,
            &format!(" · {error}"),
            Style::default().fg(theme.thinking_xhigh()),
        );
    }
    if let Some(duration) = tool.duration_ns {
        append_span(
            &mut content,
            &format!(" · {}", format_duration(duration)),
            Style::default().fg(theme.muted()),
        );
    }

    const PREFIX_WIDTH: u16 = 6;
    if width <= PREFIX_WIDTH {
        let spans = prefix.into_iter().chain(content).collect::<Vec<_>>();
        return wrap_spans(&spans, width, true);
    }

    let mut lines = wrap_spans(&content, width - PREFIX_WIDTH, true);
    for (index, line) in lines.iter_mut().enumerate() {
        let line_prefix = if index == 0 {
            prefix.clone()
        } else {
            vec![Span::raw("      ")]
        };
        line.spans.splice(0..0, line_prefix);
    }
    lines
}

fn append_span(spans: &mut Vec<Span<'static>>, text: &str, style: Style) {
    let text = sanitize(text);
    if !text.is_empty() {
        spans.push(Span::styled(text, style));
    }
}

fn push_subject(spans: &mut Vec<Span<'static>>, subject: &Subject, theme: &Theme) {
    match subject {
        Subject::Plain(subject) if !subject.is_empty() => {
            append_span(
                spans,
                &format!("  {subject}"),
                Style::default().fg(theme.text()),
            );
        }
        Subject::Styled(subject) if !subject.is_empty() => {
            append_span(spans, "  ", Style::default());
            for span in subject {
                append_span(spans, &span.content, span.style);
            }
        }
        Subject::Plain(_) | Subject::Styled(_) => {}
    }
}

fn append_details(
    lines: &mut Vec<Line<'static>>,
    presentation: Presentation,
    width: u16,
    theme: &Theme,
) {
    let rail = Style::default().fg(theme.border());
    if width < 7 {
        lines.extend(
            presentation
                .details
                .into_iter()
                .map(|line| truncate_line(line, width)),
        );
        if let Some(footer) = presentation.footer {
            lines.push(Line::from(Span::styled(
                truncate(&sanitize(&footer), width),
                Style::default().fg(theme.muted()),
            )));
        }
        return;
    }
    for detail in presentation.details {
        lines.push(Line::from(
            std::iter::once(Span::styled("    │ ", rail))
                .chain(detail.spans)
                .collect::<Vec<_>>(),
        ));
    }
    let footer = presentation.footer.unwrap_or_else(|| "details".to_owned());
    let footer = truncate(&sanitize(&footer), width.saturating_sub(6));
    lines.push(Line::from(vec![
        Span::styled("    └ ", rail),
        Span::styled(footer, Style::default().fg(theme.muted())),
    ]));
}

fn truncate_line(line: Line<'static>, width: u16) -> Line<'static> {
    let mut spans = Vec::new();
    let mut remaining = width;
    for span in line.spans {
        push_span(&mut spans, &mut remaining, &span.content, span.style);
        if remaining == 0 {
            break;
        }
    }
    Line::from(spans)
}

fn generic(tool: &ToolEntry, width: u16, theme: &Theme, expanded: bool) -> Presentation {
    let title = humanize_tool(&tool.name);
    let subject = meaningful_subject(&tool.arguments).unwrap_or_else(|| {
        let count = tool.arguments.as_object().map_or(0, serde_json::Map::len);
        format!("{count} arguments")
    });
    let mut presentation = Presentation::new(title, subject);
    if !expanded {
        return presentation;
    }
    let mut details = pretty_value(&tool.arguments, width, theme);
    if let Some(result) = &tool.result {
        details.extend(render_result(result, width, theme));
    }
    presentation.details = details;
    presentation.footer = Some("arguments and result".to_owned());
    presentation
}

pub(super) fn render_result(value: &Value, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    if contains_image_data(value) {
        return wrap_plain(
            &format!("image data · {}", format_bytes(value.to_string().len())),
            width,
            Style::default().fg(theme.muted()),
        );
    }
    if let Some(text) = value.as_str() {
        return wrap_plain(text, width, Style::default().fg(theme.text()));
    }
    pretty_value(value, width, theme)
}

pub(super) fn pretty_value(value: &Value, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    let rendered = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
    wrap_plain(
        &rendered,
        width,
        Style::default()
            .fg(theme.code_text())
            .bg(theme.code_background()),
    )
}

pub(super) fn format_bytes(bytes: usize) -> String {
    if bytes >= 1_048_576 {
        return format!("{:.1} MiB", bytes as f64 / 1_048_576.0);
    }
    if bytes >= 1024 {
        return format!("{:.1} KiB", bytes as f64 / 1024.0);
    }
    format!("{bytes} B")
}

fn meaningful_subject(arguments: &Value) -> Option<String> {
    ["path", "query", "prompt", "url", "name"]
        .into_iter()
        .find_map(|key| arguments.get(key).and_then(Value::as_str))
        .map(|value| sanitize(value.lines().next().unwrap_or_default()))
}

fn first_error_line(result: Option<&Value>) -> Option<String> {
    let result = result?;
    let text = result
        .get("error")
        .and_then(Value::as_str)
        .or_else(|| result.get("output").and_then(Value::as_str))
        .or_else(|| result.as_str())?;
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(sanitize)
}

fn contains_image_data(value: &Value) -> bool {
    match value {
        Value::String(text) => text.starts_with("data:image/"),
        Value::Array(values) => values.iter().any(contains_image_data),
        Value::Object(values) => values.values().any(contains_image_data),
        _ => false,
    }
}

fn push_span(spans: &mut Vec<Span<'static>>, remaining: &mut u16, text: &str, style: Style) {
    if *remaining == 0 {
        return;
    }
    let rendered = truncate(text, *remaining);
    let used = u16::try_from(UnicodeWidthStr::width(rendered.as_str())).unwrap_or(u16::MAX);
    *remaining = remaining.saturating_sub(used);
    if !rendered.is_empty() {
        spans.push(Span::styled(rendered, style));
    }
}

fn truncate(text: &str, width: u16) -> String {
    let mut rendered = String::new();
    let mut used = 0_u16;
    for grapheme in text.graphemes(true) {
        let next = used
            .saturating_add(u16::try_from(UnicodeWidthStr::width(grapheme)).unwrap_or(u16::MAX));
        if next > width {
            break;
        }
        rendered.push_str(grapheme);
        used = next;
    }
    rendered
}

fn status_symbol(state: ToolState) -> &'static str {
    match state {
        ToolState::Running => "◌",
        ToolState::Succeeded => "✓",
        ToolState::Failed => "×",
    }
}

fn status_style(state: ToolState, theme: &Theme) -> Style {
    let color = match state {
        ToolState::Running => theme.accent(),
        ToolState::Succeeded => Color::Green,
        ToolState::Failed => theme.thinking_xhigh(),
    };
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

#[cfg(test)]
mod tests {
    use super::{render, render_expanded};
    use crate::tui::{
        theme::Theme,
        transcript::{ToolEntry, ToolState},
    };
    use ratatui::style::Color;
    use serde_json::json;

    fn tool(name: &str, arguments: serde_json::Value) -> ToolEntry {
        ToolEntry {
            name: name.to_owned(),
            arguments,
            state: ToolState::Succeeded,
            duration_ns: Some(1_200_000_000),
            result: None,
            metadata: None,
            substeps: Vec::new(),
        }
    }

    #[test]
    fn completed_shell_is_a_single_collapsed_summary() {
        let mut shell = tool(
            "exec_command",
            json!({"cmd": "cargo test", "workdir": "/work"}),
        );
        shell.result = Some(json!({
            "output": "all tests passed\nsecond line",
            "exit_code": 0,
            "wall_time_seconds": 1.2,
        }));

        let lines = render(&shell, 80, &Theme::default());

        assert_eq!(lines.len(), 1);
        assert_eq!(
            lines[0].to_string(),
            "  ▶ ✓ Shell  $ cargo test · exit 0 · 1.2s"
        );
        let checkmark = lines[0]
            .spans
            .iter()
            .find(|span| span.content == "✓ ")
            .expect("successful tool should render a checkmark");
        assert_eq!(checkmark.style.fg, Some(Color::Green));
    }

    #[test]
    fn collapsed_tool_summary_wraps_instead_of_discarding_overflow() {
        let shell = tool(
            "exec_command",
            json!({"cmd": "cargo test --all-targets --no-fail-fast --workspace"}),
        );

        let lines = render(&shell, 32, &Theme::default());
        let rendered = lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" ");
        let rendered = rendered.split_whitespace().collect::<Vec<_>>().join(" ");

        assert!(lines.len() > 1);
        assert!(lines.iter().all(|line| line.width() <= 32));
        assert!(
            lines
                .iter()
                .skip(1)
                .all(|line| line.to_string().starts_with("      "))
        );
        assert!(rendered.contains("cargo test --all-targets --no-fail-fast --workspace"));
        assert!(rendered.contains("1.2s"));
    }

    #[test]
    fn collapsed_web_call_does_not_render_its_large_result() {
        let mut web = tool("web__run", json!({"search_query": [{"q": "rust ratatui"}]}));
        web.result = Some(json!("large result body\n".repeat(1_000)));

        let lines = render(&web, 80, &Theme::default());
        let rendered = lines.iter().map(ToString::to_string).collect::<String>();

        assert_eq!(lines.len(), 1);
        assert!(rendered.contains("search \"rust ratatui\""));
        assert!(!rendered.contains("large result body"));
    }

    #[test]
    fn collapsed_failure_includes_the_first_error_line() {
        let mut shell = tool("exec_command", json!({"cmd": "cargo test"}));
        shell.state = ToolState::Failed;
        shell.result = Some(json!({
            "output": "compilation failed\nmore diagnostics",
            "exit_code": 101,
        }));

        let lines = render(&shell, 80, &Theme::default());

        assert_eq!(lines.len(), 1);
        assert!(lines[0].to_string().contains("compilation failed"));
        assert!(!lines[0].to_string().contains("more diagnostics"));
    }

    #[test]
    fn killed_shell_renders_failure_without_a_checkmark() {
        let mut shell = tool("exec_command", json!({"cmd": "sleep 100"}));
        shell.state = ToolState::Failed;
        shell.result = Some(json!({"output": "", "exit_code": null}));

        let rendered = render(&shell, 80, &Theme::default())[0].to_string();

        assert!(rendered.contains("× Shell"));
        assert!(rendered.contains("terminated"));
        assert!(!rendered.contains('✓'));
    }

    #[test]
    fn expansion_reveals_shell_output() {
        let mut shell = tool("exec_command", json!({"cmd": "cargo test"}));
        shell.result = Some(json!({"output": "all tests passed", "exit_code": 0}));

        let rendered = render_expanded(&shell, 80, &Theme::default())
            .into_iter()
            .map(|line| line.to_string())
            .collect::<String>();

        assert!(rendered.contains("all tests passed"));
        assert!(rendered.contains("└ 1 line · 16 B"));
    }

    #[test]
    fn image_data_is_never_rendered_verbatim() {
        let mut image = tool("view_image", json!({"path": "image.png"}));
        image.result = Some(json!({"image_url": "data:image/png;base64,AAAA"}));

        let rendered = render_expanded(&image, 40, &Theme::default())
            .into_iter()
            .map(|line| line.to_string())
            .collect::<String>();

        assert!(!rendered.contains("base64"));
        assert!(rendered.contains("image returned"));
    }

    #[test]
    fn every_first_party_tool_has_a_semantic_summary() {
        let cases = [
            ("exec", json!("text(true)"), "Code  0 emitted items"),
            (
                "update_plan",
                json!({"plan": [{"step": "done", "status": "completed"}]}),
                "Plan  1/1 complete",
            ),
            (
                "apply_patch",
                json!("*** Begin Patch\n*** Update File: src/main.rs\n+new\n-old\n*** End Patch"),
                "Patch  1 file · +1 −1",
            ),
            (
                "view_image",
                json!({"path": "/tmp/image.png", "detail": "original"}),
                "Image  /tmp/image.png · original",
            ),
            (
                "image_gen__imagegen",
                json!({"prompt": "a compact terminal"}),
                "Image generation  a compact terminal",
            ),
            ("wait", json!({"cell_id": "12"}), "Wait  background work"),
            (
                "mcp__files__read",
                json!({"path": "/tmp/file"}),
                "files · read  /tmp/file",
            ),
        ];

        for (name, arguments, expected) in cases {
            let rendered = render(&tool(name, arguments), 100, &Theme::default())[0].to_string();
            assert!(rendered.contains(expected), "{name}: {rendered}");
        }
    }

    #[test]
    fn patch_summary_colors_additions_green_and_deletions_red() {
        let patch = tool(
            "apply_patch",
            json!("*** Begin Patch\n*** Update File: src/main.rs\n+new\n-old\n*** End Patch"),
        );

        let lines = render(&patch, 100, &Theme::default());
        let additions = lines[0]
            .spans
            .iter()
            .find(|span| span.content == "+1")
            .expect("patch summary should include additions");
        let deletions = lines[0]
            .spans
            .iter()
            .find(|span| span.content == "−1")
            .expect("patch summary should include deletions");

        assert_eq!(additions.style.fg, Some(Color::Green));
        assert_eq!(deletions.style.fg, Some(Color::Red));
    }

    #[test]
    fn expanded_patch_colors_diff_lines() {
        let patch = tool(
            "apply_patch",
            json!("*** Begin Patch\n*** Update File: src/main.rs\n+new\n-old\n*** End Patch"),
        );

        let lines = render_expanded(&patch, 80, &Theme::default());
        let addition = lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.content == "+ ")
            .expect("addition should be rendered");
        let deletion = lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.content == "- ")
            .expect("deletion should be rendered");

        assert_eq!(addition.style.fg, Some(Color::Green));
        assert_eq!(deletion.style.fg, Some(Color::Red));
    }

    #[test]
    fn expanded_patch_renders_each_hunk_with_its_file_and_context() {
        let patch = tool(
            "apply_patch",
            json!(
                "*** Begin Patch\n*** Update File: src/main.rs\n@@ fn main()\n-old();\n+new();\n*** End Patch"
            ),
        );

        let rendered = render_expanded(&patch, 80, &Theme::default())
            .iter()
            .map(ToString::to_string)
            .collect::<String>();

        assert!(rendered.contains("src/main.rs"));
        assert!(rendered.contains("fn main()"));
        assert!(rendered.contains("+1 −1"));
    }

    #[test]
    fn mixed_web_operations_are_summarized_by_count() {
        let web = tool(
            "web__run",
            json!({
                "search_query": [{"q": "one"}, {"q": "two"}],
                "open": [{"ref_id": "turn0search0"}],
                "weather": [{"location": "Amsterdam"}],
            }),
        );

        let rendered = render(&web, 100, &Theme::default())[0].to_string();

        assert!(rendered.contains("search 2 · open 1 · weather 1"));
    }

    #[test]
    fn expanded_web_results_hide_protocol_annotations() {
        let mut web = tool("web__run", json!({"open": [{"ref_id": "turn0search0"}]}));
        web.result = Some(json!(
            "citeturn0view0 Useful content [wordlim: 200]\nSecond line"
        ));

        let rendered = render_expanded(&web, 80, &Theme::default())
            .into_iter()
            .map(|line| line.to_string())
            .collect::<String>();

        assert!(rendered.contains("Useful content"));
        assert!(rendered.contains("Second line"));
        assert!(!rendered.contains("cite"));
        assert!(!rendered.contains("wordlim"));
    }

    #[test]
    fn tool_rendering_never_exceeds_narrow_widths() {
        let mut shell = tool(
            "exec_command",
            json!({"cmd": "cargo test --all-targets --no-fail-fast"}),
        );
        shell.result = Some(json!({
            "output": "a very long output line that must wrap safely",
            "exit_code": 0,
        }));

        for width in 1..=12 {
            let collapsed = render(&shell, width, &Theme::default());
            assert!(!collapsed.is_empty());
            assert!(
                collapsed
                    .iter()
                    .all(|line| line.width() <= usize::from(width))
            );

            let expanded = render_expanded(&shell, width, &Theme::default());
            assert!(!expanded.is_empty());
            assert!(
                expanded
                    .iter()
                    .all(|line| line.width() <= usize::from(width))
            );
        }
    }
}
