use super::{
    Component, ComposerChromeTarget, ConfirmationAction, DraftReset, InputMode, Overlay,
    RenderRequest, RootEffect, RootEvent, RootNode, SessionListKind, SubagentOverlay, ThreadState,
    TranscriptEvent,
};
use crate::{
    app::{
        config::{ReasoningEffort, ReasoningMode, Speed, TuiConfig},
        theme::{Theme, ThemeMode},
    },
    core::{
        extensions::Skill,
        session::{RecentPrompt, SessionSummary},
        transcript::{CompactionFinished, LocalEvent, TranscriptRecord, TurnId, UserSubmitted},
    },
};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use nanocodex::{
    ClaudeModel, HarnessModel as Model, Model as CodexModel,
    ReasoningMode as NanocodexReasoningMode, Thinking,
    agent::{
        events::{AgentEvent, AgentEventKind},
        input::{PromptInput, UserInput},
    },
};
use ratatui::{
    Terminal,
    backend::TestBackend,
    layout::Position,
    style::{Color, Modifier},
};
use semver::Version;
use serde_json::{Value, json, value::to_raw_value};
use std::{
    fs,
    num::NonZeroU16,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use tact_memory::{MemoryAccess, MemoryKey, MemoryRecord, MemorySource};
use tact_subagents::{AgentDescriptor, AgentId, AgentMessageUpdate, AgentStatus, AgentUpdate};

fn key(code: KeyCode, modifiers: KeyModifiers) -> super::RootEvent {
    super::RootEvent::Terminal(Event::Key(KeyEvent::new(code, modifiers)))
}

fn memory_record(id: i64, version: u64, content: &str) -> MemoryRecord {
    MemoryRecord {
        key: MemoryKey::local(id, version),
        content: content.to_owned(),
        created_at_ms: 0,
        updated_at_ms: 0,
        last_scanned_at_ms: None,
        scan_count: 0,
        last_used_at_ms: None,
        use_count: 0,
        probation_until_ms: None,
    }
}

fn key_with_kind(code: KeyCode, modifiers: KeyModifiers, kind: KeyEventKind) -> super::RootEvent {
    let mut key = KeyEvent::new(code, modifiers);
    key.kind = kind;
    super::RootEvent::Terminal(Event::Key(key))
}

fn mouse(kind: MouseEventKind, column: u16, row: u16) -> super::RootEvent {
    super::RootEvent::Terminal(Event::Mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }))
}

fn text_column(buffer: &ratatui::buffer::Buffer, row: u16, text: &str) -> u16 {
    let symbols = text
        .chars()
        .map(|character| character.to_string())
        .collect::<Vec<_>>();
    let width = u16::try_from(symbols.len()).unwrap();
    (0..=buffer.area.width.saturating_sub(width))
        .find(|&column| {
            symbols.iter().enumerate().all(|(offset, symbol)| {
                buffer[(column + u16::try_from(offset).unwrap(), row)].symbol() == symbol
            })
        })
        .expect("rendered text should be present")
}

fn subagent(id: u64, task: &str) -> AgentDescriptor {
    AgentDescriptor {
        id: AgentId::new(id),
        session_id: format!("agent-{id}"),
        model: Model::Codex(CodexModel::Sol),
        thinking: Thinking::Medium,
        reasoning_mode: NanocodexReasoningMode::Standard,
        role: "worker".to_owned(),
        task: task.to_owned(),
        parent: None,
    }
}

/// A coordination message from `from` to agent `to` that has been delivered.
fn delivered_message(from: Value, to: u64, body: &str) -> AgentMessageUpdate {
    serde_json::from_value(json!({
        "message_id": 1,
        "thread": {
            "id": 1,
            "participants": [from, {"kind": "agent", "agent_id": to}],
            "messages": [{
                "id": 1,
                "thread_id": 1,
                "from": from,
                "to": to,
                "priority": "deferred",
                "purpose": "coordinate",
                "body": body
            }]
        },
        "delivery": {"state": "delivered", "disposition": "started"}
    }))
    .unwrap()
}

fn pending_confirmation(root: &RootNode) -> Option<ConfirmationAction> {
    root.key_confirmation
        .as_ref()
        .map(|confirmation| confirmation.action)
}

fn render_root_text(root: &mut RootNode, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();
    terminal
        .backend()
        .buffer()
        .content
        .chunks(usize::from(width))
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn run_steered() -> super::RootEvent {
    super::RootEvent::Transcript(Arc::new(TranscriptRecord::from_agent(
        1,
        1,
        AgentEvent {
            protocol_version: 1,
            request_id: Arc::from("test"),
            seq: 1,
            kind: AgentEventKind::RunSteered,
            payload: to_raw_value(&json!({
                "steer_index": 1,
                "instruction_bytes": 5,
            }))
            .unwrap()
            .into(),
        },
    )))
}

fn agent_record(
    sequence: u64,
    kind: AgentEventKind,
    payload: serde_json::Value,
) -> Arc<TranscriptRecord> {
    Arc::new(TranscriptRecord::from_agent(
        sequence,
        sequence,
        AgentEvent {
            protocol_version: 1,
            request_id: Arc::from("opaque-test-id"),
            seq: sequence,
            kind,
            payload: to_raw_value(&payload).unwrap().into(),
        },
    ))
}

#[test]
fn tui_config_survives_fork_reset_and_session_restore() {
    let workspace = Path::new("/work");
    let mut root = RootNode::new(workspace, ReasoningEffort::Medium);
    let tui = TuiConfig {
        mouse_scroll_lines: NonZeroU16::new(1).unwrap(),
    };
    root.set_tui_config(tui);
    let fork = root.fork(workspace, ReasoningEffort::Medium);
    assert_eq!(fork.tui.mouse_scroll_lines.get(), 1);
    root.reset_session(
        workspace,
        ReasoningEffort::Medium,
        ReasoningMode::Standard,
        ReasoningMode::Standard,
        DraftReset::Clear,
    );
    assert_eq!(root.tui.mouse_scroll_lines.get(), 1);
    let projection = RootNode::project_session(ReasoningEffort::Medium, Vec::new());
    root.install_session_projection(
        workspace,
        ReasoningEffort::Medium,
        ReasoningMode::Standard,
        ReasoningMode::Standard,
        Speed::Standard,
        projection,
    );
    assert_eq!(root.tui.mouse_scroll_lines.get(), 1);
}

#[test]
fn native_context_budget_updates_live_and_restored_meter() {
    let record: TranscriptRecord = serde_json::from_value(json!({
        "schema_version": 2, "sequence": 1, "recorded_at_unix_ms": 1,
        "source": "tact", "type": "context.budget",
        "payload": {"active_tokens": 125_000, "window_tokens": 1_000_000}
    }))
    .unwrap();
    let record = Arc::new(record);
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.set_model(Model::Claude(nanocodex::ClaudeModel::Opus55));
    root.update(super::RootEvent::ContextBudget(
        crate::core::context::ContextBudget {
            active_tokens: 0,
            window_tokens: 900_000,
        },
    ));
    assert_eq!(
        root.composer().context_budget(),
        crate::core::context::ContextBudget {
            active_tokens: 0,
            window_tokens: 900_000,
        }
    );
    assert!(root.context_diagnostics.auto_compact_token_limit.is_none());
    root.update(super::RootEvent::Transcript(record.clone()));
    assert_eq!(root.context_diagnostics.active_tokens, Some(125_000));
    assert!(root.context_diagnostics.usage.is_none());
    assert_eq!(
        root.composer().context_budget(),
        crate::core::context::ContextBudget {
            active_tokens: 125_000,
            window_tokens: 1_000_000,
        }
    );
    root.restore_session(
        Path::new("/work"),
        ReasoningEffort::Medium,
        ReasoningMode::Standard,
        ReasoningMode::Standard,
        Speed::Standard,
        vec![record],
    );
    root.set_model(Model::Claude(nanocodex::ClaudeModel::Opus55));
    assert_eq!(
        root.composer().context_budget(),
        crate::core::context::ContextBudget {
            active_tokens: 125_000,
            window_tokens: 1_000_000,
        }
    );
}

#[test]
fn context_diagnostics_tracks_restored_and_live_records() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    let outbound = agent_record(
        1,
        AgentEventKind::ApiEvent,
        json!({
            "direction": "outbound",
            "phase": "generation",
            "event": {
                "previous_response_id": "opaque-response-id",
                "prompt_cache_key": "opaque-cache-key"
            }
        }),
    );
    root.restore_session(
        Path::new("/work"),
        ReasoningEffort::Medium,
        ReasoningMode::Standard,
        ReasoningMode::Standard,
        Speed::Standard,
        vec![outbound],
    );
    assert_eq!(
        root.context_diagnostics.continuation,
        Some(crate::core::context::ContinuationMode::PreviousResponse)
    );

    let completed = agent_record(
        2,
        AgentEventKind::ModelCallCompleted,
        json!({
            "call_index": 1,
            "model": "gpt-6.1-sol",
            "attempt": 1,
            "connection_generation": 1,
            "status": "completed",
            "duration_ns": 1,
            "time_to_first_event_ns": 1,
            "time_to_first_output_ns": 1,
            "tool_calls": 0,
            "usage": {
                "input_tokens": 1_000,
                "input_tokens_details": {"cached_tokens": 800},
                "output_tokens": 50,
                "total_tokens": 1_050
            }
        }),
    );
    root.update(super::RootEvent::Transcript(completed));
    assert_eq!(root.context_diagnostics.usage.unwrap().cached_input, 800);

    root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));
    for character in "debug context".chars() {
        root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
    }
    root.update(key(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(root.overlay, Some(Overlay::ContextDiagnostics(_))));

    root.update(key(KeyCode::Char('r'), KeyModifiers::NONE));
    assert_eq!(root.context_diagnostics.usage.unwrap().total, 1_050);
    root.update(key(KeyCode::Esc, KeyModifiers::NONE));
    assert!(root.overlay.is_none());
}

#[test]
fn restored_session_does_not_keep_historical_activity() {
    let mut projection = RootNode::project_session(
        ReasoningEffort::Medium,
        vec![
            agent_record(1, AgentEventKind::RunStarted, json!({})),
            agent_record(2, AgentEventKind::RunStarted, json!({})),
            agent_record(
                3,
                AgentEventKind::ToolCall,
                json!({
                    "call_id": "orphaned-shell",
                    "tool": "exec_command",
                    "arguments": {"cmd": "sleep 100"},
                }),
            ),
        ],
    );

    let restored = projection
        .transcript
        .update(TranscriptEvent::AgentStreamClosed);
    assert!(restored.effects.is_empty());

    let started = projection
        .transcript
        .update(TranscriptEvent::Record(agent_record(
            4,
            AgentEventKind::RunStarted,
            json!({}),
        )));
    assert_eq!(started.effects.len(), 1);
    assert!(started.effects[0].active);
    assert_eq!(started.effects[0].status.as_deref(), Some("Thinking…"));

    let completed = projection
        .transcript
        .update(TranscriptEvent::Record(agent_record(
            5,
            AgentEventKind::RunCompleted,
            json!({"duration_ns": 1_000_000}),
        )));
    assert_eq!(completed.effects.len(), 1);
    assert!(!completed.effects[0].active);
    assert!(completed.effects[0].status.is_none());
}

#[test]
fn composer_is_anchored_to_the_bottom() {
    let backend = TestBackend::new(40, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);

    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();

    let buffer = terminal.backend().buffer();
    assert_eq!(buffer[(0, 7)].symbol(), "╭");
    assert_eq!(buffer[(0, 11)].symbol(), "╰");
    assert_eq!(buffer[(0, 6)].symbol(), " ");
}

#[test]
fn clicking_composer_chrome_opens_model_effort_speed_and_subagents() {
    let mut terminal = Terminal::new(TestBackend::new(100, 16)).unwrap();
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();
    let top = root.composer_area.y;
    let model_x = text_column(terminal.backend().buffer(), top, "gpt-6.1-sol");
    let effort_x = text_column(terminal.backend().buffer(), top, "medium");
    let speed_x = text_column(terminal.backend().buffer(), top, "󰳗");
    assert_eq!(
        root.composer.chrome_target(Position::new(model_x, top)),
        Some(ComposerChromeTarget::Model)
    );
    assert_eq!(
        root.composer.chrome_target(Position::new(effort_x, top)),
        Some(ComposerChromeTarget::Effort)
    );

    root.update(mouse(MouseEventKind::Down(MouseButton::Left), model_x, top));
    assert!(matches!(root.overlay, Some(Overlay::Model(_))));

    root.overlay = None;
    root.update(mouse(
        MouseEventKind::Down(MouseButton::Left),
        effort_x,
        top,
    ));
    assert!(matches!(root.overlay, Some(Overlay::Effort(_))));

    root.overlay = None;
    assert_eq!(
        root.composer().chrome_target(Position::new(speed_x, top)),
        Some(ComposerChromeTarget::Speed)
    );
    root.update(mouse(MouseEventKind::Down(MouseButton::Left), speed_x, top));
    assert!(matches!(root.overlay, Some(Overlay::Speed(_))));

    root.overlay = None;
    root.update(super::RootEvent::Subagent(AgentUpdate::Added(subagent(
        1, "work",
    ))));
    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();
    let subagents_x = text_column(terminal.backend().buffer(), top, "1 subagents");

    root.update(mouse(
        MouseEventKind::Down(MouseButton::Left),
        subagents_x,
        top,
    ));
    assert!(matches!(
        root.overlay,
        Some(Overlay::Subagents(SubagentOverlay::Tree))
    ));
}

#[test]
fn root_messages_render_once_in_main_and_are_projected_into_child_transcripts() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.update(RootEvent::Subagent(AgentUpdate::Added(subagent(
        1,
        "verify ordering",
    ))));
    root.update(RootEvent::Transcript(Arc::new(
        TranscriptRecord::from_agent(
            1,
            1,
            AgentEvent {
                protocol_version: 1,
                request_id: Arc::from("test"),
                seq: 1,
                kind: AgentEventKind::ToolCall,
                payload: to_raw_value(&json!({
                    "call_id": "message-1",
                    "tool": "send_agent_message",
                    "arguments": {
                        "agent_id": 1,
                        "message": "Please verify the ordering.",
                        "priority": "deferred",
                        "purpose": "coordinate"
                    }
                }))
                .unwrap()
                .into(),
            },
        ),
    )));
    let message = delivered_message(json!({"kind": "root"}), 1, "Please verify the ordering.");
    root.update(RootEvent::Subagent(AgentUpdate::Message(message)));

    let main = render_root_text(&mut root, 100, 20);

    let mut child = Terminal::new(TestBackend::new(100, 40)).unwrap();
    child
        .draw(|frame| {
            root.subagents.render_transcript(
                AgentId::new(1),
                frame,
                frame.area(),
                &Theme::default(),
            );
        })
        .unwrap();
    let child = child
        .backend()
        .buffer()
        .content
        .chunks(100)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");

    assert_eq!(main.matches("Message").count(), 1);
    assert!(child.contains("← Message  root → you"));
    assert!(child.contains("Please verify"));
    assert!(child.contains("ordering."));
}

#[test]
fn peer_messages_are_projected_into_the_main_transcript() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    for (id, role) in [(1, "sender"), (2, "recipient")] {
        root.update(RootEvent::Subagent(AgentUpdate::Added(AgentDescriptor {
            role: role.to_owned(),
            ..subagent(id, "coordinate with a peer")
        })));
    }
    let message = delivered_message(
        json!({"kind": "agent", "agent_id": 1}),
        2,
        "Peer coordination is visible.",
    );

    root.update(RootEvent::Subagent(AgentUpdate::Message(message)));

    let main = render_root_text(&mut root, 100, 20);
    assert!(main.contains("← Message  #1 → #2"));
    assert!(main.contains("Peer coordination is visible."));
}

#[test]
fn composer_hides_subagents_after_they_stop_running() {
    let mut terminal = Terminal::new(TestBackend::new(100, 16)).unwrap();
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.update(super::RootEvent::Subagent(AgentUpdate::Added(subagent(
        1, "work",
    ))));
    root.update(super::RootEvent::Subagent(AgentUpdate::Status {
        id: AgentId::new(1),
        status: AgentStatus::Completed {
            output: json!({ "report": "done" }),
        },
    }));

    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();
    let rendered = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    assert!(!rendered.contains("subagents"));
}

#[test]
fn completed_direct_subagent_starts_a_continuation_when_idle() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.update(super::RootEvent::Subagent(AgentUpdate::Added(subagent(
        1,
        "inspect the queue",
    ))));

    let update = root.update(super::RootEvent::Subagent(AgentUpdate::Status {
        id: AgentId::new(1),
        status: AgentStatus::Completed {
            output: json!({ "report": "queue is sound" }),
        },
    }));

    assert!(matches!(
        update.effects.as_slice(),
        [RootEffect::ContinueSubagent(prompt)]
            if prompt.display_text().contains("list_agents")
                && prompt.display_text().contains("agent_id=\"1\"")
                && !prompt.display_text().contains("queue is sound")
    ));
    assert_eq!(root.busy().turns, 1);
}

#[test]
fn completed_subagent_does_not_start_a_competing_active_turn() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.turns.start_turn();
    root.update(super::RootEvent::Subagent(AgentUpdate::Added(subagent(
        1,
        "inspect the queue",
    ))));

    let update = root.update(super::RootEvent::Subagent(AgentUpdate::Status {
        id: AgentId::new(1),
        status: AgentStatus::Completed {
            output: json!({ "report": "queue is sound" }),
        },
    }));

    assert!(update.effects.is_empty());
    assert_eq!(root.busy().turns, 1);
}

#[test]
fn completed_nested_subagent_does_not_bypass_its_parent() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.update(super::RootEvent::Subagent(AgentUpdate::Added(
        AgentDescriptor {
            parent: Some(AgentId::new(1)),
            ..subagent(2, "inspect the queue")
        },
    )));

    let update = root.update(super::RootEvent::Subagent(AgentUpdate::Status {
        id: AgentId::new(2),
        status: AgentStatus::Completed {
            output: json!({ "report": "queue is sound" }),
        },
    }));

    assert!(update.effects.is_empty());
    assert_eq!(root.busy().turns, 0);
}

#[test]
fn transcript_uses_the_space_above_the_composer() {
    let backend = TestBackend::new(40, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    let record = TranscriptRecord::from_local(
        1,
        1,
        LocalEvent::UserSubmitted(UserSubmitted {
            id: TurnId::new(1),
            text: "hello transcript".to_owned(),
        }),
    )
    .unwrap();
    root.update(super::RootEvent::Transcript(Arc::new(record)));

    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();

    let buffer = terminal.backend().buffer();
    assert!((0..7).any(|y| buffer[(0, y)].symbol() == "┃"));
    assert_eq!(buffer[(0, 7)].symbol(), "╭");
}

#[test]
fn clicking_a_pinned_prompt_reveals_its_transcript_entry() {
    let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    let prompt = TranscriptRecord::from_local(
        1,
        1,
        LocalEvent::UserSubmitted(UserSubmitted {
            id: TurnId::new(1),
            text: "jump to this prompt".to_owned(),
        }),
    )
    .unwrap();
    root.update(super::RootEvent::Transcript(Arc::new(prompt)));
    root.update(super::RootEvent::Transcript(agent_record(
        2,
        AgentEventKind::AssistantMessage,
        json!({
            "model_call_index": 1,
            "item_id": "answer",
            "phase": "final_answer",
            "text": (1..=40)
                .map(|line| format!("answer {line}"))
                .collect::<Vec<_>>()
                .join("\n"),
        }),
    )));
    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();
    root.update(mouse(MouseEventKind::ScrollUp, 5, 1));
    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();
    assert_eq!(
        terminal.backend().buffer()[(5, 0)].bg,
        Theme::default().code_background()
    );

    root.update(mouse(MouseEventKind::Down(MouseButton::Left), 5, 0));
    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();

    let first_row = (0..40)
        .map(|column| terminal.backend().buffer()[(column, 0)].symbol())
        .collect::<String>();
    assert!(first_row.contains("jump to this prompt"));
    assert_ne!(
        terminal.backend().buffer()[(5, 0)].bg,
        Theme::default().code_background()
    );
}

#[test]
fn clicking_the_updates_banner_returns_to_the_transcript_tail() {
    let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    for sequence in 1..=20 {
        let record = TranscriptRecord::from_local(
            sequence,
            sequence,
            LocalEvent::UserSubmitted(UserSubmitted {
                id: TurnId::new(sequence),
                text: format!("prompt {sequence}"),
            }),
        )
        .unwrap();
        root.update(super::RootEvent::Transcript(Arc::new(record)));
    }
    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();
    root.update(key(KeyCode::PageUp, KeyModifiers::NONE));
    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();
    let latest = TranscriptRecord::from_local(
        21,
        21,
        LocalEvent::UserSubmitted(UserSubmitted {
            id: TurnId::new(21),
            text: "latest prompt".to_owned(),
        }),
    )
    .unwrap();
    root.update(super::RootEvent::Transcript(Arc::new(latest)));
    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();
    let banner_column = text_column(terminal.backend().buffer(), 0, "1 update");

    root.update(mouse(
        MouseEventKind::Down(MouseButton::Left),
        banner_column,
        0,
    ));
    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    assert!(rendered.contains("latest prompt"));
    assert!(!rendered.contains("1 update"));
}

#[test]
fn clicking_a_transcript_link_requests_that_it_be_opened() {
    let mut terminal = Terminal::new(TestBackend::new(50, 12)).unwrap();
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    let record = TranscriptRecord::from_agent(
        1,
        1,
        AgentEvent {
            protocol_version: 1,
            request_id: Arc::from("test"),
            seq: 1,
            kind: AgentEventKind::AssistantMessage,
            payload: to_raw_value(&json!({
                "model_call_index": 1,
                "item_id": "answer",
                "phase": "final_answer",
                "text": "Open [the site](https://example.com).",
            }))
            .unwrap()
            .into(),
        },
    );
    root.update(super::RootEvent::Transcript(Arc::new(record)));
    root.queue.push("queued".to_owned());
    root.queue.set_focused(true);
    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let (column, row) = (0..buffer.area.height)
        .find_map(|row| {
            let rendered = (0..buffer.area.width)
                .map(|column| buffer[(column, row)].symbol())
                .collect::<String>();
            rendered
                .find("the site")
                .map(|column| (u16::try_from(column).unwrap(), row))
        })
        .expect("link label should be rendered");

    let down = root.update(mouse(MouseEventKind::Down(MouseButton::Left), column, row));
    assert!(down.effects.is_empty());
    let up = root.update(mouse(MouseEventKind::Up(MouseButton::Left), column, row));

    assert_eq!(
        up.effects,
        [RootEffect::OpenLink("https://example.com".to_owned())]
    );
    assert!(!root.queue.focused());
}

fn copy_test_root() -> RootNode {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    for (sequence, text) in [
        (1, "first **reply**"),
        (2, "second\n```rust\ncode\n```"),
        (3, "  "),
    ] {
        root.update(super::RootEvent::Transcript(agent_record(
            sequence,
            AgentEventKind::AssistantMessage,
            json!({"model_call_index": sequence, "item_id": format!("answer-{sequence}"), "text": text}),
        )));
    }
    root.update(super::RootEvent::Transcript(agent_record(
        4,
        AgentEventKind::AssistantDelta,
        json!({"model_call_index": 4, "item_id": "streaming", "text": "partial"}),
    )));
    root
}

#[test]
fn slash_copy_selects_completed_messages_without_submitting_a_turn() {
    for (command, expected) in [
        ("/copy", "second\n```rust\ncode\n```"),
        ("/copy 1", "second\n```rust\ncode\n```"),
        ("/copy 2", "first **reply**"),
        ("/COPY 2", "first **reply**"),
    ] {
        for paste in [false, true] {
            let mut root = copy_test_root();
            root.turns.start_turn();
            if paste {
                root.update(super::RootEvent::Terminal(Event::Paste(command.to_owned())));
            } else {
                for character in command.chars() {
                    root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
                }
            }
            let update = root.update(key(KeyCode::Enter, KeyModifiers::NONE));
            assert_eq!(
                update.effects,
                [RootEffect::Copy(expected.to_owned())],
                "{command}, paste={paste}"
            );
            assert_eq!(root.busy().turns, 1);
            assert!(!root.queue.has_pending_steer());
        }
    }
}

#[test]
fn slash_copy_reports_invalid_or_unavailable_positions_locally() {
    for command in [
        "/copy 0",
        "/copy -1",
        "/copy nope",
        "/copy 1 2",
        "/copy 999999999999999999999999999999",
        "/copy 4",
    ] {
        let mut root = copy_test_root();
        root.update(super::RootEvent::Terminal(Event::Paste(command.to_owned())));
        let update = root.update(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(update.effects.is_empty(), "{command}");
        assert!(root.notification.is_some(), "{command}");
        assert_eq!(root.busy().turns, 0);
    }
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    let update = root.copy_response("");
    assert!(update.effects.is_empty());
    assert!(root.notification.is_some());
}

#[test]
fn slash_copy_prefix_in_a_prompt_is_not_a_command() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.update(super::RootEvent::Terminal(Event::Paste(
        "/copyright".to_owned(),
    )));
    let update = root.update(key(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        update.effects,
        [RootEffect::Submit("/copyright".to_owned().into())]
    );
}

#[test]
fn submitting_a_prompt_returns_the_transcript_to_the_tail() {
    let backend = TestBackend::new(40, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    for sequence in 1..=20 {
        let record = TranscriptRecord::from_local(
            sequence,
            sequence,
            LocalEvent::UserSubmitted(UserSubmitted {
                id: TurnId::new(sequence),
                text: format!("old prompt {sequence}"),
            }),
        )
        .unwrap();
        root.update(super::RootEvent::Transcript(Arc::new(record)));
    }
    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();
    root.update(key(KeyCode::PageUp, KeyModifiers::NONE));
    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();

    for character in "new prompt".chars() {
        root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
    }
    let submitted = root.update(key(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        submitted.effects,
        [RootEffect::Submit("new prompt".to_owned().into())]
    );
    let record = TranscriptRecord::from_local(
        21,
        21,
        LocalEvent::UserSubmitted(UserSubmitted {
            id: TurnId::new(21),
            text: "new prompt".to_owned(),
        }),
    )
    .unwrap();
    root.update(super::RootEvent::Transcript(Arc::new(record)));

    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    assert!(rendered.contains("new prompt"));
}

#[test]
fn leading_slash_opens_actions_without_changing_the_draft() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);

    let update = root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));

    assert!(matches!(&root.overlay, Some(Overlay::Actions(_))));
    assert!(root.composer().draft().is_empty());
    assert_eq!(update.render, super::RenderRequest::Immediate);
}

#[test]
fn slash_after_prompt_text_remains_in_the_composer() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.update(key(KeyCode::Char('a'), KeyModifiers::NONE));

    root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));

    assert!(root.overlay.is_none());
    assert_eq!(root.composer().draft(), "a/");
}

#[test]
fn dollar_at_a_token_boundary_opens_skills_and_remains_in_the_draft() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.set_skills(
        vec![Skill::new(
            "autofix",
            "Review and repair a pull request until clean.",
        )]
        .into(),
    );
    for character in "use ".chars() {
        root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
    }

    let update = root.update(key(KeyCode::Char('$'), KeyModifiers::NONE));

    assert!(matches!(&root.overlay, Some(Overlay::Skills(_))));
    assert_eq!(root.composer().draft(), "use $");
    assert_eq!(update.render, RenderRequest::Immediate);
    let rendered = render_root_text(&mut root, 90, 20);
    assert!(rendered.contains("$autofix"));
    assert!(rendered.contains("Review and repair a pull request until clean."));
}

#[test]
fn dollar_is_literal_without_available_skills_or_inside_a_token() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.update(key(KeyCode::Char('$'), KeyModifiers::NONE));
    assert!(root.overlay.is_none());
    assert_eq!(root.composer().draft(), "$");

    root.composer.replace_draft(String::new());
    root.set_skills(vec![Skill::new("autofix", "Repair a pull request.")].into());
    for character in "price$5".chars() {
        root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
    }

    assert!(root.overlay.is_none());
    assert_eq!(root.composer().draft(), "price$5");
}

#[test]
fn dollar_is_literal_in_shell_mode() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.set_skills(vec![Skill::new("autofix", "Repair a pull request.")].into());

    for character in "!echo $PATH".chars() {
        root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
    }

    assert!(root.overlay.is_none());
    assert_eq!(root.composer().draft(), "!echo $PATH");

    let submitted = root.update(key(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        submitted.effects,
        [RootEffect::RunShell("echo $PATH".to_owned())]
    );
}

fn assert_skill_selection(key_code: KeyCode) {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.set_skills(
        vec![
            Skill::new("autofix", "Repair a pull request."),
            Skill::new("open-docs", "Open documentation."),
        ]
        .into(),
    );
    for character in "use later".chars() {
        root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
    }
    for _ in 0.."later".len() {
        root.update(key(KeyCode::Left, KeyModifiers::NONE));
    }
    for character in "$auto".chars() {
        root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
    }

    root.update(key(key_code, KeyModifiers::NONE));

    assert!(root.overlay.is_none());
    assert_eq!(root.composer().draft(), "use $autofix later");
}

#[test]
fn enter_selects_a_filtered_skill_at_the_composer_cursor() {
    assert_skill_selection(KeyCode::Enter);
}

#[test]
fn tab_selects_a_filtered_skill_at_the_composer_cursor() {
    assert_skill_selection(KeyCode::Tab);
}

#[test]
fn escape_preserves_a_literal_skill_query() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.set_skills(vec![Skill::new("autofix", "Repair a pull request.")].into());
    for character in "$auto".chars() {
        root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
    }

    root.update(key(KeyCode::Esc, KeyModifiers::NONE));

    assert!(root.overlay.is_none());
    assert_eq!(root.composer().draft(), "$auto");
}

#[test]
fn mouse_dismisses_mention_popovers_with_an_immediate_redraw() {
    let workspace = tempfile::tempdir().unwrap();
    let mut root = RootNode::new(workspace.path(), ReasoningEffort::Medium);
    root.update(key(KeyCode::Char('@'), KeyModifiers::NONE));

    let file_update = root.update(mouse(MouseEventKind::Moved, 0, 0));

    assert!(root.overlay.is_none());
    assert_eq!(file_update.render, RenderRequest::Immediate);

    root.set_skills(vec![Skill::new("autofix", "Repair a pull request.")].into());
    root.update(key(KeyCode::Char(' '), KeyModifiers::NONE));
    root.update(key(KeyCode::Char('$'), KeyModifiers::NONE));

    let skill_update = root.update(mouse(MouseEventKind::Moved, 0, 0));

    assert!(root.overlay.is_none());
    assert_eq!(skill_update.render, RenderRequest::Immediate);
}

#[test]
fn at_at_a_token_boundary_opens_the_file_finder_and_remains_in_the_draft() {
    let workspace = tempfile::tempdir().unwrap();
    let mut root = RootNode::new(workspace.path(), ReasoningEffort::Medium);
    for character in "inspect ".chars() {
        root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
    }

    let update = root.update(key(KeyCode::Char('@'), KeyModifiers::NONE));

    assert!(matches!(&root.overlay, Some(Overlay::FileFinder(_))));
    assert_eq!(root.composer().draft(), "inspect @");
    assert_eq!(update.render, super::RenderRequest::Immediate);
}

#[test]
fn releasing_at_keeps_the_file_finder_open() {
    let workspace = tempfile::tempdir().unwrap();
    let mut root = RootNode::new(workspace.path(), ReasoningEffort::Medium);
    root.update(key(KeyCode::Char('@'), KeyModifiers::NONE));

    let update = root.update(key_with_kind(
        KeyCode::Char('@'),
        KeyModifiers::NONE,
        KeyEventKind::Release,
    ));

    assert!(matches!(&root.overlay, Some(Overlay::FileFinder(_))));
    assert!(update.effects.is_empty());
    assert_eq!(update.render, super::RenderRequest::None);
}

#[test]
fn second_at_switches_from_files_to_session_mentions() {
    let workspace = tempfile::tempdir().unwrap();
    let mut root = RootNode::new(workspace.path(), ReasoningEffort::Medium);
    for character in "compare ".chars() {
        root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
    }
    root.update(key(KeyCode::Char('@'), KeyModifiers::NONE));

    let loading = root.update(key(KeyCode::Char('@'), KeyModifiers::NONE));

    assert_eq!(root.composer().draft(), "compare @@");
    assert!(root.overlay.is_none());
    assert_eq!(
        loading.effects,
        [RootEffect::LoadSessions(SessionListKind::Mention)]
    );

    root.update(RootEvent::SessionsLoaded(vec![SessionSummary {
        session_id: "session-123".to_owned(),
        started_at_unix_ms: 1,
        model: "model".to_owned(),
        effort: ReasoningEffort::Medium,
        reasoning_mode: ReasoningMode::Standard,
        workspace: workspace.path().to_path_buf(),
        preview: "earlier investigation".to_owned(),
    }]));
    assert!(matches!(&root.overlay, Some(Overlay::Sessions(_))));

    root.update(key(KeyCode::Enter, KeyModifiers::NONE));

    assert!(root.overlay.is_none());
    assert_eq!(root.composer().draft(), "compare @@session-123 ");
}

#[test]
fn later_at_closes_file_suggestions_without_opening_sessions() {
    let workspace = tempfile::tempdir().unwrap();
    let mut root = RootNode::new(workspace.path(), ReasoningEffort::Medium);
    for character in "@someone@".chars() {
        let update = root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
        assert!(update.effects.is_empty());
    }

    assert!(root.overlay.is_none());
    assert_eq!(root.composer().draft(), "@someone@");
}

#[test]
fn at_inside_a_token_is_inserted_without_opening_the_file_finder() {
    let workspace = tempfile::tempdir().unwrap();
    let mut root = RootNode::new(workspace.path(), ReasoningEffort::Medium);
    for character in "name@example.com".chars() {
        root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
    }

    assert!(root.overlay.is_none());
    assert_eq!(root.composer().draft(), "name@example.com");
}

fn assert_file_selection(key_code: KeyCode) {
    let workspace = tempfile::tempdir().unwrap();
    fs::write(workspace.path().join("notes.md"), "remember this").unwrap();
    let mut root = RootNode::new(workspace.path(), ReasoningEffort::Medium);
    for character in "inspect ".chars() {
        root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
    }
    root.update(key(KeyCode::Char('@'), KeyModifiers::NONE));
    for character in "notes".chars() {
        root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
    }

    root.update(key(key_code, KeyModifiers::NONE));

    assert!(root.overlay.is_none());
    assert_eq!(root.composer().draft(), "inspect @notes.md ");
}

#[test]
fn enter_selects_a_file_at_the_composer_cursor() {
    assert_file_selection(KeyCode::Enter);
}

#[test]
fn tab_selects_a_file_at_the_composer_cursor() {
    assert_file_selection(KeyCode::Tab);
}

#[test]
fn selecting_a_file_replaces_the_query_in_the_middle_of_a_draft() {
    let workspace = tempfile::tempdir().unwrap();
    fs::write(workspace.path().join("notes.md"), "remember this").unwrap();
    let mut root = RootNode::new(workspace.path(), ReasoningEffort::Medium);
    for character in "inspect later".chars() {
        root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
    }
    for _ in 0.."later".len() {
        root.update(key(KeyCode::Left, KeyModifiers::NONE));
    }
    for character in "@notes".chars() {
        root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
    }

    root.update(key(KeyCode::Enter, KeyModifiers::NONE));

    assert!(root.overlay.is_none());
    assert_eq!(root.composer().draft(), "inspect @notes.md later");
}

#[test]
fn escape_preserves_a_literal_mention_and_backspace_removes_it() {
    let workspace = tempfile::tempdir().unwrap();
    let mut root = RootNode::new(workspace.path(), ReasoningEffort::Medium);

    root.update(key(KeyCode::Char('@'), KeyModifiers::NONE));
    root.update(key(KeyCode::Esc, KeyModifiers::NONE));
    assert!(root.overlay.is_none());
    assert_eq!(root.composer().draft(), "@");

    root.update(key(KeyCode::Backspace, KeyModifiers::NONE));
    root.update(key(KeyCode::Char('@'), KeyModifiers::NONE));
    root.update(key(KeyCode::Backspace, KeyModifiers::NONE));
    assert!(root.overlay.is_none());
    assert!(root.composer().draft().is_empty());
}

#[test]
fn mention_query_is_composer_text_and_space_closes_suggestions() {
    let workspace = tempfile::tempdir().unwrap();
    let mut root = RootNode::new(workspace.path(), ReasoningEffort::Medium);

    for character in "@someone ".chars() {
        root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
    }

    assert!(root.overlay.is_none());
    assert_eq!(root.composer().draft(), "@someone ");
}

#[test]
fn escape_and_empty_backspace_close_actions_immediately() {
    for dismiss in [KeyCode::Esc, KeyCode::Backspace] {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));

        let update = root.update(key(dismiss, KeyModifiers::NONE));

        assert!(root.overlay.is_none());
        assert_eq!(update.render, super::RenderRequest::Immediate);
    }
}

#[test]
fn control_c_requires_confirmation_while_actions_are_open() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));

    let first = root.update(key(KeyCode::Char('c'), KeyModifiers::CONTROL));

    assert!(first.effects.is_empty());
    assert!(root.overlay.is_some());
    assert_eq!(pending_confirmation(&root), Some(ConfirmationAction::Exit));

    let second = root.update(key(KeyCode::Char('c'), KeyModifiers::CONTROL));

    assert_eq!(second.effects, [super::RootEffect::Shutdown]);
}

#[test]
fn escape_cancels_a_pending_exit() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.update(key(KeyCode::Char('c'), KeyModifiers::CONTROL));

    let cancel = root.update(key(KeyCode::Esc, KeyModifiers::NONE));
    let next_control_c = root.update(key(KeyCode::Char('c'), KeyModifiers::CONTROL));

    assert!(cancel.effects.is_empty());
    assert_eq!(cancel.render, super::RenderRequest::Immediate);
    assert!(next_control_c.effects.is_empty());
}

#[test]
fn control_c_requires_two_distinct_presses() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.update(key(KeyCode::Char('c'), KeyModifiers::CONTROL));

    let release = root.update(key_with_kind(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL,
        KeyEventKind::Release,
    ));
    let repeat = root.update(key_with_kind(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL,
        KeyEventKind::Repeat,
    ));

    assert!(release.effects.is_empty());
    assert!(repeat.effects.is_empty());
    assert!(root.key_confirmation.is_some());

    let second_press = root.update(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert_eq!(second_press.effects, [RootEffect::Shutdown]);
}

#[test]
fn confirmation_floats_above_the_composer_top_right() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.update(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
    let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();

    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();

    let buffer = terminal.backend().buffer();
    let popup_bottom = root.composer_area.y - 2;
    assert_eq!(
        buffer[(root.composer_area.right() - 28, popup_bottom)].symbol(),
        "╰"
    );
    assert_eq!(
        buffer[(root.composer_area.right() - 1, popup_bottom)].symbol(),
        "╯"
    );
    assert_eq!(
        buffer[(root.composer_area.right() - 1, root.composer_area.y)].symbol(),
        "╮"
    );
}

#[test]
fn control_c_clears_the_focused_composer_before_shutting_down() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.update(key(KeyCode::Char('h'), KeyModifiers::NONE));
    root.update(key(KeyCode::Char('i'), KeyModifiers::NONE));

    let clear = root.update(key(KeyCode::Char('c'), KeyModifiers::CONTROL));

    assert!(clear.effects.is_empty());
    assert_eq!(clear.render, super::RenderRequest::Immediate);
    assert!(root.composer().draft().is_empty());

    let confirmation = root.update(key(KeyCode::Char('c'), KeyModifiers::CONTROL));

    assert!(confirmation.effects.is_empty());
    assert_eq!(pending_confirmation(&root), Some(ConfirmationAction::Exit));

    let shutdown = root.update(key(KeyCode::Char('c'), KeyModifiers::CONTROL));

    assert_eq!(shutdown.effects, [RootEffect::Shutdown]);
}

#[test]
fn control_z_restores_the_last_cleared_draft() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.update(RootEvent::ReplaceDraft("first\nλright".to_owned()));
    for _ in 0..5 {
        root.update(key(KeyCode::Left, KeyModifiers::NONE));
    }

    root.update(key(KeyCode::Char('c'), KeyModifiers::CONTROL));

    assert!(root.composer().draft().is_empty());
    assert!(root.discarded_draft.is_some());

    let restored = root.update(key(KeyCode::Char('z'), KeyModifiers::CONTROL));
    root.update(key(KeyCode::Char('|'), KeyModifiers::NONE));

    assert_eq!(restored.render, super::RenderRequest::Immediate);
    assert_eq!(root.composer().draft(), "first\nλ|right");
    assert!(root.discarded_draft.is_none());
}

#[test]
fn restored_draft_keeps_pasted_images() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.update(RootEvent::ReplaceDraft("inspect ".to_owned()));
    root.update(RootEvent::PasteImage(
        "data:image/png;base64,restored".to_owned(),
    ));
    root.update(key(KeyCode::Char('c'), KeyModifiers::CONTROL));

    root.update(key(KeyCode::Char('z'), KeyModifiers::CONTROL));
    let submitted = root.update(key(KeyCode::Enter, KeyModifiers::NONE));

    let [RootEffect::Submit(prompt)] = submitted.effects.as_slice() else {
        panic!("restored draft should submit");
    };
    let PromptInput::Content(content) = prompt.agent_prompt().instruction else {
        panic!("restored image should produce multimodal input");
    };
    assert!(matches!(&content[0], UserInput::Text { text } if text == "inspect "));
    assert!(matches!(
        &content[1],
        UserInput::Image { image_url, .. } if image_url.ends_with("restored")
    ));
}

#[test]
fn control_z_does_not_overwrite_a_nonempty_draft() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.update(RootEvent::ReplaceDraft("recover me".to_owned()));
    root.update(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
    root.update(RootEvent::ReplaceDraft("keep me".to_owned()));

    let update = root.update(key(KeyCode::Char('z'), KeyModifiers::CONTROL));

    assert_eq!(update.render, super::RenderRequest::None);
    assert_eq!(root.composer().draft(), "keep me");
    assert!(root.discarded_draft.is_some());
}

#[test]
fn successful_session_replacement_preserves_the_displaced_draft() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.update(RootEvent::ReplaceDraft("continue later".to_owned()));

    root.reset_session(
        Path::new("/work"),
        ReasoningEffort::High,
        ReasoningMode::Standard,
        ReasoningMode::Standard,
        DraftReset::Clear,
    );
    root.update(key(KeyCode::Char('z'), KeyModifiers::CONTROL));

    assert_eq!(root.composer().draft(), "continue later");
}

#[test]
fn double_escape_interrupts_without_shutting_down() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);

    let first = root.update(key(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(
        pending_confirmation(&root),
        Some(ConfirmationAction::Interrupt)
    );
    // The confirmation prompt's copy is the contract users read, so it is checked once here.
    let rendered = render_root_text(&mut root, 60, 12);
    let second = root.update(key(KeyCode::Esc, KeyModifiers::NONE));

    assert!(first.effects.is_empty());
    assert!(rendered.contains("Esc then"));
    assert!(rendered.contains("Esc Interrupt"));
    assert!(rendered.contains("Any other key cancel"));
    assert_eq!(second.effects, [RootEffect::CancelTurns]);
}

#[test]
fn escape_requires_two_distinct_presses() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.update(key(KeyCode::Esc, KeyModifiers::NONE));

    let release = root.update(key_with_kind(
        KeyCode::Esc,
        KeyModifiers::NONE,
        KeyEventKind::Release,
    ));
    let repeat = root.update(key_with_kind(
        KeyCode::Esc,
        KeyModifiers::NONE,
        KeyEventKind::Repeat,
    ));

    assert!(release.effects.is_empty());
    assert!(repeat.effects.is_empty());
    assert!(root.key_confirmation.is_some());

    let second_press = root.update(key(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(second_press.effects, [RootEffect::CancelTurns]);
}

#[test]
fn tab_swaps_between_the_queue_and_composer() {
    let backend = TestBackend::new(60, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.turns.start_turn();
    for character in "queued".chars() {
        root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
    }
    root.update(key(KeyCode::Enter, KeyModifiers::NONE));
    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();

    root.update(key(KeyCode::Tab, KeyModifiers::NONE));
    assert!(root.queue.focused());
    root.update(key(KeyCode::Char('x'), KeyModifiers::NONE));
    assert!(root.composer().draft().is_empty());

    root.update(key(KeyCode::Tab, KeyModifiers::NONE));
    assert!(!root.queue.focused());
    root.update(key(KeyCode::Char('x'), KeyModifiers::NONE));

    assert_eq!(root.composer().draft(), "x");
}

#[test]
fn enter_steers_a_prompt_submitted_while_a_turn_runs() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.turns.start_turn();
    root.queue.push("later".to_owned());
    root.update(super::RootEvent::ReplaceDraft("steer now".to_owned()));

    let update = root.update(key(KeyCode::Enter, KeyModifiers::NONE));

    assert!(matches!(
        update.effects.as_slice(),
        [RootEffect::Steer { prompt, .. }] if prompt.display_text() == "steer now"
    ));
    assert!(!root.queue.focused());
    assert!(root.composer().draft().is_empty());
}

#[test]
fn enter_in_an_empty_composer_leaves_the_queue_alone() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.turns.start_turn();
    root.queue.push("later".to_owned());

    let update = root.update(key(KeyCode::Enter, KeyModifiers::NONE));

    assert!(update.effects.is_empty());
    assert_eq!(root.queue.len(), 1);
}

#[test]
fn shift_tab_queues_a_draft_without_steering() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.turns.start_turn();
    root.update(super::RootEvent::ReplaceDraft("later".to_owned()));

    let update = root.update(key(KeyCode::BackTab, KeyModifiers::SHIFT));

    assert!(update.effects.is_empty());
    assert_eq!(root.queue.len(), 1);
    assert!(root.composer().draft().is_empty());
}

#[test]
fn clicking_the_composer_returns_focus_to_it() {
    let backend = TestBackend::new(60, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.turns.start_turn();
    root.queue.push("queued".to_owned());
    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();
    root.update(key(KeyCode::Tab, KeyModifiers::NONE));
    assert!(root.queue.focused());

    let down = root.update(mouse(MouseEventKind::Down(MouseButton::Left), 10, 9));
    let up = root.update(mouse(MouseEventKind::Up(MouseButton::Left), 10, 9));

    assert!(!root.queue.focused());
    assert_eq!(down.render.max(up.render), super::RenderRequest::Immediate);
}

#[test]
fn clicking_the_queue_keeps_focus_on_the_queue() {
    let backend = TestBackend::new(60, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.queue.push("queued".to_owned());
    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();

    let update = root.update(mouse(
        MouseEventKind::Down(MouseButton::Left),
        root.queue_area.x + 2,
        root.queue_area.y + 1,
    ));

    assert!(root.queue.focused());
    assert_eq!(update.render, super::RenderRequest::Immediate);
}

#[test]
fn clicking_empty_transcript_space_returns_focus_to_the_composer() {
    let backend = TestBackend::new(60, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.turns.start_turn();
    root.queue.push("queued".to_owned());
    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();
    root.update(key(KeyCode::Tab, KeyModifiers::NONE));
    assert!(root.queue.focused());

    let update = root.update(mouse(MouseEventKind::Down(MouseButton::Left), 10, 2));

    assert!(!root.queue.focused());
    assert_eq!(update.render, super::RenderRequest::Immediate);
}

#[test]
fn active_turn_submissions_can_grow_the_queue_without_restoring_the_draft() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.turns.start_turn();
    for prompt in ["one", "two", "three", "four", "five"] {
        for character in prompt.chars() {
            root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
        }
        let update = root.update(key(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert!(update.effects.is_empty());
    }

    assert_eq!(root.queue.len(), 5);
    assert!(root.composer().draft().is_empty());
    assert!(!root.queue.focused());
    assert!(root.notification.is_none());

    let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();
    assert_eq!(root.queue_area.width, 95);
    assert_eq!(root.queue_area.bottom(), root.composer_area.y);
}

#[test]
fn shell_commands_bypass_the_agent_message_queue() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.turns.start_turn();
    root.update(super::RootEvent::ReplaceDraft("!pwd".to_owned()));

    let submitted = root.update(key(KeyCode::Enter, KeyModifiers::NONE));

    assert_eq!(submitted.effects, [RootEffect::RunShell("pwd".to_owned())]);
    assert!(root.queue.is_empty());
    assert_eq!(root.busy().turns, 1);
}

#[test]
fn finished_turns_batch_ready_queued_messages_in_order() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.turns.start_turn();
    root.queue.push("first".to_owned());
    root.queue.push("second".to_owned());

    let update = root.update(super::RootEvent::WorkerTurnFinished {
        terminal_expected: false,
    });
    assert_eq!(
        update.effects,
        [RootEffect::Submit("first\n\nsecond".to_owned().into())]
    );
    assert_eq!(root.busy().turns, 1);
    assert!(root.queue.is_empty());
}

#[test]
fn queue_edit_uses_the_composer_and_restores_its_draft_after_saving() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.turns.start_turn();
    root.update(super::RootEvent::ReplaceDraft(
        "unfinished draft".to_owned(),
    ));
    root.queue.push("original".to_owned());
    root.queue.set_focused(true);

    let edit = root.update(key(KeyCode::Char('e'), KeyModifiers::NONE));
    assert!(edit.effects.is_empty());
    assert_eq!(root.composer.draft(), "original");
    assert!(root.queue_edit.is_some());
    assert_eq!(root.queue.len(), 1);

    root.update(super::RootEvent::ReplaceDraft("edited".to_owned()));
    let saved = root.update(key(KeyCode::Enter, KeyModifiers::NONE));

    assert!(saved.effects.is_empty());
    assert_eq!(root.composer.draft(), "unfinished draft");
    assert!(root.queue_edit.is_none());
    assert_eq!(root.queue.len(), 1);
}

#[test]
fn editing_blocks_queue_dequeue_until_the_inline_edit_is_saved() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.turns.start_turn();
    root.queue.push("edit me".to_owned());
    root.queue.push("later".to_owned());
    root.queue.set_focused(true);
    root.update(key(KeyCode::Up, KeyModifiers::NONE));

    root.update(key(KeyCode::Char('e'), KeyModifiers::NONE));
    assert_eq!(root.composer.draft(), "edit me");

    let finished = root.update(super::RootEvent::WorkerTurnFinished {
        terminal_expected: false,
    });
    assert!(finished.effects.is_empty());
    assert_eq!(root.busy().turns, 0);

    root.update(super::RootEvent::ReplaceDraft("edited".to_owned()));
    let restored = root.update(key(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        restored.effects,
        [RootEffect::Submit("edited\n\nlater".to_owned().into())]
    );
    assert_eq!(root.busy().turns, 1);
    assert!(root.queue.is_empty());
}

#[test]
fn blank_queue_edit_discards_the_item_and_releases_later_messages() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.turns.start_turn();
    root.queue.push("discard me".to_owned());
    root.queue.push("later".to_owned());
    root.queue.set_focused(true);
    root.update(key(KeyCode::Up, KeyModifiers::NONE));

    root.update(key(KeyCode::Char('e'), KeyModifiers::NONE));
    let finished = root.update(super::RootEvent::WorkerTurnFinished {
        terminal_expected: false,
    });
    assert!(finished.effects.is_empty());

    root.update(super::RootEvent::ReplaceDraft("  \n".to_owned()));
    let edited = root.update(key(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        edited.effects,
        [RootEffect::Submit("later".to_owned().into())]
    );
    assert_eq!(root.busy().turns, 1);
    assert!(root.queue.is_empty());
}

#[test]
fn escape_cancels_a_queue_edit_and_releases_the_original_message() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.turns.start_turn();
    root.update(super::RootEvent::ReplaceDraft("keep this draft".to_owned()));
    root.queue.push("keep original".to_owned());
    root.queue.set_focused(true);
    root.update(key(KeyCode::Char('e'), KeyModifiers::NONE));
    root.update(super::RootEvent::ReplaceDraft(
        "discard this edit".to_owned(),
    ));

    let finished = root.update(super::RootEvent::WorkerTurnFinished {
        terminal_expected: false,
    });
    assert!(finished.effects.is_empty());

    let cancelled = root.update(key(KeyCode::Esc, KeyModifiers::NONE));

    assert_eq!(
        cancelled.effects,
        [RootEffect::Submit("keep original".to_owned().into())]
    );
    assert_eq!(root.composer.draft(), "keep this draft");
    assert!(root.queue_edit.is_none());
}

#[test]
fn shift_enter_in_a_queue_edit_inserts_a_newline() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.turns.start_turn();
    root.queue.push("first line".to_owned());
    root.queue.set_focused(true);
    root.update(key(KeyCode::Char('e'), KeyModifiers::NONE));

    let newline = root.update(key(KeyCode::Enter, KeyModifiers::SHIFT));

    assert!(newline.effects.is_empty());
    assert_eq!(root.composer.draft(), "first line\n");
    assert!(root.queue_edit.is_some());
}

#[test]
fn steer_completion_race_does_not_release_another_queued_message() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.turns.start_turn();
    root.queue.push("later".to_owned());
    root.queue.push("steer now".to_owned());
    root.queue.set_focused(true);

    let steer = root.update(key(KeyCode::Enter, KeyModifiers::NONE));
    let RootEffect::Steer { id, .. } = &steer.effects[0] else {
        panic!("enter should issue a steer");
    };
    let id = *id;
    let finished = root.update(super::RootEvent::WorkerTurnFinished {
        terminal_expected: false,
    });
    assert!(finished.effects.is_empty());
    assert_eq!(root.queue.len(), 2);

    root.update(super::RootEvent::SteerPromoted(id));
    let promoted_finished = root.update(super::RootEvent::WorkerTurnFinished {
        terminal_expected: false,
    });
    assert_eq!(
        promoted_finished.effects,
        [RootEffect::Submit("later".to_owned().into())]
    );
}

#[test]
fn interrupt_drains_a_pending_steer_before_regular_queue_items() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.turns.start_turn();
    root.queue.push("regular".to_owned());
    root.queue.push("priority steer".to_owned());
    root.queue.set_focused(true);
    root.update(key(KeyCode::Enter, KeyModifiers::NONE));
    root.queue.set_focused(false);

    root.update(key(KeyCode::Esc, KeyModifiers::NONE));
    let interrupt = root.update(key(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(interrupt.effects, [RootEffect::CancelTurns]);

    root.update(super::RootEvent::TurnsCancelled);
    let finished = root.update(super::RootEvent::WorkerTurnFinished {
        terminal_expected: false,
    });
    assert_eq!(
        finished.effects,
        [RootEffect::Submit(
            "priority steer\n\nregular".to_owned().into()
        )]
    );
}

#[test]
fn applied_steer_after_interrupt_ack_is_not_submitted_again() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.turns.start_turn();
    root.queue.push("later".to_owned());
    root.queue.push("priority steer".to_owned());
    root.queue.set_focused(true);
    let steer = root.update(key(KeyCode::Enter, KeyModifiers::NONE));
    let RootEffect::Steer { id, .. } = &steer.effects[0] else {
        panic!("enter should issue a steer");
    };
    let id = *id;
    root.update(super::RootEvent::SteerAdmitted(id));

    root.update(super::RootEvent::TurnsCancelled);
    let applied = root.update(run_steered());
    let finished = root.update(super::RootEvent::WorkerTurnFinished {
        terminal_expected: false,
    });

    assert_eq!(
        applied.effects,
        [RootEffect::PersistSteer("priority steer".to_owned())]
    );
    assert_eq!(
        finished.effects,
        [RootEffect::Submit("later".to_owned().into())]
    );
}

#[test]
fn steer_stays_queued_until_the_model_boundary_event() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.turns.start_turn();
    root.queue.push("steer".to_owned());
    root.queue.set_focused(true);
    let steer = root.update(key(KeyCode::Enter, KeyModifiers::NONE));
    let RootEffect::Steer { id, .. } = &steer.effects[0] else {
        panic!("enter should issue a steer");
    };
    let id = *id;

    let admitted = root.update(super::RootEvent::SteerAdmitted(id));
    assert!(admitted.effects.is_empty());
    assert_eq!(root.queue.len(), 1);

    let applied = root.update(run_steered());
    assert_eq!(
        applied.effects,
        [RootEffect::PersistSteer("steer".to_owned())]
    );
    assert!(root.queue.is_empty());
}

#[test]
fn model_boundary_before_worker_ack_is_reconciled_once() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.turns.start_turn();
    root.queue.push("steer".to_owned());
    root.queue.set_focused(true);
    let steer = root.update(key(KeyCode::Enter, KeyModifiers::NONE));
    let RootEffect::Steer { id, .. } = &steer.effects[0] else {
        panic!("enter should issue a steer");
    };
    let id = *id;

    let early_boundary = root.update(run_steered());
    assert!(early_boundary.effects.is_empty());
    assert_eq!(root.queue.len(), 1);

    let admitted = root.update(super::RootEvent::SteerAdmitted(id));
    assert_eq!(
        admitted.effects,
        [RootEffect::PersistSteer("steer".to_owned())]
    );
    assert!(root.queue.is_empty());
}

#[test]
fn displaced_release_over_the_composer_copies_without_drag_events() {
    let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    for character in "copy me".chars() {
        root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
    }
    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();

    root.update(mouse(MouseEventKind::Down(MouseButton::Left), 1, 8));
    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();
    let update = root.update(mouse(MouseEventKind::Up(MouseButton::Left), 7, 8));

    assert_eq!(update.effects, [RootEffect::Copy("copy me".to_owned())]);
    assert_eq!(update.render, super::RenderRequest::Immediate);
    assert!(!root.selection.is_active());
    assert!(root.notification.is_none());
    assert_eq!(root.composer().draft(), "copy me");
    root.update(super::RootEvent::NotifySuccess(
        "Copied selection to clipboard.".to_owned(),
    ));
    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("Copied selection to clipboard."));
    let buffer = terminal.backend().buffer();
    let left = (40 - ("Copied selection to clipboard.".len() as u16 + 4)) / 2;
    assert_eq!(buffer[(left, 0)].symbol(), "╭");
    assert_eq!(buffer[(left, 0)].fg, ratatui::style::Color::Green);
    assert!(buffer[(left + 2, 1)].modifier.contains(Modifier::BOLD));

    let deadline = root.notification.as_ref().unwrap().deadline;
    root.update(super::RootEvent::AnimationFrame(deadline));
    assert!(root.notification.is_none());
}

#[test]
fn narrow_notifications_keep_wrapped_action_text_visible() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.update(super::RootEvent::NotifySuccess(
        "Pro enabled for new sessions · start a new session to apply.".to_owned(),
    ));
    let mut terminal = Terminal::new(TestBackend::new(30, 10)).unwrap();

    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();

    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("start a new session"));
    assert!(rendered.contains("apply."));
}

#[test]
fn update_available_uses_the_success_frame_and_styles_version_and_command() {
    let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    let version = Version::new(1, 2, 3);
    let message = "Update available · v1.2.3 · run `tact update`";

    root.update(super::RootEvent::UpdateAvailable(version));
    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();

    let buffer = terminal.backend().buffer();
    let message_width = unicode_width::UnicodeWidthStr::width(message) as u16;
    let left = (80 - (message_width + 4)) / 2;
    let text_start = left + 2;
    let prefix_width = unicode_width::UnicodeWidthStr::width("Update available · ") as u16;
    let version_width = unicode_width::UnicodeWidthStr::width("v1.2.3") as u16;
    let suffix_width = unicode_width::UnicodeWidthStr::width(" · run ") as u16;
    let version_start = text_start + prefix_width;
    let command_start = version_start + version_width + suffix_width;

    assert_eq!(buffer[(left, 0)].symbol(), "╭");
    assert_eq!(buffer[(left, 0)].fg, Color::Green);
    for column in text_start..version_start {
        assert_eq!(buffer[(column, 1)].fg, Color::Green);
        assert!(!buffer[(column, 1)].modifier.contains(Modifier::BOLD));
    }
    for column in version_start..version_start + version_width {
        assert_eq!(buffer[(column, 1)].fg, Color::Green);
        assert!(buffer[(column, 1)].modifier.contains(Modifier::BOLD));
    }
    for column in version_start + version_width..command_start {
        assert_eq!(buffer[(column, 1)].fg, Color::Green);
        assert!(!buffer[(column, 1)].modifier.contains(Modifier::BOLD));
    }
    for column in command_start..text_start + message_width {
        assert_eq!(buffer[(column, 1)].fg, Color::Reset);
        assert!(!buffer[(column, 1)].modifier.contains(Modifier::BOLD));
    }

    let rendered = buffer
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains(message));

    let deadline = root.notification.as_ref().unwrap().deadline;
    root.update(super::RootEvent::AnimationFrame(deadline));
    assert!(root.notification.is_none());
}

#[test]
fn dragging_over_the_transcript_copies_visible_text() {
    let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    let record = TranscriptRecord::from_local(
        1,
        1,
        LocalEvent::UserSubmitted(UserSubmitted {
            id: TurnId::new(1),
            text: "hello transcript".to_owned(),
        }),
    )
    .unwrap();
    root.update(super::RootEvent::Transcript(Arc::new(record)));
    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();
    let row = (0..7)
        .find(|&row| terminal.backend().buffer()[(0, row)].symbol() == "┃")
        .unwrap();

    root.update(mouse(MouseEventKind::Down(MouseButton::Left), 2, row));
    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();
    root.update(mouse(MouseEventKind::Drag(MouseButton::Left), 6, row));
    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();
    let update = root.update(mouse(MouseEventKind::Up(MouseButton::Left), 6, row));

    assert_eq!(update.effects, [RootEffect::Copy("hello".to_owned())]);
    assert!(!root.selection.is_active());
}

#[test]
fn transcript_selection_copies_code_source_without_rendered_borders() {
    let mut terminal = Terminal::new(TestBackend::new(40, 14)).unwrap();
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.update(super::RootEvent::Transcript(agent_record(
        1,
        AgentEventKind::AssistantMessage,
        json!({
            "model_call_index": 1,
            "item_id": "answer",
            "phase": "final_answer",
            "text": "```rust\n    let answer = 42;\n```",
        }),
    )));
    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();
    let row = (0..terminal.backend().buffer().area.height)
        .find(|&row| {
            (0..terminal.backend().buffer().area.width)
                .map(|column| terminal.backend().buffer()[(column, row)].symbol())
                .collect::<String>()
                .contains("    let answer = 42;")
        })
        .expect("code should be visible");

    root.update(mouse(MouseEventKind::Down(MouseButton::Left), 0, row));
    root.update(mouse(MouseEventKind::Drag(MouseButton::Left), 39, row));
    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();

    let buffer = terminal.backend().buffer();
    assert_ne!(buffer[(0, row)].bg, Color::Yellow);
    assert_eq!(buffer[(2, row)].bg, Color::Yellow);
    assert_ne!(buffer[(39, row)].bg, Color::Yellow);

    let update = root.update(mouse(MouseEventKind::Up(MouseButton::Left), 39, row));
    assert_eq!(
        update.effects,
        [RootEffect::Copy("    let answer = 42;".to_owned())]
    );
}

#[test]
fn transcript_selection_copies_shell_command_and_output_without_chrome_or_soft_wraps() {
    let mut terminal = Terminal::new(TestBackend::new(40, 16)).unwrap();
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    let command = "$HOME/bin/printf output";
    let output = "alpha beta gamma delta epsilon zeta\nsecond line";
    root.update(super::RootEvent::Transcript(agent_record(
        1,
        AgentEventKind::ToolCall,
        json!({
            "call_id": "workflow",
            "tool": "exec",
            "arguments": "await tools.exec_command({cmd: '$HOME/bin/printf output'})",
        }),
    )));
    root.update(super::RootEvent::Transcript(agent_record(
        2,
        AgentEventKind::ToolCall,
        json!({
            "call_id": "workflow/shell",
            "tool": "exec_command",
            "arguments": {"cmd": command},
        }),
    )));
    root.update(super::RootEvent::Transcript(agent_record(
        3,
        AgentEventKind::ToolResult,
        json!({
            "call_id": "workflow/shell",
            "tool": "exec_command",
            "status": "completed",
            "duration_ns": 1_u64,
            "result": format!(
                "Wall time: 0.0000 seconds\nProcess exited with code 0\nOutput:\n{output}"
            ),
            "structured_result": {
                "output": output,
                "exit_code": 0,
                "wall_time_seconds": 0.0,
            },
            "metadata": null,
        }),
    )));
    root.update(key(KeyCode::Char('o'), KeyModifiers::CONTROL));
    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let command_row = (0..buffer.area.height)
        .rfind(|&row| {
            (0..buffer.area.width)
                .map(|column| buffer[(column, row)].symbol())
                .collect::<String>()
                .contains(command)
        })
        .expect("the expanded shell command should be visible");
    let command_start = text_column(buffer, command_row, command);
    let command_end = command_start + u16::try_from(command.len()).unwrap();

    root.update(mouse(
        MouseEventKind::Down(MouseButton::Left),
        command_start,
        command_row,
    ));
    root.update(mouse(
        MouseEventKind::Drag(MouseButton::Left),
        command_end,
        command_row,
    ));
    let update = root.update(mouse(
        MouseEventKind::Up(MouseButton::Left),
        command_end,
        command_row,
    ));
    assert_eq!(update.effects, [RootEffect::Copy(command.to_owned())]);

    let first_row = (0..buffer.area.height)
        .find(|&row| {
            (0..buffer.area.width)
                .map(|column| buffer[(column, row)].symbol())
                .collect::<String>()
                .contains("alpha")
        })
        .expect("the first output line should be visible");
    let last_row = (0..buffer.area.height)
        .find(|&row| {
            (0..buffer.area.width)
                .map(|column| buffer[(column, row)].symbol())
                .collect::<String>()
                .contains("second line")
        })
        .expect("the second output line should be visible");
    let start = text_column(buffer, first_row, "alpha");
    let end = text_column(buffer, last_row, "second line") + 10;

    root.update(mouse(
        MouseEventKind::Down(MouseButton::Left),
        start,
        first_row,
    ));
    root.update(mouse(
        MouseEventKind::Drag(MouseButton::Left),
        end,
        last_row,
    ));
    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();
    let buffer = terminal.backend().buffer();
    assert_ne!(buffer[(0, first_row)].bg, Color::Yellow);
    assert_ne!(
        buffer[(start.saturating_sub(1), first_row)].bg,
        Color::Yellow
    );
    assert_eq!(buffer[(start, first_row)].bg, Color::Yellow);

    let update = root.update(mouse(MouseEventKind::Up(MouseButton::Left), end, last_row));
    assert_eq!(update.effects, [RootEffect::Copy(output.to_owned())]);
}

#[test]
fn transcript_selection_copies_original_markdown_syntax() {
    let mut terminal = Terminal::new(TestBackend::new(64, 12)).unwrap();
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.update(super::RootEvent::Transcript(agent_record(
        1,
        AgentEventKind::AssistantMessage,
        json!({
            "model_call_index": 1,
            "item_id": "answer",
            "phase": "final_answer",
            "text": "**bold** and [site](https://example.com)",
        }),
    )));
    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();
    let row = (0..terminal.backend().buffer().area.height)
        .find(|&row| {
            (0..terminal.backend().buffer().area.width)
                .map(|column| terminal.backend().buffer()[(column, row)].symbol())
                .collect::<String>()
                .contains("bold and site")
        })
        .expect("message should be visible");
    let start = text_column(terminal.backend().buffer(), row, "bold");
    let end = text_column(terminal.backend().buffer(), row, "site") + 3;

    root.update(mouse(MouseEventKind::Down(MouseButton::Left), start, row));
    root.update(mouse(MouseEventKind::Drag(MouseButton::Left), end, row));
    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();
    let destination = text_column(terminal.backend().buffer(), row, "https://example.com");
    assert_ne!(
        terminal.backend().buffer()[(destination, row)].bg,
        Color::Yellow
    );

    let update = root.update(mouse(MouseEventKind::Up(MouseButton::Left), end, row));
    assert_eq!(
        update.effects,
        [RootEffect::Copy(
            "**bold** and [site](https://example.com)".to_owned()
        )]
    );
}

#[test]
fn transcript_selection_crosses_table_cell_boundaries() {
    let mut terminal = Terminal::new(TestBackend::new(40, 14)).unwrap();
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    let markdown = "| Left | Right |\n|---|---|\n| alpha | omega |";
    root.update(super::RootEvent::Transcript(agent_record(
        1,
        AgentEventKind::AssistantMessage,
        json!({
            "model_call_index": 1,
            "item_id": "answer",
            "phase": "final_answer",
            "text": markdown,
        }),
    )));
    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let header_row = (0..buffer.area.height)
        .find(|&row| {
            (0..buffer.area.width)
                .map(|column| buffer[(column, row)].symbol())
                .collect::<String>()
                .contains("Left")
        })
        .expect("table header should be visible");
    let data_row = header_row + 2;
    let start = text_column(buffer, header_row, "Left");
    let end = text_column(buffer, data_row, "omega") + 5;
    let separator_row = header_row + 1;

    root.update(mouse(
        MouseEventKind::Down(MouseButton::Left),
        start,
        header_row,
    ));
    root.update(mouse(
        MouseEventKind::Drag(MouseButton::Left),
        end,
        separator_row,
    ));
    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();

    let buffer = terminal.backend().buffer();
    for text in ["Left", "Right", "alpha", "omega"] {
        let row = if matches!(text, "Left" | "Right") {
            header_row
        } else {
            data_row
        };
        let column = text_column(buffer, row, text);
        assert_eq!(
            buffer[(column, row)].bg,
            Color::Yellow,
            "{text} should be included in the table selection"
        );
    }

    let update = root.update(mouse(
        MouseEventKind::Up(MouseButton::Left),
        end,
        separator_row,
    ));
    assert_eq!(update.effects, [RootEffect::Copy(markdown.to_owned())]);

    let alpha = text_column(terminal.backend().buffer(), data_row, "alpha");
    let omega_start = text_column(terminal.backend().buffer(), data_row, "omega");
    let cell_divider = omega_start - 1;
    root.update(mouse(
        MouseEventKind::Down(MouseButton::Left),
        alpha,
        data_row,
    ));
    root.update(mouse(
        MouseEventKind::Drag(MouseButton::Left),
        cell_divider,
        data_row,
    ));
    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();
    assert_eq!(
        terminal.backend().buffer()[(omega_start, data_row)].bg,
        Color::Yellow
    );
    let update = root.update(mouse(
        MouseEventKind::Up(MouseButton::Left),
        cell_divider,
        data_row,
    ));
    assert_eq!(update.effects, [RootEffect::Copy(" alpha | o".to_owned())]);

    let omega = text_column(terminal.backend().buffer(), data_row, "omega") + 4;
    root.update(mouse(
        MouseEventKind::Down(MouseButton::Left),
        omega,
        data_row,
    ));
    root.update(mouse(
        MouseEventKind::Drag(MouseButton::Left),
        start,
        separator_row,
    ));
    let update = root.update(mouse(
        MouseEventKind::Up(MouseButton::Left),
        start,
        separator_row,
    ));
    assert_eq!(update.effects, [RootEffect::Copy(markdown.to_owned())]);
}

#[test]
fn transcript_selection_highlights_rendered_link_destinations() {
    let mut terminal = Terminal::new(TestBackend::new(120, 12)).unwrap();
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    let markdown = "See [inclusion.rs](/workspace/glue/src/inclusion.rs) or [mailbox.rs](/workspace/glue/src/mailbox.rs).";
    root.update(super::RootEvent::Transcript(agent_record(
        1,
        AgentEventKind::AssistantMessage,
        json!({
            "model_call_index": 1,
            "item_id": "answer",
            "phase": "final_answer",
            "text": markdown,
        }),
    )));
    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let row = (0..buffer.area.height)
        .find(|&row| {
            (0..buffer.area.width)
                .map(|column| buffer[(column, row)].symbol())
                .collect::<String>()
                .contains("See inclusion.rs")
        })
        .expect("message should be visible");
    let start = text_column(buffer, row, "See");
    let rendered_link = "mailbox.rs ↗ /workspace/glue/src/mailbox.rs";
    let end = text_column(buffer, row, rendered_link)
        + u16::try_from(rendered_link.chars().count()).unwrap();

    root.update(mouse(MouseEventKind::Down(MouseButton::Left), start, row));
    root.update(mouse(MouseEventKind::Drag(MouseButton::Left), end, row));
    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();

    let buffer = terminal.backend().buffer();
    for column in start..end {
        let cell = &buffer[(column, row)];
        assert_eq!(
            (cell.fg, cell.bg, cell.modifier),
            (Color::Black, Color::Yellow, Modifier::empty()),
            "selected cell at column {column} ({:?}) retained link styling",
            cell.symbol()
        );
    }
}

#[test]
fn partial_transcript_selection_does_not_copy_unmatched_markdown_delimiters() {
    let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.update(super::RootEvent::Transcript(agent_record(
        1,
        AgentEventKind::AssistantMessage,
        json!({
            "model_call_index": 1,
            "item_id": "answer",
            "phase": "final_answer",
            "text": "**bold**",
        }),
    )));
    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();
    let row = (0..root.transcript_area.height)
        .find(|&row| {
            (0..40)
                .map(|column| terminal.backend().buffer()[(column, row)].symbol())
                .collect::<String>()
                .contains("bold")
        })
        .unwrap();
    let start = text_column(terminal.backend().buffer(), row, "bold");

    root.update(mouse(MouseEventKind::Down(MouseButton::Left), start, row));
    root.update(mouse(
        MouseEventKind::Drag(MouseButton::Left),
        start + 1,
        row,
    ));
    let update = root.update(mouse(MouseEventKind::Up(MouseButton::Left), start + 1, row));

    assert_eq!(update.effects, [RootEffect::Copy("bo".to_owned())]);
}

#[test]
fn transcript_selection_survives_scrolling_beyond_the_viewport() {
    let mut terminal = Terminal::new(TestBackend::new(32, 12)).unwrap();
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    for sequence in 1..=10 {
        let record = TranscriptRecord::from_local(
            sequence,
            sequence,
            LocalEvent::UserSubmitted(UserSubmitted {
                id: TurnId::new(sequence),
                text: format!("prompt {sequence}"),
            }),
        )
        .unwrap();
        root.update(super::RootEvent::Transcript(Arc::new(record)));
    }
    root.update(key(KeyCode::Home, KeyModifiers::CONTROL));
    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();
    let start_row = (0..root.transcript_area.height)
        .find(|&row| {
            (0..32)
                .map(|column| terminal.backend().buffer()[(column, row)].symbol())
                .collect::<String>()
                .contains("prompt 1")
        })
        .expect("first prompt should be visible");

    root.update(mouse(MouseEventKind::Down(MouseButton::Left), 0, start_row));
    root.update(mouse(MouseEventKind::ScrollDown, 4, start_row));
    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();
    let end_row = (0..root.transcript_area.height)
        .rev()
        .find(|&row| terminal.backend().buffer()[(0, row)].symbol() == "┃")
        .expect("a later prompt should be visible");
    let end_column = (0..32)
        .rev()
        .find(|&column| terminal.backend().buffer()[(column, end_row)].symbol() != " ")
        .expect("prompt should contain text");
    root.update(mouse(
        MouseEventKind::Drag(MouseButton::Left),
        end_column,
        end_row,
    ));
    let update = root.update(mouse(
        MouseEventKind::Up(MouseButton::Left),
        end_column,
        end_row,
    ));

    assert_eq!(
        update.effects,
        [RootEffect::Copy(
            "prompt 1\n\nprompt 2\n\nprompt 3\n\nprompt 4\n\nprompt 5".to_owned()
        )]
    );
}

#[test]
fn dragging_at_the_viewport_edge_keeps_extending_the_selection() {
    let mut terminal = Terminal::new(TestBackend::new(32, 12)).unwrap();
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    for sequence in 1..=12 {
        let record = TranscriptRecord::from_local(
            sequence,
            sequence,
            LocalEvent::UserSubmitted(UserSubmitted {
                id: TurnId::new(sequence),
                text: format!("prompt {sequence}"),
            }),
        )
        .unwrap();
        root.update(super::RootEvent::Transcript(Arc::new(record)));
    }
    root.update(key(KeyCode::Home, KeyModifiers::CONTROL));
    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();
    let start_row = (0..root.transcript_area.height)
        .find(|&row| {
            (0..32)
                .map(|column| terminal.backend().buffer()[(column, row)].symbol())
                .collect::<String>()
                .contains("prompt 1")
        })
        .unwrap();
    let edge = root.transcript_area.bottom().saturating_sub(1);
    root.update(mouse(MouseEventKind::Down(MouseButton::Left), 2, start_row));
    root.update(mouse(MouseEventKind::Drag(MouseButton::Left), 31, edge));

    for _ in 0..4 {
        terminal
            .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        let deadline = root
            .selection_auto_scroll
            .as_ref()
            .expect("edge drag should keep scrolling")
            .deadline;
        root.update(super::RootEvent::AnimationFrame(deadline));
    }
    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();
    let update = root.update(mouse(MouseEventKind::Up(MouseButton::Left), 31, edge));
    let [RootEffect::Copy(text)] = update.effects.as_slice() else {
        panic!("edge drag should copy the semantic selection");
    };

    assert!(text.starts_with("prompt 1\n\n"));
    assert!(text.contains("prompt 6"));
    assert!(!text.contains('┃'));
    assert!(root.selection_auto_scroll.is_none());
}

#[test]
fn composer_selection_scrolls_without_losing_offscreen_text() {
    let mut terminal = Terminal::new(TestBackend::new(32, 12)).unwrap();
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.update(super::RootEvent::ReplaceDraft(
        (1..=10)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n"),
    ));
    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();
    let start_row = root.composer_content_area.bottom().saturating_sub(1);
    let start_column = text_column(terminal.backend().buffer(), start_row, "line 10");

    root.update(mouse(
        MouseEventKind::Down(MouseButton::Left),
        start_column + 6,
        start_row,
    ));
    root.update(mouse(MouseEventKind::ScrollUp, start_column, start_row));
    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();
    let end_row = root.composer_content_area.y;
    let end_column = text_column(terminal.backend().buffer(), end_row, "line 2");
    root.update(mouse(
        MouseEventKind::Drag(MouseButton::Left),
        end_column,
        end_row,
    ));
    let update = root.update(mouse(
        MouseEventKind::Up(MouseButton::Left),
        end_column,
        end_row,
    ));

    assert_eq!(
        update.effects,
        [RootEffect::Copy(
            (2..=10)
                .map(|line| format!("line {line}"))
                .collect::<Vec<_>>()
                .join("\n")
        )]
    );
}

#[test]
fn transcript_selection_excludes_the_top_right_hint() {
    let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    let record = TranscriptRecord::from_local(
        1,
        1,
        LocalEvent::UserSubmitted(UserSubmitted {
            id: TurnId::new(1),
            text: ["copy this prompt"; 8].join("\n"),
        }),
    )
    .unwrap();
    root.update(super::RootEvent::Transcript(Arc::new(record)));
    root.transcript.focus_expandables();
    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();

    root.update(mouse(MouseEventKind::Down(MouseButton::Left), 2, 0));
    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();
    root.update(mouse(MouseEventKind::Drag(MouseButton::Left), 39, 0));
    terminal
        .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
        .unwrap();
    let update = root.update(mouse(MouseEventKind::Up(MouseButton::Left), 39, 0));

    assert_eq!(
        update.effects,
        [RootEffect::Copy("copy this prompt".to_owned())]
    );
}

#[test]
fn key_confirmation_expires_and_unrelated_input_resets_it() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    let now = Instant::now();

    assert!(
        root.update_key_confirmation(ConfirmationAction::Interrupt, now)
            .effects
            .is_empty()
    );
    assert!(
        root.update_key_confirmation(
            ConfirmationAction::Interrupt,
            now + super::KEY_CONFIRMATION_TIMEOUT + Duration::from_millis(1),
        )
        .effects
        .is_empty()
    );
    root.update(key(KeyCode::Char('x'), KeyModifiers::NONE));
    assert!(
        root.update(key(KeyCode::Esc, KeyModifiers::NONE))
            .effects
            .is_empty()
    );
}

#[test]
fn expired_confirmation_is_removed_immediately() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    let now = Instant::now();
    root.update_key_confirmation(ConfirmationAction::Exit, now);

    let update = root.update(super::RootEvent::AnimationFrame(
        now + super::KEY_CONFIRMATION_TIMEOUT,
    ));

    assert!(root.key_confirmation.is_none());
    assert_eq!(update.render, super::RenderRequest::Immediate);
}

#[test]
fn rejected_effort_update_restores_display_and_reports_error() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::High);
    let update = root.update(RootEvent::EffortUpdateFailed {
        effort: ReasoningEffort::Low,
        error: "Effort update rejected".to_owned(),
    });
    assert_eq!(root.composer().effort(), ReasoningEffort::Low);
    assert!(update.effects.is_empty());
    assert!(
        root.notification
            .as_ref()
            .unwrap()
            .message
            .to_string()
            .contains("Effort update rejected")
    );
}

#[test]
fn started_claude_session_explains_why_effort_is_fixed() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.set_model(Model::Claude(nanocodex::ClaudeModel::Opus55));
    root.open_effort();
    assert!(matches!(root.overlay, Some(Overlay::Effort(_))));
    root.thread = ThreadState::Started;
    let update = root.open_effort();
    assert!(update.effects.is_empty());
    assert!(root.overlay.is_none());
    assert_eq!(root.composer().effort(), ReasoningEffort::Medium);
    assert!(
        root.notification
            .as_ref()
            .unwrap()
            .message
            .to_string()
            .contains("Effort is fixed for this Claude session")
    );
}

#[test]
fn effort_action_opens_the_selector_and_applies_the_selection() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));

    for character in "effort".chars() {
        root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
    }

    root.update(key(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(&root.overlay, Some(Overlay::Effort(_))));

    root.update(key(KeyCode::Right, KeyModifiers::NONE));
    assert!(root.animation_deadline().is_some());
    let update = root.update(key(KeyCode::Enter, KeyModifiers::NONE));

    assert_eq!(
        update.effects,
        [RootEffect::SetEffort {
            effort: ReasoningEffort::High,
            reasoning_mode: ReasoningMode::Standard,
        }]
    );
    assert_eq!(root.composer().effort(), ReasoningEffort::High);
    assert!(root.overlay.is_none());

    let theme = Theme::default();
    let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
    terminal
        .draw(|frame| root.render(frame, frame.area(), &theme))
        .unwrap();
    let plasma = terminal
        .backend()
        .buffer()
        .content()
        .chunks(80)
        .take(15)
        .flatten()
        .filter(|cell| cell.symbol() != " ")
        .collect::<Vec<_>>();
    assert!(!plasma.is_empty());
    assert!(
        plasma
            .iter()
            .all(|cell| matches!(cell.fg, Color::Yellow) || cell.fg == theme.code_text())
    );
}

#[test]
fn pro_preference_does_not_change_the_running_session_mode() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));
    for character in "effort".chars() {
        root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
    }
    root.update(key(KeyCode::Enter, KeyModifiers::NONE));

    root.update(key(KeyCode::Char('p'), KeyModifiers::NONE));
    let update = root.update(key(KeyCode::Enter, KeyModifiers::NONE));

    assert_eq!(
        update.effects,
        [RootEffect::SetEffort {
            effort: ReasoningEffort::Medium,
            reasoning_mode: ReasoningMode::Pro,
        }]
    );
    assert_eq!(root.composer().reasoning_mode(), ReasoningMode::Standard);
    let notification = root.notification.as_ref().unwrap();
    let message = notification
        .message
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();
    assert_eq!(
        message,
        "Pro enabled for new sessions · start a new session to apply."
    );
    assert_eq!(notification.color, Color::Green);

    root.reset_session(
        Path::new("/work"),
        ReasoningEffort::Medium,
        ReasoningMode::Pro,
        ReasoningMode::Pro,
        DraftReset::Clear,
    );
    assert_eq!(root.composer().reasoning_mode(), ReasoningMode::Pro);

    root.open_effort();
    root.update(key(KeyCode::Char('p'), KeyModifiers::NONE));
    root.update(key(KeyCode::Enter, KeyModifiers::NONE));
    let notification = root.notification.as_ref().unwrap();
    let message = notification
        .message
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();
    assert_eq!(
        message,
        "Pro disabled for new sessions · start a new session to apply."
    );
    assert_eq!(notification.color, Color::Green);
}

#[test]
fn speed_action_selects_applies_and_cancels_preferences() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));
    for character in "speed".chars() {
        root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
    }
    let opened = root.update(key(KeyCode::Enter, KeyModifiers::NONE));
    assert!(opened.effects.is_empty());
    assert!(matches!(root.overlay, Some(Overlay::Speed(_))));
    root.update(key(KeyCode::Left, KeyModifiers::NONE));
    assert_eq!(root.composer().speed(), Speed::Standard);
    assert!(root.animation_deadline().is_some());
    let applied = root.update(key(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(applied.effects, [RootEffect::SetSpeed(Speed::Ultrafast)]);
    assert_eq!(root.composer().speed(), Speed::Ultrafast);
    assert!(root.overlay.is_none());

    root.open_speed();
    root.update(key(KeyCode::Right, KeyModifiers::NONE));
    let cancelled = root.update(key(KeyCode::Esc, KeyModifiers::NONE));
    assert!(cancelled.effects.is_empty());
    assert_eq!(root.composer().speed(), Speed::Ultrafast);
    assert!(root.overlay.is_none());
}

#[test]
fn unsupported_models_preserve_speed_preference_for_forks_and_later_models() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.set_speed(Speed::Ultrafast);
    for model in [
        ClaudeModel::Haiku55,
        ClaudeModel::Sonnet55,
        ClaudeModel::Fable51,
    ] {
        root.set_model(Model::Claude(model));
        assert_eq!(root.composer().speed(), Speed::Ultrafast);
        let fork = root.fork(Path::new("/work"), ReasoningEffort::Medium);
        assert_eq!(fork.composer().speed(), Speed::Ultrafast);
        assert!(render_root_text(&mut root, 80, 18).contains("medium 󰳗"));
        root.open_speed();
        assert!(render_root_text(&mut root, 80, 18).contains("Uses standard with this model"));
        root.update(key(KeyCode::Esc, KeyModifiers::NONE));
    }
    root.set_model(Model::Codex(CodexModel::Sol));
    assert!(render_root_text(&mut root, 80, 18).contains("medium 󰑣"));
    assert_eq!(root.composer().speed(), Speed::Ultrafast);
}

#[test]
fn theme_action_opens_the_selector_and_applies_the_selection() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));
    for character in "appearance".chars() {
        root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
    }

    root.update(key(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(&root.overlay, Some(Overlay::Theme(_))));

    root.update(key(KeyCode::Down, KeyModifiers::NONE));
    let update = root.update(key(KeyCode::Enter, KeyModifiers::NONE));

    assert_eq!(update.effects, [RootEffect::SetTheme(ThemeMode::Light)]);
    assert!(root.overlay.is_none());
}

#[test]
fn subagents_action_reopens_the_active_filter_on_the_oldest_active_agent() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    for (id, role) in [(1, "completed"), (2, "active")] {
        root.update(RootEvent::Subagent(AgentUpdate::Added(AgentDescriptor {
            role: role.to_owned(),
            ..subagent(id, role)
        })));
    }
    root.update(RootEvent::Subagent(AgentUpdate::Status {
        id: AgentId::new(1),
        status: AgentStatus::Completed {
            output: json!({ "report": "done" }),
        },
    }));
    root.subagents.update_tree(Event::Key(KeyEvent::new(
        KeyCode::Char('f'),
        KeyModifiers::NONE,
    )));

    root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));
    for character in "agents".chars() {
        root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
    }
    root.update(key(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(
        root.overlay,
        Some(Overlay::Subagents(SubagentOverlay::Tree))
    ));

    root.update(key(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(
        root.overlay,
        Some(Overlay::Subagents(SubagentOverlay::Transcript(id)))
            if id == AgentId::new(2)
    ));
}

#[test]
fn control_s_opens_effort_for_new_and_started_threads() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);

    let opened = root.update(key(KeyCode::Char('s'), KeyModifiers::CONTROL));
    assert!(matches!(&root.overlay, Some(Overlay::Effort(_))));
    assert_eq!(opened.render, super::RenderRequest::Immediate);

    root.update(key(KeyCode::Esc, KeyModifiers::NONE));
    root.thread = super::ThreadState::Started;
    let reopened = root.update(key(KeyCode::Char('s'), KeyModifiers::CONTROL));
    assert!(matches!(&root.overlay, Some(Overlay::Effort(_))));
    assert_eq!(reopened.render, super::RenderRequest::Immediate);
}

#[test]
fn control_d_selects_a_model_only_before_the_first_prompt() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);

    let opened = root.update(key(KeyCode::Char('d'), KeyModifiers::CONTROL));
    assert!(matches!(&root.overlay, Some(Overlay::Model(_))));
    assert_eq!(opened.render, super::RenderRequest::Immediate);

    root.update(key(KeyCode::Left, KeyModifiers::NONE));
    let selected = root.update(key(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        selected.effects,
        [RootEffect::SetModel(Model::Codex(CodexModel::Luna))]
    );

    root.interactive = true;
    root.thread = super::ThreadState::Started;
    let blocked = root.update(key(KeyCode::Char('d'), KeyModifiers::CONTROL));
    assert!(blocked.effects.is_empty());
    assert!(root.overlay.is_none());
}

#[test]
fn claude_availability_survives_reset_and_fork() {
    let workspace = Path::new("/work");
    let mut root = RootNode::new(workspace, ReasoningEffort::Medium);
    root.set_claude_enabled(true);
    assert!(root.fork(workspace, ReasoningEffort::Medium).claude_enabled);
    root.reset_session(
        workspace,
        ReasoningEffort::Medium,
        ReasoningMode::Standard,
        ReasoningMode::Standard,
        DraftReset::Clear,
    );
    root.set_model(Model::Codex(CodexModel::Astra));
    root.open_model();
    root.set_claude_enabled(false);
    root.update(key(KeyCode::Right, KeyModifiers::NONE));
    let result = root.update(key(KeyCode::Enter, KeyModifiers::NONE));
    assert!(result.effects.is_empty());
    assert_eq!(root.composer().model(), Model::Codex(CodexModel::Astra));
}

#[test]
fn fork_inherits_the_model_and_cannot_change_it() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.set_model(Model::Codex(CodexModel::Luna));

    let mut fork = root.fork(Path::new("/work"), ReasoningEffort::Medium);

    assert_eq!(fork.composer().model(), Model::Codex(CodexModel::Luna));
    let update = fork.update(key(KeyCode::Char('d'), KeyModifiers::CONTROL));
    assert!(update.effects.is_empty());
    assert!(fork.overlay.is_none());
}

#[test]
fn model_action_opens_only_for_a_new_thread() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));
    for character in "intelligence".chars() {
        root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
    }
    root.update(key(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(root.overlay, Some(Overlay::Model(_))));

    root.overlay = None;
    root.thread = ThreadState::Started;
    root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));
    for character in "intelligence".chars() {
        root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
    }
    let blocked = root.update(key(KeyCode::Enter, KeyModifiers::NONE));
    assert!(blocked.effects.is_empty());
    assert!(matches!(root.overlay, Some(Overlay::Actions(_))));
}

#[test]
fn control_r_loads_recent_prompts_and_inserts_from_the_current_session() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.update(RootEvent::ReplaceDraft("keep while loading".to_owned()));

    let loading = root.update(key(KeyCode::Char('r'), KeyModifiers::CONTROL));

    assert_eq!(loading.effects, [RootEffect::LoadRecentPrompts(Vec::new())]);
    assert_eq!(root.composer().draft(), "keep while loading");
    assert!(!root.interactive);

    root.update(RootEvent::RecentPromptsLoaded {
        session_id: "current".to_owned(),
        prompts: vec![
            RecentPrompt {
                text: "other prompt".to_owned(),
                recorded_at_unix_ms: 2,
                session_id: "other".to_owned(),
                workspace: "/other".into(),
            },
            RecentPrompt {
                text: "  current\n\n    prompt  ".to_owned(),
                recorded_at_unix_ms: 1,
                session_id: "current".to_owned(),
                workspace: "/work".into(),
            },
        ],
    });
    assert!(matches!(&root.overlay, Some(Overlay::RecentPrompts(_))));

    root.update(key(KeyCode::Char('f'), KeyModifiers::CONTROL));
    for character in "crp".chars() {
        root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
    }
    root.update(key(KeyCode::Enter, KeyModifiers::NONE));

    assert!(root.overlay.is_none());
    assert_eq!(root.composer().draft(), "  current\n\n    prompt  ");
}

#[test]
fn recent_prompt_load_failure_preserves_the_draft() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.update(RootEvent::ReplaceDraft("keep me".to_owned()));
    root.update(key(KeyCode::Char('r'), KeyModifiers::CONTROL));

    root.update(RootEvent::RecentPromptLoadFailed("load failed".to_owned()));

    assert!(root.interactive);
    assert_eq!(root.composer().draft(), "keep me");
    assert!(root.notification.is_some());
}

#[test]
fn control_r_includes_the_in_memory_prompt_before_loading_disk_history() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    let prompt = TranscriptRecord::from_local(
        1,
        42,
        LocalEvent::UserSubmitted(UserSubmitted {
            id: TurnId::new(1),
            text: "just submitted".to_owned(),
        }),
    )
    .unwrap();
    root.update(RootEvent::Transcript(Arc::new(prompt)));

    let loading = root.update(key(KeyCode::Char('r'), KeyModifiers::CONTROL));

    assert_eq!(
        loading.effects,
        [RootEffect::LoadRecentPrompts(vec![
            super::RecentPromptDraft {
                text: "just submitted".to_owned(),
                recorded_at_unix_ms: 42,
            },
        ])]
    );
}

#[test]
fn control_o_toggles_transcript_expansion_globally() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);

    let expanded = root.update(key(KeyCode::Char('o'), KeyModifiers::CONTROL));
    let collapsed = root.update(key(KeyCode::Char('o'), KeyModifiers::CONTROL));

    assert_eq!(expanded.render, super::RenderRequest::Immediate);
    assert_eq!(collapsed.render, super::RenderRequest::Immediate);
    assert!(root.composer().draft().is_empty());
}

#[test]
fn control_o_does_not_change_the_hidden_transcript_behind_an_overlay() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));

    let update = root.update(key(KeyCode::Char('o'), KeyModifiers::CONTROL));

    assert_eq!(update.render, super::RenderRequest::None);
    assert!(matches!(root.overlay, Some(Overlay::Actions(_))));
}

#[test]
fn escape_that_blurs_expandable_items_does_not_start_the_interrupt_chord() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.transcript.focus_expandables();

    let blurred = root.update(key(KeyCode::Esc, KeyModifiers::NONE));

    assert!(blurred.effects.is_empty());
    assert!(!root.transcript.expandables_focused());
    assert!(root.key_confirmation.is_none());

    let chord_started = root.update(key(KeyCode::Esc, KeyModifiers::NONE));
    assert!(chord_started.effects.is_empty());
    assert!(root.key_confirmation.is_some());
}

#[test]
fn keybindings_action_opens_help_and_escape_closes_it() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));
    for character in "keyboard".chars() {
        root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
    }

    root.update(key(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(&root.overlay, Some(Overlay::Keybindings(_))));

    root.update(key(KeyCode::Esc, KeyModifiers::NONE));
    assert!(root.overlay.is_none());
}

#[test]
fn resize_redraws_while_keybindings_help_is_open() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));
    for character in "keyboard".chars() {
        root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
    }
    root.update(key(KeyCode::Enter, KeyModifiers::NONE));

    let update = root.update(super::RootEvent::Terminal(Event::Resize(100, 30)));

    assert_eq!(update.render, super::RenderRequest::Immediate);
}

#[test]
fn config_action_closes_the_menu_and_requests_the_external_editor() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));
    for character in "edit config".chars() {
        root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
    }

    let update = root.update(key(KeyCode::Enter, KeyModifiers::NONE));

    assert!(root.overlay.is_none());
    assert_eq!(update.effects, [RootEffect::OpenConfigEditor]);
    assert_eq!(update.render, super::RenderRequest::Immediate);
}

#[test]
fn manual_compaction_uses_composer_activity_and_clears_success_and_failure() {
    for error in [None, Some("synthetic failure".to_owned())] {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));
        for character in "compact".chars() {
            root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
        }
        assert_eq!(
            root.update(key(KeyCode::Enter, KeyModifiers::NONE)).effects,
            [RootEffect::Compact]
        );
        root.update(super::RootEvent::Transcript(Arc::new(
            TranscriptRecord::from_local(1, 1, LocalEvent::CompactionStarted).unwrap(),
        )));
        assert!(render_root_text(&mut root, 100, 20).contains("Compacting context"));
        assert!(
            root.update(key(KeyCode::Char('x'), KeyModifiers::NONE))
                .effects
                .is_empty()
        );
        assert!(root.composer().draft().is_empty());
        assert!(
            root.update(key(KeyCode::Esc, KeyModifiers::NONE))
                .effects
                .is_empty()
        );
        root.update(super::RootEvent::Transcript(Arc::new(
            TranscriptRecord::from_local(
                2,
                2,
                LocalEvent::CompactionFinished(CompactionFinished {
                    terminal_stop: None,
                    error: error.clone(),
                    duration_ns: 1_000_000,
                }),
            )
            .unwrap(),
        )));
        root.update(super::RootEvent::CompactionFinished);
        assert!(root.blocking_task.is_none());
        let text = render_root_text(&mut root, 100, 20);
        assert!(!text.contains("Compacting context"));
        if let Some(error) = error {
            assert!(text.contains(&error));
        }
        // The provider stream and worker receipts have independent delivery schedules.
        root.update(super::RootEvent::Transcript(agent_record(
            3,
            AgentEventKind::ModelCompactionStarted,
            json!({}),
        )));
        root.update(super::RootEvent::Transcript(agent_record(
            4,
            AgentEventKind::ModelCompactionCompleted,
            json!({"duration_ns": 1}),
        )));
        assert!(!render_root_text(&mut root, 100, 20).contains("Compacting context"));
        assert_eq!(render_root_text(&mut root, 100, 20), text);
        assert_eq!(root.context_diagnostics.compactions_started, 1);
        root.update(super::RootEvent::Transcript(agent_record(
            5,
            AgentEventKind::RunStarted,
            json!({}),
        )));
        root.update(super::RootEvent::Transcript(agent_record(
            6,
            AgentEventKind::ModelCompactionStarted,
            json!({}),
        )));
        assert!(render_root_text(&mut root, 100, 20).contains("Compacting context"));
        assert_eq!(root.context_diagnostics.compactions_started, 2);
    }
}

#[test]
fn manual_compaction_is_unavailable_during_active_work() {
    for active_shell in [true, false] {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        if active_shell {
            root.turns.start_shell();
        } else {
            root.turns.start_turn();
        }
        root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));
        for character in "compact".chars() {
            root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
        }
        assert!(
            root.update(key(KeyCode::Enter, KeyModifiers::NONE))
                .effects
                .is_empty()
        );
        assert!(root.blocking_task.is_none());
    }
}

#[test]
fn reflection_action_collects_hidden_optional_instructions() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));
    for character in "reflection".chars() {
        root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
    }
    root.update(key(KeyCode::Enter, KeyModifiers::NONE));

    assert!(root.reflection_input);
    assert_eq!(root.composer().input_mode(), InputMode::Reflection);
    for character in "Focus on validation gaps.".chars() {
        root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
    }
    let submitted = root.update(key(KeyCode::Enter, KeyModifiers::NONE));

    assert_eq!(
        submitted.effects,
        [RootEffect::Reflect(
            "Focus on validation gaps.".to_owned().into()
        )]
    );
    assert!(!root.reflection_input);
    assert_eq!(root.composer().input_mode(), InputMode::Prompt);
    assert!(root.thread == ThreadState::Started);
    assert_eq!(root.busy().turns, 1);
    assert!(root.composer().draft().is_empty());
    root.update(key(KeyCode::Up, KeyModifiers::NONE));
    assert!(root.composer().draft().is_empty());
}

#[test]
fn reflection_can_start_without_instructions_or_be_cancelled() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));
    for character in "reflection".chars() {
        root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
    }
    root.update(key(KeyCode::Enter, KeyModifiers::NONE));
    root.update(key(KeyCode::Char('x'), KeyModifiers::NONE));

    let cancelled = root.update(key(KeyCode::Esc, KeyModifiers::NONE));
    assert!(cancelled.effects.is_empty());
    assert!(!root.reflection_input);
    assert!(root.composer().draft().is_empty());

    root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));
    for character in "reflection".chars() {
        root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
    }
    root.update(key(KeyCode::Enter, KeyModifiers::NONE));
    let submitted = root.update(key(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        submitted.effects,
        [RootEffect::Reflect("".to_owned().into())]
    );
}

#[test]
fn reload_config_action_closes_the_menu_and_requests_a_reload() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));
    for character in "refresh".chars() {
        root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
    }

    let update = root.update(key(KeyCode::Enter, KeyModifiers::NONE));

    assert!(root.overlay.is_none());
    assert_eq!(update.effects, [RootEffect::ReloadConfig]);
    assert_eq!(update.render, super::RenderRequest::Immediate);
}

#[test]
fn memory_action_loads_inspects_and_deletes_without_submitting_a_prompt() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.set_memory_enabled(true);
    root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));
    for character in "remember".chars() {
        root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
    }

    let opened = root.update(key(KeyCode::Enter, KeyModifiers::NONE));

    assert_eq!(opened.effects, [RootEffect::LoadMemories]);
    assert!(matches!(&root.overlay, Some(Overlay::Memory(_))));
    assert!(root.composer().draft().is_empty());

    root.update(RootEvent::MemoriesLoaded {
        access: MemoryAccess::Local,
        records: vec![memory_record(7, 3, "remember this")],
    });
    let inspected = root.update(key(KeyCode::Enter, KeyModifiers::NONE));
    assert!(inspected.effects.is_empty());
    assert!(render_root_text(&mut root, 80, 28).contains("remember this"));

    root.update(key(KeyCode::Char('d'), KeyModifiers::NONE));
    let deleted = root.update(key(KeyCode::Delete, KeyModifiers::NONE));
    assert_eq!(
        deleted.effects,
        [RootEffect::DeleteMemory(MemoryKey::local(7, 3))]
    );
    assert!(root.composer().draft().is_empty());

    root.update(RootEvent::MemoryDeleted {
        key: MemoryKey::local(7, 3),
    });
    assert!(render_root_text(&mut root, 80, 20).contains("Local memory is empty"));
}

#[test]
fn memory_completions_are_ignored_after_the_browser_closes() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.set_memory_enabled(true);
    root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));
    for character in "memory".chars() {
        root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
    }
    root.update(key(KeyCode::Enter, KeyModifiers::NONE));
    root.update(key(KeyCode::Esc, KeyModifiers::NONE));
    assert!(root.overlay.is_none());

    for event in [
        RootEvent::MemoriesLoaded {
            access: MemoryAccess::Local,
            records: vec![memory_record(1, 1, "stale")],
        },
        RootEvent::MemoryLoadFailed {
            source: MemorySource::Local,
            access: None,
            error: "stale load".to_owned(),
        },
        RootEvent::MemoryDeleted {
            key: MemoryKey::local(1, 1),
        },
        RootEvent::MemoryDeleteFailed {
            error: "stale delete".to_owned(),
            conflict: false,
        },
    ] {
        let update = root.update(event);
        assert!(update.effects.is_empty());
        assert_eq!(update.render, RenderRequest::None);
        assert!(root.overlay.is_none());
    }
}

#[test]
fn memory_availability_survives_reset_and_fork_and_disabling_closes_the_browser() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.set_memory_enabled(true);
    root.reset_session(
        Path::new("/work"),
        ReasoningEffort::Low,
        ReasoningMode::Standard,
        ReasoningMode::Standard,
        DraftReset::Clear,
    );
    let fork = root.fork(Path::new("/work"), ReasoningEffort::Low);
    assert!(root.memory_enabled);
    assert!(fork.memory_enabled);

    root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));
    for character in "memory".chars() {
        root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
    }
    root.update(key(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(&root.overlay, Some(Overlay::Memory(_))));

    root.set_memory_enabled(false);
    assert!(!root.memory_enabled);
    assert!(root.overlay.is_none());
}

#[test]
fn new_session_action_clears_the_completed_thread_after_runtime_replacement() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.set_model(Model::Codex(CodexModel::Luna));
    for character in "old prompt".chars() {
        root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
    }
    root.update(key(KeyCode::Enter, KeyModifiers::NONE));
    root.update(super::RootEvent::WorkerTurnFinished {
        terminal_expected: false,
    });
    root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));
    for character in "clear".chars() {
        root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
    }

    let requested = root.update(key(KeyCode::Enter, KeyModifiers::NONE));

    assert_eq!(
        requested.effects,
        [RootEffect::NewSession(Model::Codex(CodexModel::Luna))]
    );
    assert!(root.overlay.is_none());
    assert!(!root.interactive);

    root.reset_session(
        Path::new("/work"),
        ReasoningEffort::Medium,
        ReasoningMode::Standard,
        ReasoningMode::Standard,
        DraftReset::Clear,
    );

    assert!(root.interactive);
    assert!(matches!(root.thread, ThreadState::New));
    assert!(root.composer().draft().is_empty());
    assert_eq!(root.busy().turns, 0);
}

#[test]
fn new_session_action_is_unavailable_while_work_is_active() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    for character in "active prompt".chars() {
        root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
    }
    root.update(key(KeyCode::Enter, KeyModifiers::NONE));
    root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));
    for character in "clear".chars() {
        root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
    }

    let update = root.update(key(KeyCode::Enter, KeyModifiers::NONE));

    assert!(update.effects.is_empty());
    assert!(matches!(&root.overlay, Some(Overlay::Actions(_))));
}

#[test]
fn effort_action_remains_available_after_the_first_prompt() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.update(key(KeyCode::Char('h'), KeyModifiers::NONE));
    root.update(key(KeyCode::Char('i'), KeyModifiers::NONE));

    let submitted = root.update(key(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        submitted.effects,
        [RootEffect::Submit("hi".to_owned().into())]
    );

    root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));
    for character in "effort".chars() {
        root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
    }
    let update = root.update(key(KeyCode::Enter, KeyModifiers::NONE));

    assert!(update.effects.is_empty());
    assert!(matches!(&root.overlay, Some(Overlay::Effort(_))));
}

#[test]
fn handoff_blocks_input_until_the_continuation_prompt_is_ready() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));
    for character in "handoff".chars() {
        root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
    }

    let started = root.update(key(KeyCode::Enter, KeyModifiers::NONE));
    let typed = root.update(key(KeyCode::Char('x'), KeyModifiers::NONE));
    let submitted = root.update(key(KeyCode::Enter, KeyModifiers::NONE));
    let pasted = root.update(RootEvent::PasteImage(
        "data:image/png;base64,abc".to_owned(),
    ));

    assert_eq!(started.effects, [RootEffect::Handoff]);
    assert!(typed.effects.is_empty());
    assert!(submitted.effects.is_empty());
    assert!(pasted.effects.is_empty());
    assert!(root.composer().draft().is_empty());
    assert!(render_root_text(&mut root, 100, 20).contains("Preparing handoff"));

    root.update(RootEvent::HandoffFinished(
        "Continue by implementing the parser.".to_owned(),
    ));

    assert_eq!(
        root.composer().draft(),
        "Continue by implementing the parser."
    );
    assert!(root.blocking_task.is_none());
}

#[test]
fn escape_cancels_an_active_handoff() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.blocking_task = Some(super::BlockingTask::Handoff);

    let update = root.update(key(KeyCode::Esc, KeyModifiers::NONE));

    assert_eq!(update.effects, [RootEffect::CancelHandoff]);
}

#[test]
fn active_turn_can_fork_from_the_latest_safe_boundary() {
    let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
    root.turns.start_turn();
    root.update(RootEvent::Transcript(agent_record(
        1,
        AgentEventKind::RunStarted,
        json!({}),
    )));

    assert_eq!(
        root.update(key(KeyCode::Char('t'), KeyModifiers::CONTROL))
            .effects,
        [RootEffect::Fork]
    );
}

mod qr_code_overlay {
    use super::{Component, Overlay, RootEvent, RootNode};
    use crate::{app::config::ReasoningEffort, tui::components::qr_code::QrCodeOverlay};
    use std::{path::Path, time::Instant};

    const LINK: &str = "https://laptop.tail1234.ts.net/#k=secret";

    fn root_preparing() -> RootNode {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.overlay = Some(Overlay::QrCode(QrCodeOverlay::preparing(Instant::now())));
        root
    }

    fn preparing(root: &RootNode) -> Option<bool> {
        match &root.overlay {
            Some(Overlay::QrCode(overlay)) => Some(overlay.is_preparing()),
            _ => None,
        }
    }

    #[test]
    fn the_code_replaces_the_waiting_overlay_when_the_link_is_ready() {
        let mut root = root_preparing();
        root.update(RootEvent::ShowQrCode(LINK.to_owned()));
        assert_eq!(preparing(&root), Some(false));
    }

    #[test]
    fn a_link_that_arrives_after_the_overlay_was_closed_is_dropped() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.update(RootEvent::ShowQrCode(LINK.to_owned()));
        assert_eq!(preparing(&root), None);
    }

    #[test]
    fn a_failure_closes_the_waiting_overlay_and_reports_the_error() {
        let mut root = root_preparing();
        root.update(RootEvent::NotifyError(
            "Cannot share over Tailscale".to_owned(),
        ));
        assert_eq!(preparing(&root), None);
        assert!(root.notification.is_some());
    }

    #[test]
    fn the_waiting_overlay_animates_until_it_is_replaced() {
        let mut root = root_preparing();
        assert!(root.animation_deadline().is_some());
        root.update(RootEvent::ShowQrCode(LINK.to_owned()));
        assert!(
            !matches!(&root.overlay, Some(Overlay::QrCode(overlay)) if overlay.animation_deadline().is_some())
        );
    }
}
