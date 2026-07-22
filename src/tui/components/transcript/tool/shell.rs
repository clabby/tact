use super::{Presentation, format_bytes};
use crate::tui::{format::shorten_home, theme::Theme, transcript::ToolEntry};
use ratatui::style::Style;
use serde_json::Value;
use std::path::Path;

pub(super) fn present(tool: &ToolEntry, width: u16, theme: &Theme, expanded: bool) -> Presentation {
    if tool.name == "write_stdin" {
        return stdin(tool, width, theme, expanded);
    }
    let command = tool
        .arguments
        .get("cmd")
        .and_then(Value::as_str)
        .unwrap_or("<command unavailable>");
    let mut presentation = Presentation::new("Shell", format!("$ {command}"));
    if let Some(outcome) = shell_outcome(tool.result.as_ref()) {
        presentation = presentation.outcome(outcome);
    }
    if !expanded {
        return presentation;
    }

    let mut details = Vec::new();
    if let Some(workdir) = tool.arguments.get("workdir").and_then(Value::as_str) {
        details.extend(super::super::markdown::wrap_plain(
            &format!("cwd {}", shorten_home(Path::new(workdir))),
            width,
            Style::default().fg(theme.muted()),
        ));
    }
    details.extend(super::super::markdown::wrap_plain(
        &format!("$ {command}"),
        width,
        Style::default()
            .fg(theme.code_text())
            .bg(theme.code_background()),
    ));
    for substep in &tool.substeps {
        details.extend(super::super::markdown::wrap_plain(
            &format!("↳ {substep}"),
            width,
            Style::default().fg(theme.muted()),
        ));
    }
    let output = tool
        .result
        .as_ref()
        .and_then(|result| result.get("output"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !output.is_empty() {
        details.extend(super::super::markdown::wrap_plain(
            output,
            width,
            Style::default().fg(theme.text()),
        ));
    }
    let line_count = output.lines().count();
    let line_label = if line_count == 1 { "line" } else { "lines" };
    presentation.details(details).footer(format!(
        "{line_count} {line_label} · {}",
        format_bytes(output.len())
    ))
}

fn stdin(tool: &ToolEntry, width: u16, theme: &Theme, expanded: bool) -> Presentation {
    let subject = tool
        .arguments
        .get("chars")
        .and_then(Value::as_str)
        .filter(|chars| !chars.is_empty())
        .map_or_else(
            || "poll process".to_owned(),
            |chars| format!("send {chars:?}"),
        );
    let presentation = Presentation::new("Shell input", subject);
    if !expanded {
        return presentation;
    }
    let details = tool.result.as_ref().map_or_else(Vec::new, |result| {
        super::render_result(result, width, theme)
    });
    presentation.details(details).footer("process interaction")
}

fn shell_outcome(result: Option<&Value>) -> Option<String> {
    let result = result?;
    if let Some(code) = result.get("exit_code").and_then(Value::as_i64) {
        return Some(format!("exit {code}"));
    }
    if let Some(error) = result.get("error").and_then(Value::as_str) {
        return Some(error.lines().next().unwrap_or(error).to_owned());
    }
    if let Some(id) = result.get("session_id").and_then(Value::as_i64) {
        return Some(format!("session {id} running"));
    }
    Some("terminated".to_owned())
}
