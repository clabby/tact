mod code;
mod media;
mod memory;
mod patch;
mod plan;
mod send_message;
mod shell;
mod web;

use super::markdown::{
    Layout, SourceSpan, plain_selection_spans_excluding, sanitize, wrap_plain, wrap_spans,
};
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
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[cfg(test)]
pub(super) fn render(tool: &ToolEntry, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    render_layout(tool, None, width, theme, false).lines
}

#[cfg(test)]
pub(super) fn render_expanded(tool: &ToolEntry, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    render_layout(tool, None, width, theme, true).lines
}

#[cfg(test)]
pub(super) fn render_live(
    tool: &ToolEntry,
    duration_ns: u64,
    width: u16,
    theme: &Theme,
    expanded: bool,
) -> Vec<Line<'static>> {
    render_layout(tool, Some(duration_ns), width, theme, expanded).lines
}

pub(super) fn render_layout(
    tool: &ToolEntry,
    live_duration_ns: Option<u64>,
    width: u16,
    theme: &Theme,
    expanded: bool,
) -> Layout {
    if width == 0 {
        return Layout {
            lines: Vec::new(),
            images: Vec::new(),
            links: Vec::new(),
            selections: Vec::new(),
            envelopes: Vec::new(),
            selection_source: None,
            image_state: super::markdown::ImageState::None,
        };
    }
    let detail_width = width.saturating_sub(6).max(1);
    let presentation = present(tool, detail_width, theme, expanded);
    let mut lines = summary_lines(
        tool,
        &presentation,
        live_duration_ns,
        width,
        theme,
        expanded,
    );
    if !expanded {
        return Layout {
            links: vec![Vec::new(); lines.len()],
            selections: vec![Vec::new(); lines.len()],
            lines,
            images: Vec::new(),
            envelopes: Vec::new(),
            selection_source: None,
            image_state: super::markdown::ImageState::None,
        };
    }
    let detail_start = lines.len();
    let Presentation {
        details,
        footer,
        selection_source,
        mut detail_selections,
        ..
    } = presentation;
    let selection_source = (!selection_source.is_empty()).then_some(selection_source);
    append_details(&mut lines, details, footer, width, theme);
    let mut selections = vec![Vec::new(); lines.len()];
    let prefix = if width < 7 { 0 } else { 6 };
    for (index, spans) in detail_selections.iter_mut().enumerate() {
        let row = detail_start + index;
        for span in &mut *spans {
            span.columns.start = span.columns.start.saturating_add(prefix);
            span.columns.end = span.columns.end.saturating_add(prefix).min(width);
        }
        spans.retain(|span| span.columns.start < span.columns.end);
        selections[row] = std::mem::take(spans);
    }
    Layout {
        links: vec![Vec::new(); lines.len()],
        lines,
        images: Vec::new(),
        selections,
        envelopes: Vec::new(),
        selection_source,
        image_state: super::markdown::ImageState::None,
    }
}

pub(super) fn render_live_summary(
    tool: &ToolEntry,
    duration_ns: u64,
    width: u16,
    theme: &Theme,
    expanded: bool,
) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let presentation = present(tool, width.saturating_sub(6).max(1), theme, false);
    summary_lines(
        tool,
        &presentation,
        Some(duration_ns),
        width,
        theme,
        expanded,
    )
}

fn present(tool: &ToolEntry, width: u16, theme: &Theme, expanded: bool) -> Presentation {
    match tool.name.as_str() {
        "exec_command" | "write_stdin" => shell::present(tool, width, theme, expanded),
        "update_plan" => plan::present(tool, width, theme, expanded),
        "send_agent_message" => send_message::present(tool, width, theme, expanded),
        "apply_patch" => patch::present(tool, width, theme, expanded),
        "web__run" => web::present(tool, width, theme, expanded),
        "view_image" | "image_gen__imagegen" => media::present(tool, width, theme, expanded),
        "memory" => memory::present(tool, width, theme, expanded),
        "exec" | "wait" => code::present(tool, width, theme, expanded),
        _ => generic(tool, width, theme, expanded),
    }
}

pub(super) struct Presentation {
    title: String,
    subject: Subject,
    outcome: Option<String>,
    details: Vec<Line<'static>>,
    footer: Option<String>,
    summary_overflow: SummaryOverflow,
    selection_source: String,
    detail_selections: Vec<Vec<SourceSpan>>,
}

enum Subject {
    Plain(String),
    Styled(Vec<Span<'static>>),
}

enum SummaryOverflow {
    Wrap,
    Truncate,
}

const TRUNCATION_MARKER: &str = " …";
const TRUNCATION_MARKER_WIDTH: u16 = 2;

impl Presentation {
    pub(super) fn new(title: impl Into<String>, subject: impl Into<String>) -> Self {
        Self {
            title: capitalize_title(&title.into()),
            subject: Subject::Plain(subject.into()),
            outcome: None,
            details: Vec::new(),
            footer: None,
            summary_overflow: SummaryOverflow::Wrap,
            selection_source: String::new(),
            detail_selections: Vec::new(),
        }
    }

    pub(super) fn styled_subject(title: impl Into<String>, subject: Vec<Span<'static>>) -> Self {
        Self {
            title: capitalize_title(&title.into()),
            subject: Subject::Styled(subject),
            outcome: None,
            details: Vec::new(),
            footer: None,
            summary_overflow: SummaryOverflow::Wrap,
            selection_source: String::new(),
            detail_selections: Vec::new(),
        }
    }

    pub(super) fn outcome(mut self, outcome: impl Into<String>) -> Self {
        self.outcome = Some(outcome.into());
        self
    }

    pub(super) fn unselectable_details(mut self, details: Vec<Line<'static>>) -> Self {
        self.detail_selections
            .resize_with(self.detail_selections.len() + details.len(), Vec::new);
        self.details.extend(details);
        self
    }

    pub(super) fn selectable_details(
        self,
        source: impl Into<String>,
        details: Vec<Line<'static>>,
    ) -> Self {
        self.selectable_details_excluding(source, details, &[])
    }

    pub(super) fn selectable_details_excluding(
        mut self,
        source: impl Into<String>,
        details: Vec<Line<'static>>,
        exclusions: &[Vec<Range<u16>>],
    ) -> Self {
        let source = source.into();
        let offset = if self.selection_source.is_empty() {
            0
        } else {
            self.selection_source.push('\n');
            self.selection_source.len()
        };
        let mut selections = plain_selection_spans_excluding(&source, &details, exclusions);
        for spans in &mut selections {
            for span in spans {
                span.source.start = span.source.start.saturating_add(offset);
                span.source.end = span.source.end.saturating_add(offset);
            }
        }
        self.selection_source.push_str(&source);
        self.details.extend(details);
        self.detail_selections.extend(selections);
        self
    }

    pub(super) fn selectable_plain(
        self,
        source: impl Into<String>,
        width: u16,
        style: Style,
    ) -> Self {
        let source = source.into();
        let lines = wrap_plain(&source, width, style);
        self.selectable_details(source, lines)
    }

    pub(super) fn footer(mut self, footer: impl Into<String>) -> Self {
        self.footer = Some(footer.into());
        self
    }

    pub(super) fn truncate_summary(mut self) -> Self {
        self.summary_overflow = SummaryOverflow::Truncate;
        self
    }
}

fn capitalize_title(title: &str) -> String {
    let mut capitalize_next = true;
    let mut capitalized = String::with_capacity(title.len());
    for character in title.chars() {
        if capitalize_next && character.is_alphanumeric() {
            capitalized.extend(character.to_uppercase());
            capitalize_next = false;
        } else {
            capitalized.push(character);
        }
    }
    capitalized
}

fn summary_lines(
    tool: &ToolEntry,
    presentation: &Presentation,
    live_duration_ns: Option<u64>,
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
    let mut outcome_spans = Vec::new();
    if let Some(outcome) = &presentation.outcome {
        append_span(
            &mut outcome_spans,
            &format!(" · {outcome}"),
            Style::default().fg(theme.muted()),
        );
    }
    let mut error_spans = Vec::new();
    if tool.state == ToolState::Failed
        && let Some(error) = first_error_line(tool.result.as_ref())
    {
        append_span(
            &mut error_spans,
            &format!(" · {error}"),
            Style::default().fg(theme.thinking_xhigh()),
        );
    }
    let mut duration_spans = Vec::new();
    if let Some(duration) = live_duration_ns.or(tool.duration_ns) {
        append_span(
            &mut duration_spans,
            &format!(" · {}", format_duration(duration)),
            Style::default().fg(theme.muted()),
        );
    }

    if matches!(presentation.summary_overflow, SummaryOverflow::Truncate) {
        let title_span_count = prefix.len() + usize::from(!content.is_empty());
        let leading = prefix.into_iter().chain(content).collect::<Vec<_>>();
        let full_summary = leading
            .iter()
            .chain(&outcome_spans)
            .chain(&error_spans)
            .chain(&duration_spans)
            .cloned()
            .collect::<Vec<_>>();
        if !spans_need_truncation(&full_summary, width) {
            return vec![Line::from(full_summary)];
        }

        let suffix = outcome_spans
            .into_iter()
            .chain(duration_spans)
            .collect::<Vec<_>>();
        let suffix_width = spans_width(&suffix);
        let minimum_leading_width =
            spans_width(&leading[..title_span_count]).saturating_add(TRUNCATION_MARKER_WIDTH);
        if suffix_width >= width || width - suffix_width < minimum_leading_width {
            return vec![truncate_spans_with_ellipsis(
                &full_summary,
                width,
                Style::default().fg(theme.muted()),
            )];
        }

        let leading_width = width - suffix_width;
        let mut line = truncate_spans_with_ellipsis(
            &leading,
            leading_width,
            Style::default().fg(theme.muted()),
        );
        line.spans.extend(suffix);
        return vec![line];
    }

    content.extend(outcome_spans);
    content.extend(error_spans);
    content.extend(duration_spans);

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

fn spans_need_truncation(spans: &[Span<'static>], width: u16) -> bool {
    spans.iter().any(|span| span.content.contains(['\n', '\r'])) || spans_width(spans) > width
}

fn spans_width(spans: &[Span<'static>]) -> u16 {
    spans.iter().fold(0_u16, |total, span| {
        let width =
            u16::try_from(UnicodeWidthStr::width(span.content.as_ref())).unwrap_or(u16::MAX);
        total.saturating_add(width)
    })
}

fn truncate_spans_with_ellipsis(
    spans: &[Span<'static>],
    width: u16,
    ellipsis_style: Style,
) -> Line<'static> {
    if width == 0 {
        return Line::default();
    }
    let marker = if width < TRUNCATION_MARKER_WIDTH {
        "…"
    } else {
        TRUNCATION_MARKER
    };
    let mut rendered = Vec::new();
    let marker_width =
        u16::try_from(UnicodeWidthStr::width(marker)).unwrap_or(TRUNCATION_MARKER_WIDTH);
    let mut remaining = width.saturating_sub(marker_width);
    for span in spans {
        let line_end = span.content.find(['\n', '\r']);
        let content = line_end.map_or(span.content.as_ref(), |end| &span.content[..end]);
        let shortened = truncate(content, remaining);
        let fully_rendered = shortened == content;
        let used = u16::try_from(UnicodeWidthStr::width(shortened.as_str())).unwrap_or(u16::MAX);
        remaining = remaining.saturating_sub(used);
        if !shortened.is_empty() {
            rendered.push(Span::styled(shortened, span.style));
        }
        if line_end.is_some() || !fully_rendered {
            break;
        }
    }
    rendered.push(Span::styled(marker, ellipsis_style));
    Line::from(rendered)
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
    details: Vec<Line<'static>>,
    footer: Option<String>,
    width: u16,
    theme: &Theme,
) {
    let rail = Style::default().fg(theme.border());
    if width < 7 {
        lines.extend(details.into_iter().map(|line| truncate_line(line, width)));
        if let Some(footer) = footer {
            lines.push(Line::from(Span::styled(
                truncate(&sanitize(&footer), width),
                Style::default().fg(theme.muted()),
            )));
        }
        return;
    }
    for detail in details {
        lines.push(Line::from(
            std::iter::once(Span::styled("    │ ", rail))
                .chain(detail.spans)
                .collect::<Vec<_>>(),
        ));
    }
    let footer = footer.unwrap_or_else(|| "details".to_owned());
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
    let details = pretty_value(&tool.arguments, width, theme);
    presentation = presentation.unselectable_details(details);
    if let Some(result) = &tool.result {
        let (source, details) = selectable_result(result, width, theme);
        presentation = presentation.selectable_details(source, details);
    }
    presentation.footer = Some("arguments and result".to_owned());
    presentation
}

pub(super) fn selectable_result(
    value: &Value,
    width: u16,
    theme: &Theme,
) -> (String, Vec<Line<'static>>) {
    if contains_image_data(value) {
        let source = format!("image data · {}", format_bytes(value.to_string().len()));
        let details = wrap_plain(&source, width, Style::default().fg(theme.muted()));
        return (source, details);
    }
    if let Some(text) = value.as_str() {
        return (
            text.to_owned(),
            wrap_plain(text, width, Style::default().fg(theme.text())),
        );
    }
    let source = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
    let details = wrap_plain(
        &source,
        width,
        Style::default()
            .fg(theme.code_text())
            .bg(theme.code_background()),
    );
    (source, details)
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
    use super::{render, render_expanded, render_layout, render_live};
    use crate::tui::{
        theme::Theme,
        transcript::{ToolEntry, ToolState},
    };
    use ratatui::style::{Color, Modifier};
    use serde_json::json;

    fn tool(name: &str, arguments: serde_json::Value) -> ToolEntry {
        ToolEntry {
            name: name.to_owned(),
            arguments,
            started_at_unix_ms: 0,
            state: ToolState::Succeeded,
            duration_ns: Some(1_200_000_000),
            result: None,
            metadata: None,
            substeps: Vec::new(),
            child_count: 0,
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
    fn shell_commands_use_prompt_and_syntax_colors() {
        let shell = tool(
            "exec_command",
            json!({"cmd": "if test \"$HOME\"; then echo ok; fi"}),
        );
        let theme = Theme::default();

        for (lines, expected_commands) in [
            (render(&shell, 80, &theme), 1),
            (render_expanded(&shell, 80, &theme), 2),
        ] {
            let spans = lines
                .iter()
                .flat_map(|line| &line.spans)
                .collect::<Vec<_>>();
            let prompts = spans
                .iter()
                .filter(|span| span.content == "$ ")
                .collect::<Vec<_>>();
            let keywords = spans
                .iter()
                .filter(|span| span.content.contains("if"))
                .collect::<Vec<_>>();

            assert_eq!(prompts.len(), expected_commands);
            assert_eq!(keywords.len(), expected_commands);
            assert!(
                prompts
                    .iter()
                    .all(|prompt| prompt.style.fg == Some(Color::Yellow))
            );
            assert!(keywords.iter().all(|keyword| {
                keyword.style.add_modifier.contains(Modifier::BOLD)
                    && keyword.style.fg == Some(Color::Blue)
            }));
        }
    }

    #[test]
    fn code_workflow_uses_the_compact_workflow_label() {
        let mut workflow = tool(
            "exec",
            json!("await tools.exec_command({cmd: \"cargo test\"})"),
        );
        workflow.child_count = 2;

        let lines = render(&workflow, 80, &Theme::default());

        assert_eq!(lines[0].to_string(), "  ▶ ✓ Batch  2 tools · 1.2s");
        assert!(lines.iter().all(|line| line.width() <= 80));
    }

    #[test]
    fn non_shell_tool_summary_wraps_instead_of_discarding_overflow() {
        let operation = tool(
            "custom_operation",
            json!({"prompt": "inspect every target without failing fast across the workspace"}),
        );

        let lines = render(&operation, 32, &Theme::default());
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
        assert!(
            rendered.contains("inspect every target without failing fast across the workspace")
        );
        assert!(rendered.contains("1.2s"));
    }

    #[test]
    fn shell_summaries_truncate_subject_and_preserve_status_suffix() {
        let mut shell = tool(
            "exec_command",
            json!({"cmd": "cargo test --all-targets --no-fail-fast --workspace"}),
        );
        shell.result = Some(json!({"output": "", "exit_code": 0}));
        let stdin = tool(
            "write_stdin",
            json!({"chars": "send a long interaction to the running process"}),
        );

        let shell_line = render(&shell, 36, &Theme::default()).remove(0);
        let live_shell_line =
            render_live(&shell, 2_500_000_000, 36, &Theme::default(), false).remove(0);
        let stdin_line = render(&stdin, 32, &Theme::default()).remove(0);

        assert_eq!(shell_line.width(), 36);
        assert!(shell_line.to_string().ends_with(" … · exit 0 · 1.2s"));
        assert!(live_shell_line.to_string().ends_with(" … · exit 0 · 2.5s"));
        assert_eq!(stdin_line.width(), 32);
        assert!(stdin_line.to_string().ends_with(" … · 1.2s"));
    }

    #[test]
    fn shell_summary_truncates_at_the_first_explicit_newline() {
        let shell = tool("exec_command", json!({"cmd": "printf one\nprintf two"}));

        let lines = render(&shell, 80, &Theme::default());

        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].to_string(), "  ▶ ✓ Shell  $ printf one … · 1.2s");
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
                "Files · read  /tmp/file",
            ),
            (
                "spawn_agent",
                json!({"role": "reviewer"}),
                "Spawn agent  1 arguments",
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

        let source = render_layout(&web, None, 80, &Theme::default(), true)
            .selection_source
            .expect("expanded web results should be selectable");
        assert_eq!(source, " Useful content\nSecond line");
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
