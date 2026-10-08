use super::{
    Anchor, Component, ExpandableCommand, RenderRequest, ScrollCommand, Transcript,
    TranscriptEvent, image, render::EntryRenderer, unix_time_ms, viewport::ScrollState,
};
use crate::{
    app::{
        config::{ReasoningEffort, ReasoningMode, Speed, TuiConfig},
        theme::Theme,
    },
    core::transcript::{EntryKind, LocalEvent, SessionStarted, TranscriptRecord, TurnId},
};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use nanocodex::{
    HarnessModel as Model, Model as CodexModel,
    agent::events::{AgentEvent, AgentEventKind},
};
use ratatui::{Terminal, backend::TestBackend, layout::Position};
use serde_json::{json, value::to_raw_value};
use std::{
    fs::File,
    num::NonZeroU16,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use tact_subagents::{AgentMessageUpdate, MessageSender};

#[test]
fn user_messages_normalize_carriage_returns() {
    let layout = EntryRenderer {
        width: 80,
        theme: &Theme::default(),
        workspace: Path::new("/"),
        images: &mut image::Cache::default(),
    }
    .user("one\r\ntwo\rthree");
    let rendered = layout
        .lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();

    assert_eq!(rendered, ["┃ one", "┃ two", "┃ three"]);
    assert_eq!(layout.selection_source.as_deref(), Some("one\ntwo\nthree"));
}

fn user(sequence: u64, text: impl Into<String>) -> Arc<TranscriptRecord> {
    Arc::new(
        TranscriptRecord::from_local(
            sequence,
            sequence,
            LocalEvent::UserSubmitted {
                id: TurnId::new(sequence),
                text: text.into(),
            },
        )
        .unwrap(),
    )
}

fn fork_started(sequence: u64, parent_sequence: u64) -> Arc<TranscriptRecord> {
    Arc::new(
        TranscriptRecord::from_local(
            sequence,
            sequence,
            LocalEvent::SessionStarted(SessionStarted {
                session_id: "fork".to_owned(),
                parent_session_id: Some("parent".to_owned()),
                parent_sequence: Some(parent_sequence),
                model: Model::Codex(CodexModel::Luna).to_string(),
                effort: ReasoningEffort::Medium,
                reasoning_mode: ReasoningMode::Standard,
                speed: Speed::Standard,
                workspace: "/work".into(),
                application_version: "test".to_owned(),
            }),
        )
        .unwrap(),
    )
}

fn agent(sequence: u64, kind: AgentEventKind) -> Arc<TranscriptRecord> {
    agent_with_payload(sequence, kind, json!({}))
}

fn agent_with_payload(
    sequence: u64,
    kind: AgentEventKind,
    payload: serde_json::Value,
) -> Arc<TranscriptRecord> {
    agent_with_payload_at(sequence, sequence, kind, payload)
}

fn agent_with_payload_at(
    sequence: u64,
    recorded_at_unix_ms: u64,
    kind: AgentEventKind,
    payload: serde_json::Value,
) -> Arc<TranscriptRecord> {
    Arc::new(TranscriptRecord::from_agent(
        sequence,
        recorded_at_unix_ms,
        AgentEvent {
            protocol_version: 1,
            request_id: Arc::from("test"),
            seq: sequence,
            kind,
            payload: to_raw_value(&payload).unwrap().into(),
        },
    ))
}

fn shell(transcript: &mut Transcript, sequence: u64, output: &str) {
    transcript.update(TranscriptEvent::Record(agent_with_payload(
        sequence,
        AgentEventKind::ToolCall,
        json!({
            "call_id": format!("call-{sequence}"),
            "tool": "exec_command",
            "arguments": {"cmd": "cargo test", "workdir": "/work"},
        }),
    )));
    transcript.update(TranscriptEvent::Record(agent_with_payload(
        sequence + 1,
        AgentEventKind::ToolResult,
        json!({
            "call_id": format!("call-{sequence}"),
            "tool": "exec_command",
            "status": "completed",
            "duration_ns": 1_200_000_000_u64,
            "result": format!(
                "Wall time: 1.2000 seconds\nProcess exited with code 0\nOutput:\n{output}"
            ),
            "structured_result": {
                "output": output,
                "exit_code": 0,
                "wall_time_seconds": 1.2,
            },
            "metadata": null,
        }),
    )));
}

fn directed_message(transcript: &mut Transcript) {
    let update = serde_json::from_value::<AgentMessageUpdate>(json!({
        "message_id": 1,
        "thread": {
            "id": 1,
            "participants": [
                {"kind": "root"},
                {"kind": "agent", "agent_id": 1}
            ],
            "messages": [{
                "id": 1,
                "thread_id": 1,
                "from": {"kind": "root"},
                "to": 1,
                "priority": "deferred",
                "purpose": "coordinate",
                "body": "verify the ordering"
            }]
        },
        "delivery": {"state": "delivered", "disposition": "started"}
    }))
    .unwrap();
    transcript.update(TranscriptEvent::DirectedMessage {
        perspective: MessageSender::Root,
        update,
    });
}

fn render(transcript: &mut Transcript, width: u16, height: u16) -> TestBackend {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            transcript.render(frame, frame.area(), &Theme::default());
            transcript.render_chrome(frame, frame.area(), &Theme::default());
        })
        .unwrap();
    terminal.backend().clone()
}

fn render_until_image_ready(transcript: &mut Transcript, width: u16, height: u16) -> TestBackend {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let backend = render(transcript, width, height);
        if transcript.cache.first_image().is_some() {
            return backend;
        }
        transcript.update(TranscriptEvent::AnimationFrame(Instant::now()));
        assert!(Instant::now() < deadline, "image preparation timed out");
        std::thread::yield_now();
    }
}

fn write_png(path: &Path) {
    write_png_size(path, 1, 1);
}

fn write_png_size(path: &Path, width: u32, height: u32) {
    let file = File::create(path).unwrap();
    let mut encoder = png::Encoder::new(file, width, height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().unwrap();
    writer
        .write_image_data(&[0xff, 0, 0].repeat((width * height) as usize))
        .unwrap();
}

fn transcript_with_image(workspace: &Path, text: &str) -> Transcript {
    let mut transcript = Transcript::new();
    transcript.cache.set_inline_images(true);
    transcript.set_workspace(workspace);
    transcript.update(TranscriptEvent::Record(agent_with_payload(
        1,
        AgentEventKind::AssistantMessage,
        json!({
            "model_call_index": 1,
            "item_id": "answer",
            "phase": "final_answer",
            "text": text,
        }),
    )));
    transcript
}

fn scroll(transcript: &mut Transcript, event: Event) {
    let command = transcript
        .scroll_command(&event, TuiConfig::default().mouse_scroll_lines)
        .expect("test event should be a transcript scroll command");
    transcript.update(TranscriptEvent::Scroll(command));
}

fn user_image_transcript(workspace: &Path, text: &str, inline: bool) -> Transcript {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let path = workspace.join("user.png");
    write_png(&path);
    let data_url = format!(
        "data:image/png;base64,{}",
        STANDARD.encode(std::fs::read(path).unwrap())
    );
    let mut transcript = Transcript::new();
    transcript.set_workspace(workspace);
    transcript.cache.set_inline_images(inline);
    transcript.update(TranscriptEvent::Record(user(1, text)));
    transcript.update(TranscriptEvent::Record(agent_with_payload(
        2,
        AgentEventKind::InputAccepted,
        json!({
            "input": [{"type": "image", "image_url": data_url}]
        }),
    )));
    transcript
}

#[test]
fn user_images_occupy_their_own_rows_and_preserve_text_selection_offsets() {
    let workspace = tempfile::tempdir().unwrap();
    let text = "before [Image #1] after";
    let mut transcript = user_image_transcript(workspace.path(), text, true);
    render_until_image_ready(&mut transcript, 40, 10);
    let entry = &transcript.model.entries()[0];
    let layout = transcript.cache.cached(entry.id).unwrap();
    let image = &layout.images[0];
    assert_eq!(image.line, 1);
    assert_eq!(layout.lines[0].to_string(), "┃ before ");
    let after = image.line + usize::from(image.protocol.size().height);
    assert_eq!(layout.lines[after].to_string(), "┃  after");
    assert_eq!(layout.selections[0].first().unwrap().source.start, 0);
    assert_eq!(layout.selections[after].first().unwrap().source.start, 17);
    assert_eq!(
        layout.selections[after].last().unwrap().source.end,
        text.len()
    );
    assert_eq!(layout.selections[after].first().unwrap().columns.start, 2);
    assert!(
        layout.selections[image.line..after]
            .iter()
            .all(Vec::is_empty)
    );
}

#[test]
fn user_image_text_offsets_preserve_original_line_endings() {
    let workspace = tempfile::tempdir().unwrap();
    let text = "a\r\nb\rc\r\n[Image #1]\r\nd\re";
    let mut transcript = user_image_transcript(workspace.path(), text, false);
    render(&mut transcript, 40, 10);
    let entry = &transcript.model.entries()[0];
    let layout = transcript.cache.cached(entry.id).unwrap();
    assert_eq!(
        layout
            .lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        ["┃ a", "┃ b", "┃ c", "┃ [Image #1]", "┃ d", "┃ e", ""]
    );
    for (line, expected) in [(0, "a"), (1, "b"), (2, "c"), (4, "d"), (5, "e")] {
        let span = &layout.selections[line][0];
        assert_eq!(&text[span.source.clone()], expected);
    }
}

#[test]
fn marker_only_user_prompt_has_only_image_rows() {
    let workspace = tempfile::tempdir().unwrap();
    let mut transcript = user_image_transcript(workspace.path(), "[Image #1]", true);
    render_until_image_ready(&mut transcript, 40, 10);
    let entry = &transcript.model.entries()[0];
    let layout = transcript.cache.cached(entry.id).unwrap();
    assert_eq!(layout.images[0].line, 0);
    assert_eq!(
        layout.lines.len(),
        usize::from(layout.images[0].protocol.size().height) + 1
    );
}

#[test]
fn unsupported_user_images_keep_markers_on_separate_rows() {
    let workspace = tempfile::tempdir().unwrap();
    let mut transcript = user_image_transcript(workspace.path(), "before [Image #1] after", false);
    render(&mut transcript, 40, 10);
    let entry = &transcript.model.entries()[0];
    let layout = transcript.cache.cached(entry.id).unwrap();
    assert!(layout.images.is_empty());
    assert_eq!(
        layout
            .lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        ["┃ before ", "┃ [Image #1]", "┃  after", ""]
    );
    assert_eq!(
        layout.lines[1].spans[1].style.fg,
        Some(Theme::default().accent())
    );
}

#[test]
fn user_lines_have_a_cyan_gutter_without_outer_chrome() {
    let mut transcript = Transcript::new();
    transcript.update(TranscriptEvent::Record(user(1, "hello\nworld")));

    let backend = render(&mut transcript, 20, 4);

    assert_eq!(backend.buffer()[(0, 1)].symbol(), "┃");
    assert_eq!(backend.buffer()[(2, 1)].symbol(), "h");
    assert_eq!(backend.buffer()[(0, 2)].symbol(), "┃");
    assert_eq!(backend.buffer()[(0, 0)].symbol(), " ");
    assert!(transcript.selection_span(Position::new(0, 0)).is_none());
    assert!(
        transcript
            .selection_span_nearest(Position::new(0, 0))
            .is_some()
    );
}

#[test]
fn assistant_markdown_images_render_between_surrounding_text_rows() {
    let workspace = tempfile::tempdir().unwrap();
    write_png(&workspace.path().join("sample.png"));
    let mut transcript =
        transcript_with_image(workspace.path(), "before ![sample](sample.png) after");

    let backend = render_until_image_ready(&mut transcript, 20, 5);
    let rows = backend
        .buffer()
        .content()
        .chunks(20)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>();
    let before = rows.iter().position(|row| row.contains("before")).unwrap();
    let after = rows.iter().position(|row| row.contains("after")).unwrap();

    assert_eq!(after, before + 2);
    assert!(rows[before + 1].chars().any(|character| character != ' '));
}

#[test]
fn refreshing_terminal_images_preserves_their_dimensions() {
    let workspace = tempfile::tempdir().unwrap();
    write_png_size(&workspace.path().join("sample.png"), 80, 40);
    let mut transcript = transcript_with_image(workspace.path(), "![sample](sample.png)");

    let _ = render_until_image_ready(&mut transcript, 20, 3);
    let original = Arc::clone(transcript.cache.first_image().unwrap());
    let original_size = original.size();
    assert!(original_size.width > 1);
    assert!(original_size.height > 1);

    transcript.refresh_terminal_images();
    let pending = transcript.cache.first_image().unwrap();
    assert!(Arc::ptr_eq(&original, pending));

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let _ = render(&mut transcript, 20, 3);
        transcript.update(TranscriptEvent::AnimationFrame(Instant::now()));
        let retransmitted = transcript.cache.first_image().unwrap();
        if !Arc::ptr_eq(&original, retransmitted) {
            assert_eq!(retransmitted.size(), original_size);
            break;
        }
        assert!(Instant::now() < deadline, "image retransmission timed out");
        std::thread::yield_now();
    }
}

#[test]
fn restored_images_present_a_link_before_native_image_hydration() {
    let workspace = tempfile::tempdir().unwrap();
    write_png(&workspace.path().join("sample.png"));
    let mut transcript =
        transcript_with_image(workspace.path(), "before ![sample](sample.png) after");
    let first = render(&mut transcript, 40, 5);
    let first_text = first
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(first_text.contains("sample ↗ sample.png"));
    assert!(transcript.cache.first_image().is_none());
    assert!(transcript.animation_deadline().is_some());

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut rendered_completion = false;
    while transcript.cache.first_image().is_none() {
        let update = transcript.update(TranscriptEvent::AnimationFrame(Instant::now()));
        rendered_completion |= update.render == RenderRequest::Streaming;
        let _ = render(&mut transcript, 40, 5);
        assert!(Instant::now() < deadline, "image hydration timed out");
        std::thread::yield_now();
    }
    assert!(rendered_completion);

    assert!(transcript.cache.first_image().is_some());
}

#[test]
fn pending_image_hydration_restarts_after_terminal_refresh() {
    let workspace = tempfile::tempdir().unwrap();
    write_png(&workspace.path().join("sample.png"));
    let mut transcript = transcript_with_image(workspace.path(), "![sample](sample.png)");

    let _ = render(&mut transcript, 20, 3);
    transcript.refresh_terminal_images();
    let _ = render_until_image_ready(&mut transcript, 20, 3);

    assert!(transcript.cache.first_image().is_some());
}

#[test]
fn fork_start_renders_its_parent_session_boundary() {
    let mut transcript = Transcript::new();
    transcript.update(TranscriptEvent::Record(fork_started(1, 0)));

    let backend = render(&mut transcript, 40, 3);
    let output = backend
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    assert!(output.contains("Forked from @@parent"));
}

#[test]
fn fork_start_discards_the_active_parent_turn() {
    let mut transcript = Transcript::new();
    transcript.update(TranscriptEvent::Record(user(1, "completed")));
    transcript.update(TranscriptEvent::Record(user(2, "still running")));
    transcript.update(TranscriptEvent::Record(agent(
        3,
        AgentEventKind::RunStarted,
    )));
    transcript.update(TranscriptEvent::Record(fork_started(4, 3)));

    assert!(!transcript.model.is_active());
    assert_eq!(transcript.model.entries().len(), 2);
    assert!(matches!(
        &transcript.model.entries()[0].kind,
        EntryKind::User { text, .. } if text == "completed"
    ));
    assert!(matches!(
        &transcript.model.entries()[1].kind,
        EntryKind::ForkedFrom { session_id } if session_id == "parent"
    ));
}

#[test]
fn user_messages_preserve_internal_code_indentation() {
    let mut transcript = Transcript::new();
    transcript.update(TranscriptEvent::Record(user(
        1,
        "before\n    fn main() {\n        work();\n    }\nafter",
    )));

    let backend = render(&mut transcript, 30, 8);
    let rows = (0..backend.buffer().area.height)
        .map(|row| {
            (0..backend.buffer().area.width)
                .map(|column| backend.buffer()[(column, row)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>();

    assert!(rows.iter().any(|row| row.starts_with("┃     fn main() {")));
    assert!(rows.iter().any(|row| row.starts_with("┃         work();")));
    assert!(rows.iter().any(|row| row.starts_with("┃     }")));
}

#[test]
fn user_soft_wrap_omits_the_separator_space_but_preserves_explicit_indentation() {
    let mut transcript = Transcript::new();
    transcript.update(TranscriptEvent::Record(user(1, "alpha bravo\n bravo")));

    let backend = render(&mut transcript, 12, 5);
    let rows = (0..backend.buffer().area.height)
        .map(|row| {
            (0..backend.buffer().area.width)
                .map(|column| backend.buffer()[(column, row)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>();

    assert!(rows.iter().any(|row| row.starts_with("┃ bravo")));
    assert_eq!(
        rows.iter()
            .filter(|row| row.starts_with("┃  bravo"))
            .count(),
        1
    );
}

#[test]
fn detached_transcript_pins_at_most_three_lines_of_the_previous_prompt() {
    let mut transcript = Transcript::new();
    transcript.update(TranscriptEvent::Record(user(
        1,
        "prompt one\nprompt two\nprompt three\nprompt four\nprompt five",
    )));
    transcript.update(TranscriptEvent::Record(agent_with_payload(
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
    drop(render(&mut transcript, 30, 6));
    let answer = transcript
        .model
        .entries()
        .iter()
        .find(|entry| matches!(entry.kind, EntryKind::Assistant { .. }))
        .unwrap()
        .id;
    transcript.viewport.set_state(ScrollState::Detached(Anchor {
        entry: answer,
        line: 2,
    }));

    let backend = render(&mut transcript, 30, 6);
    let prompt = transcript
        .pinned_prompt
        .expect("the previous prompt should be pinned");
    let rows = backend
        .buffer()
        .content()
        .chunks(30)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>();

    assert_eq!(prompt.area.height, 3);
    assert!(rows[0].contains("prompt one"));
    assert!(rows[1].contains("prompt two"));
    assert!(rows[2].contains("prompt three"));
    assert_eq!(backend.buffer()[(29, 2)].symbol(), "…");
    assert!(rows[3..].iter().any(|row| row.contains("answer")));
}

#[test]
fn pinned_prompt_uses_the_code_block_background() {
    let mut transcript = Transcript::new();
    transcript.update(TranscriptEvent::Record(user(1, "pinned prompt")));
    transcript.update(TranscriptEvent::Record(agent_with_payload(
        2,
        AgentEventKind::AssistantMessage,
        json!({
            "model_call_index": 1,
            "item_id": "answer",
            "phase": "final_answer",
            "text": "response content ".repeat(40),
        }),
    )));
    let answer = transcript
        .model
        .entries()
        .iter()
        .find(|entry| matches!(entry.kind, EntryKind::Assistant { .. }))
        .unwrap()
        .id;
    transcript.viewport.set_state(ScrollState::Detached(Anchor {
        entry: answer,
        line: 2,
    }));

    let backend = render(&mut transcript, 30, 6);
    let area = transcript.pinned_prompt.unwrap().area;

    for row in area.y..area.bottom() {
        for column in area.x..area.right() {
            assert_eq!(
                backend.buffer()[(column, row)].bg,
                Theme::default().code_background()
            );
        }
    }
}

#[test]
fn active_stream_does_not_pin_while_following() {
    let mut transcript = Transcript::new();
    transcript.update(TranscriptEvent::Record(user(1, "streaming prompt")));
    transcript.update(TranscriptEvent::Record(agent(
        2,
        AgentEventKind::RunStarted,
    )));
    transcript.update(TranscriptEvent::Record(agent_with_payload(
        3,
        AgentEventKind::AssistantMessage,
        json!({
            "model_call_index": 1,
            "item_id": "answer",
            "phase": "final_answer",
            "text": "streamed response content ".repeat(40),
        }),
    )));

    drop(render(&mut transcript, 30, 6));

    assert!(matches!(transcript.viewport.state(), ScrollState::Follow));
    assert!(transcript.pinned_prompt.is_none());
}

#[test]
fn scrolling_over_a_pinned_prompt_reveals_it_without_moving_the_transcript() {
    let mut transcript = Transcript::new();
    transcript.update(TranscriptEvent::Record(user(
        1,
        "prompt one\nprompt two\nprompt three\nprompt four\nprompt five",
    )));
    transcript.update(TranscriptEvent::Record(agent_with_payload(
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
    drop(render(&mut transcript, 30, 6));
    let answer = transcript
        .model
        .entries()
        .iter()
        .find(|entry| matches!(entry.kind, EntryKind::Assistant { .. }))
        .unwrap()
        .id;
    transcript.viewport.set_state(ScrollState::Detached(Anchor {
        entry: answer,
        line: 2,
    }));
    drop(render(&mut transcript, 30, 6));
    let transcript_top = transcript.viewport.top();

    for _ in 0..2 {
        scroll(
            &mut transcript,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 5,
                row: 1,
                modifiers: KeyModifiers::NONE,
            }),
        );
        drop(render(&mut transcript, 30, 6));
    }
    let backend = render(&mut transcript, 30, 6);
    let rows = backend
        .buffer()
        .content()
        .chunks(30)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>();

    assert_eq!(transcript.viewport.top(), transcript_top);
    assert!(rows[0].contains("prompt three"));
    assert!(rows[1].contains("prompt four"));
    assert!(rows[2].contains("prompt five"));
    assert_eq!(backend.buffer()[(29, 0)].symbol(), "…");
    assert_ne!(backend.buffer()[(29, 2)].symbol(), "…");
}

#[test]
fn page_navigation_uses_the_unpinned_transcript_height() {
    let mut transcript = Transcript::new();
    transcript.update(TranscriptEvent::Record(user(
        1,
        "prompt one\nprompt two\nprompt three",
    )));
    transcript.update(TranscriptEvent::Record(agent_with_payload(
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
    drop(render(&mut transcript, 30, 6));
    let answer = transcript
        .model
        .entries()
        .iter()
        .find(|entry| matches!(entry.kind, EntryKind::Assistant { .. }))
        .unwrap()
        .id;
    transcript.viewport.set_state(ScrollState::Detached(Anchor {
        entry: answer,
        line: 2,
    }));
    drop(render(&mut transcript, 30, 6));
    assert_eq!(transcript.pinned_prompt.unwrap().area.height, 3);

    let page_up = transcript.scroll_command(
        &Event::Key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)),
        TuiConfig::default().mouse_scroll_lines,
    );
    let page_down = transcript.scroll_command(
        &Event::Key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE)),
        TuiConfig::default().mouse_scroll_lines,
    );

    assert!(matches!(page_up, Some(ScrollCommand::Rows(-1))));
    assert!(matches!(page_down, Some(ScrollCommand::Rows(1))));
}

#[test]
fn clicking_a_pinned_prompt_jumps_to_it_in_the_transcript() {
    let mut transcript = Transcript::new();
    transcript.update(TranscriptEvent::Record(user(1, "pinned prompt")));
    transcript.update(TranscriptEvent::Record(agent_with_payload(
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
    let prompt = transcript.model.entries()[0].id;
    let answer = transcript.model.entries()[1].id;
    transcript.viewport.set_state(ScrollState::Detached(Anchor {
        entry: answer,
        line: 2,
    }));
    drop(render(&mut transcript, 30, 6));

    let click = Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 5,
        row: 0,
        modifiers: KeyModifiers::NONE,
    });
    assert!(transcript.pinned_prompt_clicked(&click));
    transcript.update(TranscriptEvent::JumpToPinnedPrompt);
    let backend = render(&mut transcript, 30, 6);

    assert_eq!(
        transcript.viewport.top(),
        Some(Anchor {
            entry: prompt,
            line: 0
        })
    );
    assert!(transcript.pinned_prompt.is_none());
    assert!(
        backend
            .buffer()
            .content()
            .chunks(30)
            .next()
            .unwrap()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
            .contains("pinned prompt")
    );
}

#[test]
fn pinned_prompt_tracks_the_turn_at_the_top_of_the_viewport() {
    let mut transcript = Transcript::new();
    for turn in 1..=2 {
        transcript.update(TranscriptEvent::Record(user(
            turn * 2 - 1,
            format!("prompt {turn}"),
        )));
        transcript.update(TranscriptEvent::Record(agent_with_payload(
            turn * 2,
            AgentEventKind::AssistantMessage,
            json!({
                "model_call_index": turn,
                "item_id": format!("answer-{turn}"),
                "phase": "final_answer",
                "text": (1..=40)
                    .map(|line| format!("turn {turn} answer {line}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            }),
        )));
    }
    let users = transcript
        .model
        .entries()
        .iter()
        .filter(|entry| matches!(entry.kind, EntryKind::User { .. }))
        .map(|entry| entry.id)
        .collect::<Vec<_>>();
    let answers = transcript
        .model
        .entries()
        .iter()
        .filter(|entry| matches!(entry.kind, EntryKind::Assistant { .. }))
        .map(|entry| entry.id)
        .collect::<Vec<_>>();

    for (prompt, answer) in users.into_iter().zip(answers) {
        transcript.viewport.set_state(ScrollState::Detached(Anchor {
            entry: answer,
            line: 2,
        }));
        drop(render(&mut transcript, 30, 6));

        assert_eq!(
            transcript.pinned_prompt.map(|pinned| pinned.entry),
            Some(prompt)
        );
    }
}

#[test]
fn prompt_is_not_pinned_above_another_visible_prompt() {
    let mut transcript = Transcript::new();
    transcript.update(TranscriptEvent::Record(user(1, "first prompt")));
    transcript.update(TranscriptEvent::Record(user(2, "second prompt")));
    transcript.update(TranscriptEvent::Record(agent_with_payload(
        3,
        AgentEventKind::AssistantMessage,
        json!({
            "model_call_index": 1,
            "item_id": "answer",
            "phase": "final_answer",
            "text": "a sufficiently long response ".repeat(20),
        }),
    )));
    let second_prompt = transcript
        .model
        .entries()
        .iter()
        .filter(|entry| matches!(entry.kind, EntryKind::User { .. }))
        .nth(1)
        .unwrap()
        .id;
    transcript.viewport.set_state(ScrollState::Detached(Anchor {
        entry: second_prompt,
        line: 0,
    }));

    drop(render(&mut transcript, 30, 2));

    assert!(transcript.pinned_prompt.is_none());
}

#[test]
fn active_selection_can_cross_an_unselectable_tool() {
    let mut transcript = Transcript::new();
    transcript.update(TranscriptEvent::Record(user(1, "before")));
    shell(&mut transcript, 2, "output");
    transcript.update(TranscriptEvent::Record(user(4, "after")));

    let backend = render(&mut transcript, 40, 12);
    let tool_row = (0..backend.buffer().area.height)
        .find(|&row| {
            (0..backend.buffer().area.width)
                .map(|column| backend.buffer()[(column, row)].symbol())
                .collect::<String>()
                .contains("Shell")
        })
        .expect("tool summary should be visible");
    let position = Position::new(0, tool_row);

    assert!(transcript.selection_span(position).is_none());
    assert!(transcript.selection_span_nearest(position).is_some());
}

#[test]
fn expanded_tool_details_are_selectable_but_the_summary_remains_clickable() {
    let mut transcript = Transcript::new();
    shell(&mut transcript, 1, "selectable output");
    drop(render(&mut transcript, 60, 12));
    transcript.focus_expandables();
    transcript.update(TranscriptEvent::Expandable(ExpandableCommand::Toggle));
    let backend = render(&mut transcript, 60, 12);
    let row_containing = |text: &str| {
        (0..backend.buffer().area.height)
            .find(|&row| {
                (0..backend.buffer().area.width)
                    .map(|column| backend.buffer()[(column, row)].symbol())
                    .collect::<String>()
                    .contains(text)
            })
            .unwrap()
    };
    let summary = Position::new(10, row_containing("Shell"));
    let output = Position::new(10, row_containing("selectable output"));

    assert!(transcript.selection_span(summary).is_none());
    assert!(transcript.selection_span(output).is_some());
}

#[test]
fn completed_turn_is_rendered_like_other_transcript_milestones() {
    let mut transcript = Transcript::new();
    transcript.update(TranscriptEvent::Record(agent_with_payload_at(
        1,
        1_000,
        AgentEventKind::RunStarted,
        json!({}),
    )));
    transcript.update(TranscriptEvent::Record(agent_with_payload_at(
        2,
        66_432,
        AgentEventKind::RunCompleted,
        json!({"duration_ns": 65_432_000_000_u64}),
    )));

    let rendered = render(&mut transcript, 60, 4)
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("◇ Turn completed · 1m 5s"));
}

#[test]
fn reflection_start_renders_as_a_marker() {
    let mut transcript = Transcript::new();
    transcript.update(TranscriptEvent::Record(Arc::new(
        TranscriptRecord::from_local(1, 1, LocalEvent::ReflectionStarted { id: TurnId::new(1) })
            .unwrap(),
    )));

    let rendered = render(&mut transcript, 60, 4)
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("◇ Reflection started"));
}

#[test]
fn session_setting_notifications_are_distinct_and_styled() {
    let mut transcript = Transcript::new();
    transcript.update(TranscriptEvent::Record(Arc::new(
        TranscriptRecord::from_local(
            1,
            1,
            LocalEvent::EffortChanged {
                from: ReasoningEffort::Medium,
                to: ReasoningEffort::High,
            },
        )
        .unwrap(),
    )));
    transcript.update(TranscriptEvent::Record(Arc::new(
        TranscriptRecord::from_local(
            2,
            2,
            LocalEvent::SpeedChanged {
                from: Speed::Standard,
                to: Speed::Fast,
            },
        )
        .unwrap(),
    )));

    let backend = render(&mut transcript, 72, 6);
    let cells = backend.buffer().content();
    let rendered = cells.iter().map(|cell| cell.symbol()).collect::<String>();
    let fast = cells
        .windows(4)
        .find(|cells| {
            cells
                .iter()
                .map(|cell| cell.symbol())
                .eq(["f", "a", "s", "t"])
        })
        .expect("speed notification should include its value");
    let high = cells
        .windows(4)
        .find(|cells| {
            cells
                .iter()
                .map(|cell| cell.symbol())
                .eq(["h", "i", "g", "h"])
        })
        .expect("effort notification should include its value");

    assert!(rendered.contains("Effort changed to high · takes effect on the next turn"));
    assert!(rendered.contains("Speed changed to fast · takes effect on the next turn"));
    assert_eq!(fast[0].fg, Theme::default().speed(Speed::Fast));
    assert_eq!(high[0].fg, Theme::default().effort(ReasoningEffort::High));
}

#[test]
fn commentary_and_reasoning_render_their_content_without_labels() {
    let mut transcript = Transcript::new();
    transcript.update(TranscriptEvent::Record(agent_with_payload(
        1,
        AgentEventKind::AssistantMessage,
        json!({
            "model_call_index": 1,
            "item_id": "commentary",
            "phase": "commentary",
            "text": "commentary body",
        }),
    )));
    transcript.update(TranscriptEvent::Record(agent_with_payload(
        2,
        AgentEventKind::ReasoningSummaryDelta,
        json!({"model_call_index": 1, "text": "reasoning body"}),
    )));

    let backend = render(&mut transcript, 40, 8);
    let rendered = backend
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    assert!(rendered.contains("commentary body"));
    assert!(rendered.contains("reasoning body"));
    assert!(!rendered.contains("Commentary"));
    assert!(!rendered.contains("Thinking"));
}

#[test]
fn adjacent_bold_reasoning_steps_render_on_separate_rows() {
    let mut transcript = Transcript::new();
    for (sequence, text) in [(1, "**Planning retrieval**"), (2, "**Confirming output**")] {
        transcript.update(TranscriptEvent::Record(agent_with_payload(
            sequence,
            AgentEventKind::ReasoningSummaryDelta,
            json!({"model_call_index": 1, "text": text}),
        )));
    }

    let backend = render(&mut transcript, 40, 6);
    let rows = backend
        .buffer()
        .content()
        .chunks(40)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>();
    let planning = rows
        .iter()
        .position(|row| row.contains("Planning retrieval"))
        .expect("first reasoning step should render");
    let confirming = rows
        .iter()
        .position(|row| row.contains("Confirming output"))
        .expect("second reasoning step should render");

    assert_ne!(planning, confirming);
    assert!(rows.iter().all(|row| !row.contains("****")));
}

#[test]
fn empty_logo_is_replaced_as_soon_as_transcript_content_arrives() {
    let mut transcript = Transcript::new();

    let empty = render(&mut transcript, 41, 14);
    assert_ne!(empty.buffer()[(5, 2)].symbol(), " ");
    let deadline = transcript
        .animation_deadline()
        .expect("empty transcript should schedule the logo");
    assert_eq!(
        transcript
            .update(TranscriptEvent::AnimationFrame(deadline))
            .render,
        RenderRequest::Streaming
    );

    transcript.update(TranscriptEvent::Record(user(1, "hello")));
    let populated = render(&mut transcript, 41, 14);
    assert_eq!(populated.buffer()[(5, 2)].symbol(), " ");
    assert!(transcript.animation_deadline().is_none());
}

#[test]
fn retry_status_keeps_scheduled_delay_during_attempt() {
    for delay_ns in [0, 200_000_000, 2_000_000_000_u64] {
        let mut transcript = Transcript::new();
        transcript.update(TranscriptEvent::Record(user(1, "hello")));
        transcript.update(TranscriptEvent::Record(agent(
            2,
            AgentEventKind::RunStarted,
        )));
        transcript.update(TranscriptEvent::Record(agent_with_payload(
            3,
            AgentEventKind::ModelAttemptRetrying,
            json!({"delay_ns": delay_ns, "attempt": 1, "next_attempt": 2, "max_attempts": 5, "error": "temporary"}),
        )));
        let initial = transcript.activity();
        let now = Instant::now();
        for elapsed in [Duration::from_millis(100), Duration::from_secs(3)] {
            let update = transcript.update(TranscriptEvent::AnimationFrame(now + elapsed));
            assert_eq!(transcript.activity(), initial);
            assert!(update.effects.is_empty());
        }
        assert!(transcript.animation_deadline().is_none());
    }
}

#[test]
fn retry_status_shows_next_attempt_and_updates_from_events() {
    for terminal in [
        AgentEventKind::ModelCallCompleted,
        AgentEventKind::RunFailed,
    ] {
        let mut transcript = Transcript::new();
        transcript.update(TranscriptEvent::Record(user(1, "hello")));
        transcript.update(TranscriptEvent::Record(agent(
            2,
            AgentEventKind::RunStarted,
        )));
        transcript.update(TranscriptEvent::Record(agent_with_payload(
            3,
            AgentEventKind::ModelAttemptRetrying,
            json!({"delay_ns": 2_000_000_000_u64, "attempt": 1, "next_attempt": 2, "max_attempts": 5, "error": "temporary"}),
        )));
        assert_eq!(
            transcript.activity().status.as_deref(),
            Some("Retrying in 2.0s (attempt 2/5)…")
        );
        transcript.update(TranscriptEvent::Record(agent_with_payload(
            4,
            AgentEventKind::ModelAttemptRetrying,
            json!({"delay_ns": 400_000_000, "attempt": 2, "next_attempt": 3, "max_attempts": 5, "error": "temporary"}),
        )));
        assert_eq!(
            transcript.activity().status.as_deref(),
            Some("Retrying in 400ms (attempt 3/5)…")
        );
        transcript.update(TranscriptEvent::Record(agent(5, terminal)));
        assert_eq!(
            transcript.activity().status.as_deref(),
            (terminal == AgentEventKind::ModelCallCompleted).then_some("Thinking…")
        );
        assert!(transcript.animation_deadline().is_none());
    }
}

#[test]
fn tool_focus_and_expansion_are_inline() {
    let mut transcript = Transcript::new();
    shell(&mut transcript, 1, "all tests passed");
    let collapsed = render(&mut transcript, 60, 8);
    let collapsed = collapsed
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(collapsed.contains("▶"));
    assert!(!collapsed.contains("all tests passed"));

    transcript.focus_expandables();
    let focused = render(&mut transcript, 60, 8);
    assert!(
        focused
            .buffer()
            .content()
            .iter()
            .any(|cell| cell.symbol() == "›")
    );

    transcript.update(TranscriptEvent::Expandable(ExpandableCommand::Toggle));
    let expanded = render(&mut transcript, 60, 8);
    let expanded = expanded
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(expanded.contains("▼"));
    assert!(expanded.contains("all tests passed"));

    transcript.update(TranscriptEvent::BlurExpandables);
    assert!(!transcript.expandables_focused());
    let blurred = render(&mut transcript, 60, 8);
    assert!(
        blurred
            .buffer()
            .content()
            .iter()
            .all(|cell| cell.symbol() != "›")
    );
}

#[test]
fn focus_navigation_moves_between_tools_and_message_threads() {
    let mut transcript = Transcript::new();
    shell(&mut transcript, 1, "done");
    directed_message(&mut transcript);
    drop(render(&mut transcript, 80, 12));

    transcript.focus_expandables();
    let message = transcript.selected_expandable.unwrap();
    assert!(matches!(
        transcript.model.entry(message).unwrap().kind,
        EntryKind::DirectedMessage(_)
    ));

    transcript.update(TranscriptEvent::Expandable(ExpandableCommand::Previous));
    let tool = transcript.selected_expandable.unwrap();
    assert!(matches!(
        transcript.model.entry(tool).unwrap().kind,
        EntryKind::Tool(_)
    ));

    transcript.update(TranscriptEvent::Expandable(ExpandableCommand::Next));
    assert_eq!(transcript.selected_expandable, Some(message));
}

#[test]
fn expand_all_includes_directed_message_threads() {
    let mut transcript = Transcript::new();
    directed_message(&mut transcript);

    let collapsed = render(&mut transcript, 80, 10)
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(!collapsed.contains("thread #1 · 1 message"));

    transcript.update(TranscriptEvent::ToggleExpandAll);
    let expanded = render(&mut transcript, 80, 10)
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(expanded.contains("thread #1 · 1 message"));
}

#[test]
fn expand_all_toggles_every_tool_and_applies_to_future_entries() {
    let mut transcript = Transcript::new();
    shell(&mut transcript, 1, "first output");

    transcript.update(TranscriptEvent::ToggleExpandAll);
    shell(&mut transcript, 3, "future output");
    let expanded = render(&mut transcript, 80, 16)
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(expanded.contains("first output"));
    assert!(expanded.contains("future output"));

    transcript.update(TranscriptEvent::ToggleExpandAll);
    let collapsed = render(&mut transcript, 80, 16)
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(!collapsed.contains("first output"));
    assert!(!collapsed.contains("future output"));
    assert_eq!(collapsed.matches('▶').count(), 2);
}

#[test]
fn plan_tools_are_expanded_by_default() {
    let mut transcript = Transcript::new();
    transcript.update(TranscriptEvent::Record(agent_with_payload(
        1,
        AgentEventKind::ToolCall,
        json!({
            "call_id": "plan-1",
            "tool": "update_plan",
            "arguments": {
                "explanation": "Implementation plan",
                "plan": [
                    {"step": "Write the regression test", "status": "completed"},
                    {"step": "Change the default", "status": "in_progress"},
                ],
            },
        }),
    )));

    let backend = render(&mut transcript, 80, 10);
    let rendered = backend
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    assert!(rendered.contains("▼"));
    assert!(rendered.contains("Implementation plan"));
    assert!(rendered.contains("Write the regression test"));
    assert!(rendered.contains("Change the default"));
}

#[test]
fn clicking_a_tool_summary_focuses_and_expands_it() {
    let mut transcript = Transcript::new();
    shell(&mut transcript, 1, "done");
    drop(render(&mut transcript, 60, 8));
    let row = transcript.hits.expandable_rows()[0];
    let event = Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 10,
        row,
        modifiers: KeyModifiers::NONE,
    });
    let command = transcript.expandable_command(&event).unwrap();

    transcript.update(TranscriptEvent::Expandable(command));
    let backend = render(&mut transcript, 60, 8);
    let rendered = backend
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    assert!(transcript.expandables_focused());
    assert!(rendered.contains("▼"));
    assert!(rendered.contains("done"));
    assert!(rendered.contains("↑↓ item · Enter toggle · Esc back"));
}

#[test]
fn clicking_a_wrapped_markdown_link_returns_its_destination() {
    let mut transcript = Transcript::new();
    transcript.update(TranscriptEvent::Record(agent_with_payload(
        1,
        AgentEventKind::AssistantMessage,
        json!({
            "model_call_index": 1,
            "item_id": "answer",
            "phase": "final_answer",
            "text": "[a long local filename](/work/src/main.rs:12)",
        }),
    )));
    drop(render(&mut transcript, 12, 8));
    let start = transcript
        .hits
        .link_start("/work/src/main.rs:12")
        .expect("rendered link should have a hit region");
    let event = Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: start.x,
        row: start.y,
        modifiers: KeyModifiers::NONE,
    });

    assert_eq!(
        transcript.link_destination(&event).as_deref(),
        Some("/work/src/main.rs:12")
    );
}

#[test]
fn expanding_a_visible_tool_preserves_its_summary_row() {
    let mut transcript = Transcript::new();
    for sequence in 1..=10 {
        transcript.update(TranscriptEvent::Record(user(
            sequence,
            format!("before {sequence}"),
        )));
    }
    shell(&mut transcript, 11, "one\ntwo\nthree");
    for sequence in 13..=18 {
        transcript.update(TranscriptEvent::Record(user(
            sequence,
            format!("after {sequence}"),
        )));
    }
    transcript.viewport.set_state(ScrollState::Detached(Anchor {
        entry: transcript.model.entries()[8].id,
        line: 0,
    }));
    drop(render(&mut transcript, 60, 10));
    transcript.focus_expandables();
    drop(render(&mut transcript, 60, 10));
    let before = transcript.hits.expandable_rows()[0];

    transcript.update(TranscriptEvent::Expandable(ExpandableCommand::Toggle));
    drop(render(&mut transcript, 60, 10));

    assert_eq!(transcript.hits.expandable_rows()[0], before);
}

#[test]
fn mouse_scroll_uses_configured_rows_in_both_directions() {
    let transcript = Transcript::new();
    for lines in [1, 3, 8, u16::MAX] {
        for (kind, direction) in [
            (MouseEventKind::ScrollUp, -1),
            (MouseEventKind::ScrollDown, 1),
        ] {
            let event = Event::Mouse(MouseEvent {
                kind,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            });
            let command = transcript.scroll_command(&event, NonZeroU16::new(lines).unwrap());
            assert!(
                matches!(command, Some(ScrollCommand::Rows(rows)) if rows == direction * i32::from(lines))
            );
        }
    }
}

#[test]
fn page_and_mouse_scrolling_detach_then_return_to_tail() {
    let mut transcript = Transcript::new();
    for sequence in 1..=20 {
        transcript.update(TranscriptEvent::Record(user(
            sequence,
            format!("line {sequence}"),
        )));
    }
    drop(render(&mut transcript, 30, 6));

    scroll(
        &mut transcript,
        Event::Key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)),
    );
    let backend = render(&mut transcript, 30, 6);
    let scrolled = backend
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(!scrolled.contains("line 20"));

    scroll(
        &mut transcript,
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }),
    );
    scroll(
        &mut transcript,
        Event::Key(KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL)),
    );
    let backend = render(&mut transcript, 30, 6);
    let tail = backend
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(tail.contains("line 20"));
}

#[test]
fn scrolling_down_near_the_tail_keeps_the_viewport_filled() {
    let mut transcript = Transcript::new();
    for sequence in 1..=2 {
        transcript.update(TranscriptEvent::Record(user(
            sequence,
            format!("line {sequence}"),
        )));
    }
    drop(render(&mut transcript, 30, 6));
    scroll(
        &mut transcript,
        Event::Key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)),
    );
    drop(render(&mut transcript, 30, 6));

    scroll(
        &mut transcript,
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }),
    );
    let backend = render(&mut transcript, 30, 6);
    let rendered = backend
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    assert!(rendered.contains("line 1"));
    assert!(rendered.contains("line 2"));
}

#[test]
fn incoming_records_follow_when_the_viewport_is_at_the_bottom() {
    let mut transcript = Transcript::new();
    for sequence in 1..=20 {
        transcript.update(TranscriptEvent::Record(user(
            sequence,
            format!("line {sequence}"),
        )));
    }
    drop(render(&mut transcript, 30, 6));
    transcript.update(TranscriptEvent::Scroll(ScrollCommand::Rows(-4)));
    drop(render(&mut transcript, 30, 6));
    transcript.update(TranscriptEvent::Scroll(ScrollCommand::Rows(4)));
    let bottom = render(&mut transcript, 30, 6);
    assert!(matches!(transcript.viewport.state(), ScrollState::Follow));
    let bottom = bottom
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(bottom.contains("line 20"));

    transcript.update(TranscriptEvent::Record(user(21, "new tail")));
    let backend = render(&mut transcript, 30, 6);
    let rendered = backend
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    assert!(rendered.contains("new tail"));
}

#[test]
fn incoming_records_do_not_move_a_detached_viewport() {
    let mut transcript = Transcript::new();
    for sequence in 1..=20 {
        transcript.update(TranscriptEvent::Record(user(
            sequence,
            format!("line {sequence}"),
        )));
    }
    drop(render(&mut transcript, 30, 6));
    transcript.update(TranscriptEvent::Scroll(ScrollCommand::Rows(-4)));
    drop(render(&mut transcript, 30, 6));

    transcript.update(TranscriptEvent::Record(user(21, "new tail")));
    let backend = render(&mut transcript, 30, 6);
    let rendered = backend
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    assert!(matches!(
        transcript.viewport.state(),
        ScrollState::Detached(_)
    ));
    assert!(!rendered.contains("new tail"));
    assert!(rendered.contains("1 update"));
}

#[test]
fn generic_activity_is_only_rendered_by_the_composer() {
    let mut transcript = Transcript::new();
    transcript.update(TranscriptEvent::Record(agent(
        1,
        AgentEventKind::RunStarted,
    )));

    let backend = render(&mut transcript, 30, 4);
    let rendered = backend
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.chars().any(|character| character != ' '));
    assert!(!rendered.contains("Thinking"));
}

#[test]
fn active_tool_keeps_its_inline_spinner_without_a_status_row() {
    let mut transcript = Transcript::new();
    let update = transcript.update(TranscriptEvent::Record(agent_with_payload(
        1,
        AgentEventKind::ToolCall,
        json!({
            "call_id": "call-1",
            "tool": "exec_command",
            "arguments": {"cmd": "cargo test", "workdir": "/work"},
        }),
    )));

    assert_eq!(update.effects.len(), 1);
    assert_eq!(
        update.effects[0].status.as_deref(),
        Some("Running exec command…")
    );

    let backend = render(&mut transcript, 30, 4);
    let rendered = backend
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    assert!(
        backend
            .buffer()
            .content()
            .iter()
            .any(|cell| cell.symbol() == "⠋")
    );
    assert!(!rendered.contains("Running exec"));
}

#[test]
fn active_tool_uses_a_monotonic_timer_until_the_reported_duration_arrives() {
    let mut transcript = Transcript::new();
    transcript.update(TranscriptEvent::Record(agent_with_payload_at(
        1,
        unix_time_ms().saturating_sub(10_000),
        AgentEventKind::ToolCall,
        json!({
            "call_id": "call-1",
            "tool": "exec_command",
            "arguments": {"cmd": "cargo test", "workdir": "/work"},
        }),
    )));
    let timer = *transcript
        .running_tool_timers
        .values()
        .next()
        .expect("running tool should have a timer");

    let initial = render(&mut transcript, 50, 4)
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(initial.contains("10.0s"));

    let update = transcript.update(TranscriptEvent::AnimationFrame(
        timer.observed_at + Duration::from_millis(1_234),
    ));
    assert_eq!(update.render, super::RenderRequest::Streaming);
    let running = render(&mut transcript, 50, 4)
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(running.contains("11.2s"));

    transcript.update(TranscriptEvent::Record(agent_with_payload(
        2,
        AgentEventKind::ToolResult,
        json!({
            "call_id": "call-1",
            "tool": "exec_command",
            "status": "completed",
            "duration_ns": 2_500_000_000_u64,
            "result": "Wall time: 2.5000 seconds\nProcess exited with code 0\nOutput:\ndone",
            "structured_result": {
                "output": "done",
                "exit_code": 0,
                "wall_time_seconds": 2.5,
            },
            "metadata": null,
        }),
    )));
    assert!(transcript.running_tool_timers.is_empty());
    let completed = render(&mut transcript, 50, 4)
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(completed.contains("2.5s"));
}

#[test]
fn single_code_workflow_child_renders_as_a_standalone_expandable_tool() {
    let mut transcript = Transcript::new();
    transcript.update(TranscriptEvent::Record(agent_with_payload(
        1,
        AgentEventKind::ToolCall,
        json!({
            "call_id": "workflow",
            "tool": "exec",
            "arguments": "await tools.exec_command({cmd: 'cargo test'})",
        }),
    )));
    transcript.update(TranscriptEvent::Record(agent_with_payload(
        2,
        AgentEventKind::ToolCall,
        json!({
            "call_id": "workflow/code-1",
            "tool": "exec_command",
            "arguments": {"cmd": "cargo test"},
        }),
    )));

    let backend = render(&mut transcript, 80, 5);
    let rows = backend
        .buffer()
        .content()
        .chunks(80)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>();
    let running = rows.join("");
    assert!(!running.contains("Batch"));
    assert!(running.contains("Shell"));
    let child_row = rows.iter().position(|row| row.contains("Shell")).unwrap();
    assert!(rows[child_row + 1].trim().is_empty());

    transcript.update(TranscriptEvent::Record(agent_with_payload(
        3,
        AgentEventKind::ToolResult,
        json!({
            "call_id": "workflow/code-1",
            "tool": "exec_command",
            "status": "completed",
            "duration_ns": 10_u64,
            "result": "Wall time: 0.0000 seconds\nProcess exited with code 0\nOutput:\nok",
            "structured_result": {
                "output": "ok",
                "exit_code": 0,
                "wall_time_seconds": 0.0,
            },
            "metadata": null,
        }),
    )));
    let completed = render(&mut transcript, 80, 5)
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    assert!(completed.contains("▶ ✓ Shell"));
    assert!(!completed.contains("├─"));
    assert_eq!(transcript.model.entries().len(), 2);

    transcript.focus_expandables();
    transcript.update(TranscriptEvent::Expandable(ExpandableCommand::Toggle));
    let expanded = render(&mut transcript, 80, 8)
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(expanded.contains("ok"));
}

#[test]
fn code_workflow_promotes_a_standalone_tool_when_the_second_child_arrives() {
    let mut transcript = Transcript::new();
    transcript.update(TranscriptEvent::Record(agent_with_payload(
        1,
        AgentEventKind::ToolCall,
        json!({
            "call_id": "workflow",
            "tool": "exec",
            "arguments": "await tools.exec_command({cmd: 'cargo test'})",
        }),
    )));
    transcript.update(TranscriptEvent::Record(agent_with_payload(
        2,
        AgentEventKind::ToolCall,
        json!({
            "call_id": "workflow/code-1",
            "tool": "exec_command",
            "arguments": {"cmd": "cargo test"},
        }),
    )));

    let single = render(&mut transcript, 80, 5)
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(single.contains("Shell"));
    assert!(!single.contains("Batch"));
    assert!(!single.contains("├─"));

    transcript.update(TranscriptEvent::Record(agent_with_payload(
        3,
        AgentEventKind::ToolCall,
        json!({
            "call_id": "workflow/code-2",
            "tool": "memory",
            "arguments": {"operation": "scan", "query": "test"},
        }),
    )));

    let backend = render(&mut transcript, 80, 8);
    let rows = backend
        .buffer()
        .content()
        .chunks(80)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>();
    let batch = rows.join("");
    assert!(batch.contains("Batch"));
    assert_eq!(batch.matches("├─").count(), 1);
    assert_eq!(batch.matches("└─").count(), 1);
    let batch_row = rows.iter().position(|row| row.contains("Batch")).unwrap();
    assert!(rows[batch_row + 1].contains("├─"));
    assert!(rows[batch_row + 2].contains("└─"));
    assert!(rows[batch_row + 3].trim().is_empty());
}

#[test]
fn final_multiline_workflow_child_connects_through_its_last_row() {
    let mut transcript = Transcript::new();
    transcript.update(TranscriptEvent::Record(agent_with_payload(
        1,
        AgentEventKind::ToolCall,
        json!({
            "call_id": "workflow",
            "tool": "exec",
            "arguments": "await tools.memory({operation: 'scan'}); await tools.exec_command({cmd: 'cargo check'})",
        }),
    )));
    transcript.update(TranscriptEvent::Record(agent_with_payload(
        2,
        AgentEventKind::ToolCall,
        json!({
            "call_id": "workflow/code-1",
            "tool": "memory",
            "arguments": {"operation": "scan", "query": "transcript"},
        }),
    )));
    transcript.update(TranscriptEvent::Record(agent_with_payload(
        3,
        AgentEventKind::ToolCall,
        json!({
            "call_id": "workflow/code-2",
            "tool": "custom_operation",
            "arguments": {"prompt": "inspect every crate in the workspace"},
        }),
    )));
    transcript.update(TranscriptEvent::Record(agent_with_payload(
        4,
        AgentEventKind::ToolResult,
        json!({
            "call_id": "workflow/code-2",
            "tool": "custom_operation",
            "status": "failed",
            "duration_ns": 1_200_000_000_u64,
            "result": "error[E0277]: the size for values of type `Self` cannot be known at compilation time",
            "structured_result": "error[E0277]: the size for values of type `Self` cannot be known at compilation time",
            "metadata": null,
        }),
    )));

    let backend = render(&mut transcript, 60, 8);
    let rows = backend
        .buffer()
        .content()
        .chunks(60)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>();
    let tool_row = rows
        .iter()
        .position(|row| row.contains("Custom operation"))
        .unwrap();
    let spacer = rows[tool_row..]
        .iter()
        .position(|row| row.trim().is_empty())
        .map(|offset| tool_row + offset)
        .unwrap();
    let final_tool_row = spacer - 1;

    assert!(rows[tool_row].starts_with("  ├─"));
    assert!(
        rows[tool_row + 1..final_tool_row]
            .iter()
            .all(|row| row.starts_with("  │ "))
    );
    assert!(rows[final_tool_row].starts_with("  └─"));
}

#[test]
fn code_workflow_children_remain_visible_at_narrow_widths() {
    let mut transcript = Transcript::new();
    transcript.update(TranscriptEvent::Record(agent_with_payload(
        1,
        AgentEventKind::ToolCall,
        json!({
            "call_id": "workflow",
            "tool": "exec",
            "arguments": "await tools.exec_command({cmd: 'pwd'})",
        }),
    )));
    transcript.update(TranscriptEvent::Record(agent_with_payload(
        2,
        AgentEventKind::ToolCall,
        json!({
            "call_id": "workflow/code-1",
            "tool": "exec_command",
            "arguments": {"cmd": "pwd"},
        }),
    )));
    let child = transcript.model.entries()[1].clone();

    for width in 1..=8 {
        let lines = transcript.cache.layout(&child, width, &Theme::default());
        assert!(!lines.is_empty());
        assert!(lines[0].width() > 0);
        assert!(lines.iter().all(|line| line.width() <= usize::from(width)));
    }
}

#[test]
fn live_timer_rebuilds_only_the_cached_summary_of_an_expanded_tool() {
    let mut transcript = Transcript::new();
    let recorded_at = unix_time_ms();
    transcript.update(TranscriptEvent::Record(agent_with_payload_at(
        1,
        recorded_at,
        AgentEventKind::ToolCall,
        json!({
            "call_id": "shell",
            "tool": "exec_command",
            "arguments": {"cmd": "cargo test", "workdir": "/work"},
        }),
    )));
    transcript.update(TranscriptEvent::Record(agent_with_payload_at(
        2,
        recorded_at,
        AgentEventKind::ToolResult,
        json!({
            "call_id": "shell",
            "tool": "exec_command",
            "status": "completed",
            "duration_ns": 1_u64,
            "result": "Wall time: 0.0000 seconds\nProcess running with session ID 7\nOutput:\nfirst output line\nsecond output line",
            "structured_result": {
                "output": "first output line\nsecond output line",
                "session_id": 7,
                "wall_time_seconds": 0.0,
            },
            "metadata": null,
        }),
    )));
    let id = transcript
        .model
        .running_tool_ids()
        .next()
        .expect("yielded shell should remain active");
    transcript.cache.set_expanded(id, true);
    drop(render(&mut transcript, 50, 10));
    let cached = transcript.cache.cached(id).unwrap();
    let details = cached.lines[cached.tool_summary_lines..].to_vec();
    let timer = transcript.running_tool_timers[&id];

    let frame_at = timer.observed_at + Duration::from_millis(1_234);
    transcript.update(TranscriptEvent::AnimationFrame(frame_at));
    drop(render(&mut transcript, 50, 10));

    let cached = transcript.cache.cached(id).unwrap();
    assert_eq!(cached.lines[cached.tool_summary_lines..], details);
    assert_eq!(
        cached.live_duration_ns,
        Some(u64::try_from(timer.elapsed(frame_at).as_nanos()).unwrap())
    );
}

#[test]
fn background_wait_status_does_not_use_the_running_prefix() {
    let mut transcript = Transcript::new();
    let update = transcript.update(TranscriptEvent::Record(agent_with_payload(
        1,
        AgentEventKind::ToolCall,
        json!({
            "call_id": "call-1",
            "tool": "wait",
            "arguments": {"cell_id": "12"},
        }),
    )));

    assert_eq!(
        update.effects[0].status.as_deref(),
        Some("Waiting for background work…")
    );
}

#[test]
fn tool_summary_has_a_blank_row_before_the_next_entry() {
    let mut transcript = Transcript::new();
    shell(&mut transcript, 1, "done");
    transcript.update(TranscriptEvent::Record(user(3, "next message")));

    let backend = render(&mut transcript, 60, 8);
    let rows = backend
        .buffer()
        .content()
        .chunks(60)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>();
    let tool = rows
        .iter()
        .position(|row| row.contains("Shell"))
        .expect("tool summary should render");
    let user = rows
        .iter()
        .position(|row| row.contains("next message"))
        .expect("following user entry should render");

    assert_eq!(user, tool + 2);
    assert!(rows[tool + 1].trim().is_empty());
}
