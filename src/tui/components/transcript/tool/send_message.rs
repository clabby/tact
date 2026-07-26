use super::{super::markdown::wrap_plain, Presentation};
use crate::tui::{theme::Theme, transcript::ToolEntry};
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
    presentation = presentation.details(details).footer(footer);
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
    use super::present;
    use crate::tui::{
        theme::Theme,
        transcript::{ToolEntry, ToolState},
    };
    use serde_json::json;

    #[test]
    fn presents_recipient_intent_and_body() {
        let tool = ToolEntry {
            name: "send_agent_message".to_owned(),
            arguments: json!({
                "agent_id": 7,
                "message": "Please verify the ordering.",
                "priority": "urgent",
                "purpose": "question"
            }),
            started_at_unix_ms: 0,
            state: ToolState::Succeeded,
            duration_ns: None,
            result: Some(json!({
                "message_id": 9,
                "thread_id": 4,
                "disposition": "steered"
            })),
            metadata: None,
            substeps: Vec::new(),
        };

        let collapsed = present(&tool, 80, &Theme::default(), false);
        let expanded = present(&tool, 80, &Theme::default(), true);

        assert_eq!(collapsed.title, "Message");
        assert_eq!(
            collapsed.outcome.as_deref(),
            Some("question · urgent · steered")
        );
        assert!(expanded.details[0].to_string().contains("Please verify"));
        assert!(
            expanded
                .details
                .last()
                .unwrap()
                .to_string()
                .contains("steered")
        );
        assert_eq!(expanded.footer.as_deref(), Some("message #9 · thread #4"));
    }
}
