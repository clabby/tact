use super::{super::markdown::wrap_plain, Presentation};
use crate::{app::theme::Theme, core::transcript::ToolEntry};
use ratatui::style::Style;
use serde_json::Value;
use std::borrow::Cow;

pub(super) fn present(tool: &ToolEntry, width: u16, theme: &Theme, expanded: bool) -> Presentation {
    let recipient = tool
        .arguments
        .get("agent_id")
        .and_then(Value::as_u64)
        .map_or_else(|| "unknown agent".to_owned(), |id| format!("→ #{id}"));
    let purpose = tool
        .arguments
        .get("purpose")
        .and_then(Value::as_str)
        .unwrap_or("coordinate");
    let priority = tool
        .arguments
        .get("priority")
        .and_then(Value::as_str)
        .unwrap_or("deferred");
    let receipt = tool.result.as_ref().and_then(decoded_result);
    let disposition = receipt
        .as_deref()
        .and_then(|receipt| receipt.get("disposition"))
        .and_then(Value::as_str);
    let outcome = [
        Some(purpose),
        (priority != "deferred").then_some(priority),
        disposition,
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" · ");
    let mut presentation = Presentation::new("Message", recipient).outcome(outcome);
    if !expanded {
        return presentation;
    }

    let body = tool
        .arguments
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut details = wrap_plain(body, width, Style::default().fg(theme.text()));
    let footer = receipt.as_deref().map_or_else(
        || "message body".to_owned(),
        |receipt| {
            let message = receipt
                .get("message_id")
                .and_then(Value::as_u64)
                .map_or_else(|| "message ?".to_owned(), |id| format!("message #{id}"));
            let thread = receipt
                .get("thread_id")
                .and_then(Value::as_u64)
                .map_or_else(|| "thread ?".to_owned(), |id| format!("thread #{id}"));
            format!("{message} · {thread}")
        },
    );
    if let Some(disposition) = disposition {
        details.extend(wrap_plain(
            &format!("Delivery: {disposition}"),
            width,
            Style::default().fg(theme.muted()),
        ));
    }
    presentation = presentation.unselectable_details(details).footer(footer);
    presentation
}

fn decoded_result(value: &Value) -> Option<Cow<'_, Value>> {
    if let Some(encoded) = value.as_str() {
        return serde_json::from_str(encoded).ok().map(Cow::Owned);
    }
    Some(Cow::Borrowed(value))
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::{collapsed, entry, expanded, rendered};
    use crate::core::transcript::ToolEntry;
    use serde_json::json;

    #[test]
    fn presents_recipient_intent_and_body() {
        let tool = ToolEntry {
            result: Some(json!({
                "message_id": 9,
                "thread_id": 4,
                "disposition": "steered"
            })),
            ..entry(
                "send_agent_message",
                json!({
                    "agent_id": 7,
                    "message": "Please verify the ordering.",
                    "priority": "urgent",
                    "purpose": "question"
                }),
            )
        };

        let collapsed = collapsed(&tool);
        let expanded = expanded(&tool, 80);

        assert_eq!(collapsed.title, "Message");
        assert_eq!(
            collapsed.outcome.as_deref(),
            Some("question · urgent · steered")
        );
        assert_eq!(
            rendered(&expanded.details),
            ["Please verify the ordering.", "Delivery: steered"]
        );
        assert_eq!(expanded.footer.as_deref(), Some("message #9 · thread #4"));
    }
}
