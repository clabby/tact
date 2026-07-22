use super::{Presentation, format_bytes};
use crate::tui::{theme::Theme, transcript::ToolEntry};
use ratatui::style::Style;
use serde_json::Value;

pub(super) fn present(tool: &ToolEntry, width: u16, theme: &Theme, expanded: bool) -> Presentation {
    if tool.name == "wait" {
        return wait(tool, width, theme, expanded);
    }
    let source = tool.arguments.as_str().unwrap_or_else(|| {
        tool.arguments
            .get("input")
            .and_then(Value::as_str)
            .unwrap_or("<source unavailable>")
    });
    let emitted = tool
        .result
        .as_ref()
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let subject = if emitted == 1 {
        "1 emitted item".to_owned()
    } else {
        format!("{emitted} emitted items")
    };
    let presentation = Presentation::new("Code", subject);
    if !expanded {
        return presentation;
    }
    let mut details =
        super::super::markdown::render(&format!("```javascript\n{source}\n```"), width, theme)
            .lines;
    if let Some(result) = &tool.result {
        if let Some(items) = result.as_array() {
            for item in items {
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    details.extend(super::super::markdown::wrap_plain(
                        text,
                        width,
                        Style::default().fg(theme.text()),
                    ));
                }
            }
        } else {
            details.extend(super::render_result(result, width, theme));
        }
    }
    let size = tool
        .result
        .as_ref()
        .map_or(0, |result| result.to_string().len());
    presentation
        .details(details)
        .footer(format!("{emitted} outputs · {}", format_bytes(size)))
}

fn wait(tool: &ToolEntry, width: u16, theme: &Theme, expanded: bool) -> Presentation {
    let presentation = Presentation::new("Wait", "background work");
    if !expanded {
        return presentation;
    }
    let mut details = super::pretty_value(&tool.arguments, width, theme);
    if let Some(result) = &tool.result {
        details.extend(super::render_result(result, width, theme));
    }
    presentation.details(details).footer("wait diagnostics")
}
