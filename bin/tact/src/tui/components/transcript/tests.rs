use super::{
    Anchor, Component, ExpandableCommand, RenderRequest, ScrollCommand, Transcript,
    TranscriptEffect, TranscriptEvent, image, render::EntryRenderer, unix_time_ms,
    viewport::ScrollState,
};
use crate::{
    app::{
        config::{ReasoningEffort, ReasoningMode, Speed, TuiConfig},
        theme::Theme,
    },
    core::transcript::{EntryId, EntryKind, LocalEvent, SessionStarted, TranscriptRecord, TurnId},
};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use nanocodex::{
    HarnessModel as Model, Model as CodexModel,
    agent::events::{AgentEvent, AgentEventKind},
};
use ratatui::{Terminal, backend::TestBackend, buffer::Buffer, layout::Position};
use serde_json::{Value, json, value::to_raw_value};
use std::{
    fs::File,
    num::NonZeroU16,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use tact_subagents::{AgentMessageUpdate, MessageSender};

const FIVE_LINE_PROMPT: &str = "prompt one\nprompt two\nprompt three\nprompt four\nprompt five";

fn local(sequence: u64, event: LocalEvent) -> Arc<TranscriptRecord> {
    Arc::new(TranscriptRecord::from_local(sequence, sequence, event).unwrap())
}

fn user(sequence: u64, text: impl Into<String>) -> Arc<TranscriptRecord> {
    local(
        sequence,
        LocalEvent::UserSubmitted {
            id: TurnId::new(sequence),
            text: text.into(),
        },
    )
}

fn fork_started(sequence: u64, parent_sequence: u64) -> Arc<TranscriptRecord> {
    local(
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
}

fn agent(sequence: u64, kind: AgentEventKind) -> Arc<TranscriptRecord> {
    agent_with_payload(sequence, kind, json!({}))
}

fn agent_with_payload(
    sequence: u64,
    kind: AgentEventKind,
    payload: Value,
) -> Arc<TranscriptRecord> {
    agent_with_payload_at(sequence, sequence, kind, payload)
}

fn agent_with_payload_at(
    sequence: u64,
    recorded_at_unix_ms: u64,
    kind: AgentEventKind,
    payload: Value,
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

/// A final answer from its own model call.
fn assistant(sequence: u64, text: impl Into<String>) -> Arc<TranscriptRecord> {
    agent_with_payload(
        sequence,
        AgentEventKind::AssistantMessage,
        json!({
            "model_call_index": sequence,
            "item_id": format!("answer-{sequence}"),
            "phase": "final_answer",
            "text": text.into(),
        }),
    )
}

fn reasoning(sequence: u64, text: &str) -> Arc<TranscriptRecord> {
    agent_with_payload(
        sequence,
        AgentEventKind::ReasoningSummaryDelta,
        json!({"model_call_index": 1, "text": text}),
    )
}

fn numbered_lines(prefix: &str, count: usize) -> String {
    (1..=count)
        .map(|line| format!("{prefix} {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn tool_call(sequence: u64, call_id: &str, tool: &str, arguments: Value) -> Arc<TranscriptRecord> {
    tool_call_at(sequence, sequence, call_id, tool, arguments)
}

fn tool_call_at(
    sequence: u64,
    recorded_at_unix_ms: u64,
    call_id: &str,
    tool: &str,
    arguments: Value,
) -> Arc<TranscriptRecord> {
    agent_with_payload_at(
        sequence,
        recorded_at_unix_ms,
        AgentEventKind::ToolCall,
        json!({"call_id": call_id, "tool": tool, "arguments": arguments}),
    )
}

fn shell_arguments() -> Value {
    json!({"cmd": "cargo test", "workdir": "/work"})
}

/// A shell command that exited successfully after `duration_ns`.
fn shell_result(
    sequence: u64,
    call_id: &str,
    duration_ns: u64,
    output: &str,
) -> Arc<TranscriptRecord> {
    let seconds = Duration::from_nanos(duration_ns).as_secs_f64();
    agent_with_payload(
        sequence,
        AgentEventKind::ToolResult,
        json!({
            "call_id": call_id,
            "tool": "exec_command",
            "status": "completed",
            "duration_ns": duration_ns,
            "result": format!(
                "Wall time: {seconds:.4} seconds\nProcess exited with code 0\nOutput:\n{output}"
            ),
            "structured_result": {
                "output": output,
                "exit_code": 0,
                "wall_time_seconds": seconds,
            },
            "metadata": null,
        }),
    )
}

fn shell(transcript: &mut Transcript, sequence: u64, output: &str) {
    let call_id = format!("call-{sequence}");
    transcript.update(TranscriptEvent::Record(tool_call(
        sequence,
        &call_id,
        "exec_command",
        shell_arguments(),
    )));
    transcript.update(TranscriptEvent::Record(shell_result(
        sequence + 1,
        &call_id,
        1_200_000_000,
        output,
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

fn transcript_of(records: impl IntoIterator<Item = Arc<TranscriptRecord>>) -> Transcript {
    let mut transcript = Transcript::new();
    for record in records {
        transcript.update(TranscriptEvent::Record(record));
    }
    transcript
}

/// A transcript of user prompts reading "line 1" through "line {count}".
fn numbered_prompts(count: u64) -> Transcript {
    transcript_of((1..=count).map(|sequence| user(sequence, format!("line {sequence}"))))
}

/// A prompt followed by a 40-line answer, scrolled two lines into the answer
/// so that the prompt is pinned. Returns the prompt and answer entries.
fn pinned_transcript(prompt: &str) -> (Transcript, EntryId, EntryId) {
    let mut transcript =
        transcript_of([user(1, prompt), assistant(2, numbered_lines("answer", 40))]);
    let prompt = user_id(&transcript, prompt);
    let answer = entry_id(&transcript, |kind| {
        matches!(kind, EntryKind::Assistant { .. })
    });
    detach(&mut transcript, answer, 2);
    (transcript, prompt, answer)
}

fn entry_ids(transcript: &Transcript, matches: impl Fn(&EntryKind) -> bool) -> Vec<EntryId> {
    transcript
        .model
        .entries()
        .iter()
        .filter(|entry| matches(&entry.kind))
        .map(|entry| entry.id)
        .collect()
}

/// The only entry whose kind matches.
fn entry_id(transcript: &Transcript, matches: impl Fn(&EntryKind) -> bool) -> EntryId {
    let ids = entry_ids(transcript, matches);
    assert_eq!(ids.len(), 1, "expected exactly one matching entry");
    ids[0]
}

fn user_id(transcript: &Transcript, prompt: &str) -> EntryId {
    entry_id(
        transcript,
        |kind| matches!(kind, EntryKind::User { text, .. } if text == prompt),
    )
}

fn tool_id(transcript: &Transcript, name: &str) -> EntryId {
    entry_id(
        transcript,
        |kind| matches!(kind, EntryKind::Tool(tool) if tool.name == name),
    )
}

/// The text of each line in the cached layout of an entry.
fn layout_text(transcript: &Transcript, id: EntryId) -> Vec<String> {
    transcript
        .cache
        .cached(id)
        .expect("entry should have been laid out")
        .lines
        .iter()
        .map(ToString::to_string)
        .collect()
}

fn expanded(transcript: &Transcript, id: EntryId) -> bool {
    transcript
        .cache
        .cached(id)
        .expect("entry should have been laid out")
        .expanded()
}

/// Shows every running tool as having just started, so rendered rows do not
/// depend on the wall clock.
fn reset_running_durations(transcript: &mut Transcript) {
    for id in transcript.model.running_tool_ids() {
        transcript.cache.set_live_tool_duration(id, 0);
    }
}

fn detach(transcript: &mut Transcript, entry: EntryId, line: usize) {
    transcript
        .viewport
        .set_state(ScrollState::Detached(Anchor { entry, line }));
}

fn mouse(kind: MouseEventKind, column: u16, row: u16) -> Event {
    Event::Mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    })
}

fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
    Event::Key(KeyEvent::new(code, modifiers))
}

fn scroll(transcript: &mut Transcript, event: Event) {
    let command = transcript
        .scroll_command(&event, TuiConfig::default().mouse_scroll_lines)
        .expect("test event should be a transcript scroll command");
    transcript.update(TranscriptEvent::Scroll(command));
}

/// One drawn frame of the transcript and its chrome.
struct Screen {
    buffer: Buffer,
    /// The text of each terminal row without trailing blanks.
    rows: Vec<String>,
    /// The transcript layout line drawn on each terminal row.
    anchors: Vec<Option<Anchor>>,
}

impl Screen {
    fn row_of(&self, anchor: Anchor) -> Option<usize> {
        self.anchors.iter().position(|drawn| *drawn == Some(anchor))
    }

    /// The row of the first line of an entry that must be on screen.
    fn first_row(&self, entry: EntryId) -> usize {
        self.row_of(Anchor { entry, line: 0 })
            .expect("entry should start on screen")
    }

    /// The entries drawn on screen, top to bottom.
    fn entries(&self) -> Vec<EntryId> {
        let mut entries = Vec::new();
        for anchor in self.anchors.iter().flatten() {
            if entries.last() != Some(&anchor.entry) {
                entries.push(anchor.entry);
            }
        }
        entries
    }

    fn shows(&self, entry: EntryId) -> bool {
        self.anchors
            .iter()
            .flatten()
            .any(|anchor| anchor.entry == entry)
    }

    /// Text on rows that no transcript line was drawn on.
    fn unanchored_text(&self) -> Vec<&str> {
        self.rows
            .iter()
            .zip(&self.anchors)
            .filter(|(row, anchor)| anchor.is_none() && !row.is_empty())
            .map(|(row, _)| row.as_str())
            .collect()
    }
}

fn render(transcript: &mut Transcript, width: u16, height: u16) -> Screen {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| {
            transcript.render(frame, frame.area(), &Theme::default());
            transcript.render_chrome(frame, frame.area(), &Theme::default());
        })
        .unwrap();
    let buffer = terminal.backend().buffer().clone();
    let rows = (0..height)
        .map(|row| {
            (0..width)
                .map(|column| buffer[(column, row)].symbol())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect();
    let anchors = (0..height)
        .map(|row| transcript.hits.anchor_at(row))
        .collect();
    Screen {
        buffer,
        rows,
        anchors,
    }
}

fn render_until_image_ready(transcript: &mut Transcript, width: u16, height: u16) -> Screen {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let screen = render(transcript, width, height);
        if transcript.cache.first_image().is_some() {
            return screen;
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
    transcript.update(TranscriptEvent::Record(assistant(1, text)));
    transcript
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
    let id = transcript.model.entries()[0].id;
    assert_eq!(
        layout_text(&transcript, id),
        ["┃ a", "┃ b", "┃ c", "┃ [Image #1]", "┃ d", "┃ e", ""]
    );
    let layout = transcript.cache.cached(id).unwrap();
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
    let id = transcript.model.entries()[0].id;
    assert_eq!(
        layout_text(&transcript, id),
        ["┃ before ", "┃ [Image #1]", "┃  after", ""]
    );
    let layout = transcript.cache.cached(id).unwrap();
    assert!(layout.images.is_empty());
    assert_eq!(
        layout.lines[1].spans[1].style.fg,
        Some(Theme::default().accent())
    );
}

#[test]
fn user_lines_have_a_cyan_gutter_without_outer_chrome() {
    let mut transcript = transcript_of([user(1, "hello\nworld")]);

    let screen = render(&mut transcript, 20, 4);

    assert_eq!(screen.rows, ["", "┃ hello", "┃ world", ""]);
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

    let screen = render_until_image_ready(&mut transcript, 20, 5);
    let id = transcript.model.entries()[0].id;
    let image = &transcript.cache.cached(id).unwrap().images[0];
    let image_row = screen
        .row_of(Anchor {
            entry: id,
            line: image.line,
        })
        .expect("image should be on screen");

    assert_eq!(usize::from(image.protocol.size().height), 1);
    assert_eq!(layout_text(&transcript, id)[image.line - 1], "before");
    assert_eq!(layout_text(&transcript, id)[image.line + 1], "after");
    assert_eq!(screen.rows[image_row - 1], "before");
    assert_eq!(screen.rows[image_row + 1], "after");
    assert!(!screen.rows[image_row].is_empty());
}

#[test]
fn refreshing_terminal_images_preserves_their_dimensions() {
    let workspace = tempfile::tempdir().unwrap();
    write_png_size(&workspace.path().join("sample.png"), 80, 40);
    let mut transcript = transcript_with_image(workspace.path(), "![sample](sample.png)");

    render_until_image_ready(&mut transcript, 20, 3);
    let original = Arc::clone(transcript.cache.first_image().unwrap());
    let original_size = original.size();
    assert!(original_size.width > 1);
    assert!(original_size.height > 1);

    transcript.refresh_terminal_images();
    let pending = transcript.cache.first_image().unwrap();
    assert!(Arc::ptr_eq(&original, pending));

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        render(&mut transcript, 20, 3);
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
    render(&mut transcript, 40, 5);
    let id = transcript.model.entries()[0].id;
    assert_eq!(
        layout_text(&transcript, id),
        ["before sample ↗ sample.png after", ""]
    );
    assert!(transcript.cache.first_image().is_none());
    assert!(transcript.animation_deadline().is_some());

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut rendered_completion = false;
    while transcript.cache.first_image().is_none() {
        let update = transcript.update(TranscriptEvent::AnimationFrame(Instant::now()));
        rendered_completion |= update.render == RenderRequest::Streaming;
        render(&mut transcript, 40, 5);
        assert!(Instant::now() < deadline, "image hydration timed out");
        std::thread::yield_now();
    }
    assert!(rendered_completion);
}

#[test]
fn pending_image_hydration_restarts_after_terminal_refresh() {
    let workspace = tempfile::tempdir().unwrap();
    write_png(&workspace.path().join("sample.png"));
    let mut transcript = transcript_with_image(workspace.path(), "![sample](sample.png)");

    render(&mut transcript, 20, 3);
    transcript.refresh_terminal_images();
    render_until_image_ready(&mut transcript, 20, 3);

    assert!(transcript.cache.first_image().is_some());
}

#[test]
fn milestones_and_setting_changes_render_as_markers() {
    let cases = [
        (vec![fork_started(1, 0)], vec!["◇ Forked from @@parent", ""]),
        (
            vec![
                agent_with_payload_at(1, 1_000, AgentEventKind::RunStarted, json!({})),
                agent_with_payload_at(
                    2,
                    66_432,
                    AgentEventKind::RunCompleted,
                    json!({"duration_ns": 65_432_000_000_u64}),
                ),
            ],
            vec!["◇ Turn completed · 1m 5s", ""],
        ),
        (
            vec![local(
                1,
                LocalEvent::ReflectionStarted { id: TurnId::new(1) },
            )],
            vec!["◇ Reflection started", ""],
        ),
        (
            vec![local(
                1,
                LocalEvent::EffortChanged {
                    from: ReasoningEffort::Medium,
                    to: ReasoningEffort::High,
                },
            )],
            vec![
                "◇ Effort changed to high · takes effect on the next turn",
                "",
            ],
        ),
        (
            vec![local(
                1,
                LocalEvent::SpeedChanged {
                    from: Speed::Standard,
                    to: Speed::Fast,
                },
            )],
            vec![
                "◇ Speed changed to fast · takes effect on the next turn",
                "",
            ],
        ),
    ];

    for (records, expected) in cases {
        let mut transcript = transcript_of(records);
        let screen = render(&mut transcript, 72, 4);
        let [marker] = screen.entries()[..] else {
            panic!("expected one visible entry, got {:?}", screen.entries());
        };
        assert_eq!(layout_text(&transcript, marker), expected);
    }
}

#[test]
fn session_setting_notifications_color_their_new_value() {
    let mut transcript = transcript_of([
        local(
            1,
            LocalEvent::EffortChanged {
                from: ReasoningEffort::Medium,
                to: ReasoningEffort::High,
            },
        ),
        local(
            2,
            LocalEvent::SpeedChanged {
                from: Speed::Standard,
                to: Speed::Fast,
            },
        ),
    ]);
    render(&mut transcript, 72, 6);
    let value_color = |matches: fn(&EntryKind) -> bool, value: &str| {
        let id = entry_id(&transcript, matches);
        transcript.cache.cached(id).unwrap().lines[0]
            .spans
            .iter()
            .find(|span| span.content == value)
            .expect("notification should include its value")
            .style
            .fg
    };

    assert_eq!(
        value_color(
            |kind| matches!(kind, EntryKind::EffortChanged { .. }),
            "high"
        ),
        Some(Theme::default().effort(ReasoningEffort::High))
    );
    assert_eq!(
        value_color(
            |kind| matches!(kind, EntryKind::SpeedChanged { .. }),
            "fast"
        ),
        Some(Theme::default().speed(Speed::Fast))
    );
}

#[test]
fn fork_start_discards_the_active_parent_turn() {
    let transcript = transcript_of([
        user(1, "completed"),
        user(2, "still running"),
        agent(3, AgentEventKind::RunStarted),
        fork_started(4, 3),
    ]);

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
    let mut transcript = transcript_of([user(
        1,
        "before\n    fn main() {\n        work();\n    }\nafter",
    )]);
    render(&mut transcript, 30, 8);

    assert_eq!(
        layout_text(&transcript, transcript.model.entries()[0].id),
        [
            "┃ before",
            "┃     fn main() {",
            "┃         work();",
            "┃     }",
            "┃ after",
            "",
        ]
    );
}

#[test]
fn user_soft_wrap_omits_the_separator_space_but_preserves_explicit_indentation() {
    let mut transcript = transcript_of([user(1, "alpha bravo\n bravo")]);
    render(&mut transcript, 12, 5);

    assert_eq!(
        layout_text(&transcript, transcript.model.entries()[0].id),
        ["┃ alpha", "┃ bravo", "┃  bravo", ""]
    );
}

#[test]
fn detached_transcript_pins_at_most_three_lines_of_the_previous_prompt() {
    let (mut transcript, prompt, answer) = pinned_transcript(FIVE_LINE_PROMPT);

    let screen = render(&mut transcript, 30, 6);
    let pinned = transcript
        .pinned_prompt
        .expect("the previous prompt should be pinned");

    assert_eq!(pinned.area.height, 3);
    assert_eq!(
        screen.rows[..3],
        [
            "┃ prompt one".to_owned(),
            "┃ prompt two".to_owned(),
            format!("{:<29}…", "┃ prompt three"),
        ]
    );
    assert_eq!(
        screen.anchors[..3],
        (0..3)
            .map(|line| Some(Anchor {
                entry: prompt,
                line
            }))
            .collect::<Vec<_>>()
    );
    assert_eq!(screen.entries()[1..], [answer]);
}

#[test]
fn pinned_prompt_uses_the_code_block_background() {
    let (mut transcript, _, _) = pinned_transcript("pinned prompt");

    let screen = render(&mut transcript, 30, 6);
    let area = transcript.pinned_prompt.unwrap().area;

    for row in area.y..area.bottom() {
        for column in area.x..area.right() {
            assert_eq!(
                screen.buffer[(column, row)].bg,
                Theme::default().code_background()
            );
        }
    }
}

#[test]
fn active_stream_does_not_pin_while_following() {
    let mut transcript = transcript_of([
        user(1, "streaming prompt"),
        agent(2, AgentEventKind::RunStarted),
        assistant(3, numbered_lines("streamed", 40)),
    ]);

    render(&mut transcript, 30, 6);

    assert_eq!(transcript.viewport.state(), ScrollState::Follow);
    assert!(transcript.pinned_prompt.is_none());
}

#[test]
fn scrolling_over_a_pinned_prompt_reveals_it_without_moving_the_transcript() {
    let (mut transcript, prompt, _) = pinned_transcript(FIVE_LINE_PROMPT);
    render(&mut transcript, 30, 6);
    let transcript_top = transcript.viewport.top();

    for _ in 0..2 {
        scroll(&mut transcript, mouse(MouseEventKind::ScrollDown, 5, 1));
        render(&mut transcript, 30, 6);
    }
    let screen = render(&mut transcript, 30, 6);

    assert_eq!(transcript.viewport.top(), transcript_top);
    assert_eq!(
        screen.rows[..3],
        [
            format!("{:<29}…", "┃ prompt three"),
            "┃ prompt four".to_owned(),
            "┃ prompt five".to_owned(),
        ]
    );
    assert_eq!(
        screen.anchors[..3],
        (2..5)
            .map(|line| Some(Anchor {
                entry: prompt,
                line
            }))
            .collect::<Vec<_>>()
    );
}

#[test]
fn page_navigation_uses_the_unpinned_transcript_height() {
    let (mut transcript, _, _) = pinned_transcript("prompt one\nprompt two\nprompt three");
    render(&mut transcript, 30, 6);
    assert_eq!(transcript.pinned_prompt.unwrap().area.height, 3);

    let page_up = transcript.scroll_command(
        &key(KeyCode::PageUp, KeyModifiers::NONE),
        TuiConfig::default().mouse_scroll_lines,
    );
    let page_down = transcript.scroll_command(
        &key(KeyCode::PageDown, KeyModifiers::NONE),
        TuiConfig::default().mouse_scroll_lines,
    );

    assert!(matches!(page_up, Some(ScrollCommand::Rows(-1))));
    assert!(matches!(page_down, Some(ScrollCommand::Rows(1))));
}

#[test]
fn clicking_a_pinned_prompt_jumps_to_it_in_the_transcript() {
    let (mut transcript, prompt, _) = pinned_transcript("pinned prompt");
    render(&mut transcript, 30, 6);

    assert!(transcript.pinned_prompt_clicked(&mouse(
        MouseEventKind::Down(MouseButton::Left),
        5,
        0
    )));
    transcript.update(TranscriptEvent::JumpToPinnedPrompt);
    let screen = render(&mut transcript, 30, 6);

    let top = Anchor {
        entry: prompt,
        line: 0,
    };
    assert_eq!(transcript.viewport.top(), Some(top));
    assert!(transcript.pinned_prompt.is_none());
    assert_eq!(screen.anchors[0], Some(top));
}

#[test]
fn pinned_prompt_tracks_the_turn_at_the_top_of_the_viewport() {
    let mut transcript = transcript_of((1..=2).flat_map(|turn| {
        [
            user(turn * 2 - 1, format!("prompt {turn}")),
            assistant(turn * 2, numbered_lines(&format!("turn {turn} answer"), 40)),
        ]
    }));
    let prompts = entry_ids(&transcript, |kind| matches!(kind, EntryKind::User { .. }));
    let answers = entry_ids(&transcript, |kind| {
        matches!(kind, EntryKind::Assistant { .. })
    });

    for (prompt, answer) in prompts.into_iter().zip(answers) {
        detach(&mut transcript, answer, 2);
        render(&mut transcript, 30, 6);

        assert_eq!(
            transcript.pinned_prompt.map(|pinned| pinned.entry),
            Some(prompt)
        );
    }
}

#[test]
fn prompt_is_not_pinned_above_another_visible_prompt() {
    let mut transcript = transcript_of([
        user(1, "first prompt"),
        user(2, "second prompt"),
        assistant(3, "a sufficiently long response ".repeat(20)),
    ]);
    let second_prompt = user_id(&transcript, "second prompt");
    detach(&mut transcript, second_prompt, 0);

    render(&mut transcript, 30, 2);

    assert!(transcript.pinned_prompt.is_none());
}

#[test]
fn active_selection_can_cross_an_unselectable_tool() {
    let mut transcript = transcript_of([user(1, "before")]);
    shell(&mut transcript, 2, "output");
    transcript.update(TranscriptEvent::Record(user(4, "after")));

    let screen = render(&mut transcript, 40, 12);
    let tool_row = screen.first_row(tool_id(&transcript, "exec_command"));
    let position = Position::new(0, u16::try_from(tool_row).unwrap());

    assert!(transcript.selection_span(position).is_none());
    assert!(transcript.selection_span_nearest(position).is_some());
}

#[test]
fn expanded_tool_details_are_selectable_but_the_summary_remains_clickable() {
    let mut transcript = Transcript::new();
    shell(&mut transcript, 1, "selectable output");
    let tool = tool_id(&transcript, "exec_command");
    render(&mut transcript, 60, 12);
    transcript.focus_expandables();
    transcript.update(TranscriptEvent::Expandable(ExpandableCommand::Toggle));
    let screen = render(&mut transcript, 60, 12);
    let selectable = |row: usize| {
        transcript
            .selection_span(Position::new(10, u16::try_from(row).unwrap()))
            .is_some()
    };
    let summary = screen.first_row(tool);
    let details = (0..screen.rows.len()).filter(|&row| {
        screen.anchors[row].is_some_and(|anchor| anchor.entry == tool && anchor.line > 0)
    });

    assert!(!selectable(summary));
    assert!(details.into_iter().any(selectable));
}

#[test]
fn commentary_and_reasoning_render_their_content_without_labels() {
    let mut transcript = transcript_of([
        agent_with_payload(
            1,
            AgentEventKind::AssistantMessage,
            json!({
                "model_call_index": 1,
                "item_id": "commentary",
                "phase": "commentary",
                "text": "commentary body",
            }),
        ),
        reasoning(2, "reasoning body"),
    ]);
    render(&mut transcript, 40, 8);
    let commentary = entry_id(&transcript, |kind| {
        matches!(kind, EntryKind::Assistant { .. })
    });
    let reasoning = entry_id(&transcript, |kind| {
        matches!(kind, EntryKind::Reasoning { .. })
    });

    assert_eq!(
        layout_text(&transcript, commentary),
        ["commentary body", ""]
    );
    assert_eq!(layout_text(&transcript, reasoning), ["reasoning body", ""]);
}

#[test]
fn adjacent_bold_reasoning_steps_render_on_separate_rows() {
    let mut transcript = transcript_of([
        reasoning(1, "**Planning retrieval**"),
        reasoning(2, "**Confirming output**"),
    ]);
    render(&mut transcript, 40, 6);
    let reasoning = entry_id(&transcript, |kind| {
        matches!(kind, EntryKind::Reasoning { .. })
    });

    assert_eq!(
        layout_text(&transcript, reasoning),
        ["Planning retrieval", "Confirming output", ""]
    );
}

#[test]
fn empty_logo_is_replaced_as_soon_as_transcript_content_arrives() {
    let mut transcript = Transcript::new();

    let empty = render(&mut transcript, 41, 14);
    assert!(!empty.unanchored_text().is_empty());
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
    assert!(populated.unanchored_text().is_empty());
    assert!(transcript.animation_deadline().is_none());
}

#[test]
fn retry_status_keeps_scheduled_delay_during_attempt() {
    for delay_ns in [0, 200_000_000, 2_000_000_000_u64] {
        let mut transcript = transcript_of([
            user(1, "hello"),
            agent(2, AgentEventKind::RunStarted),
            agent_with_payload(
                3,
                AgentEventKind::ModelAttemptRetrying,
                json!({"delay_ns": delay_ns, "attempt": 1, "next_attempt": 2, "max_attempts": 5, "error": "temporary"}),
            ),
        ]);
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
        let mut transcript = transcript_of([
            user(1, "hello"),
            agent(2, AgentEventKind::RunStarted),
            agent_with_payload(
                3,
                AgentEventKind::ModelAttemptRetrying,
                json!({"delay_ns": 2_000_000_000_u64, "attempt": 1, "next_attempt": 2, "max_attempts": 5, "error": "temporary"}),
            ),
        ]);
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
    let tool = tool_id(&transcript, "exec_command");

    let collapsed = render(&mut transcript, 60, 8);
    let summary = collapsed.first_row(tool);
    assert!(!expanded(&transcript, tool));
    assert_eq!(
        collapsed.rows[summary],
        "  ▶ ✓ Shell  $ cargo test · exit 0 · 1.2s"
    );
    let collapsed_lines = layout_text(&transcript, tool).len();

    transcript.focus_expandables();
    let focused = render(&mut transcript, 60, 8);
    assert_eq!(transcript.selected_expandable, Some(tool));
    assert_eq!(
        focused.rows[focused.first_row(tool)],
        "› ▶ ✓ Shell  $ cargo test · exit 0 · 1.2s"
    );

    transcript.update(TranscriptEvent::Expandable(ExpandableCommand::Toggle));
    let opened = render(&mut transcript, 60, 8);
    let summary = opened.first_row(tool);
    assert!(expanded(&transcript, tool));
    assert!(layout_text(&transcript, tool).len() > collapsed_lines);
    assert_eq!(
        opened.rows[summary],
        "› ▼ ✓ Shell  $ cargo test · exit 0 · 1.2s"
    );
    assert_eq!(
        opened.anchors[summary + 1],
        Some(Anchor {
            entry: tool,
            line: 1
        })
    );

    transcript.update(TranscriptEvent::BlurExpandables);
    assert!(!transcript.expandables_focused());
    let blurred = render(&mut transcript, 60, 8);
    assert_eq!(
        blurred.rows[blurred.first_row(tool)],
        "  ▼ ✓ Shell  $ cargo test · exit 0 · 1.2s"
    );
}

#[test]
fn focus_navigation_moves_between_tools_and_message_threads() {
    let mut transcript = Transcript::new();
    shell(&mut transcript, 1, "done");
    directed_message(&mut transcript);
    render(&mut transcript, 80, 12);

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
    let message = entry_id(&transcript, |kind| {
        matches!(kind, EntryKind::DirectedMessage(_))
    });

    render(&mut transcript, 80, 10);
    assert!(!expanded(&transcript, message));
    let collapsed_lines = layout_text(&transcript, message).len();

    transcript.update(TranscriptEvent::ToggleExpandAll);
    render(&mut transcript, 80, 10);
    assert!(expanded(&transcript, message));
    assert!(layout_text(&transcript, message).len() > collapsed_lines);
}

#[test]
fn expand_all_toggles_every_tool_and_applies_to_future_entries() {
    let mut transcript = Transcript::new();
    shell(&mut transcript, 1, "first output");

    transcript.update(TranscriptEvent::ToggleExpandAll);
    shell(&mut transcript, 3, "future output");
    let tools = entry_ids(&transcript, |kind| matches!(kind, EntryKind::Tool(_)));
    assert_eq!(tools.len(), 2);
    render(&mut transcript, 80, 16);
    assert!(tools.iter().all(|&tool| expanded(&transcript, tool)));

    transcript.update(TranscriptEvent::ToggleExpandAll);
    render(&mut transcript, 80, 16);
    assert!(tools.iter().all(|&tool| !expanded(&transcript, tool)));
}

#[test]
fn plan_tools_are_expanded_by_default() {
    let mut transcript = transcript_of([tool_call(
        1,
        "plan-1",
        "update_plan",
        json!({
            "explanation": "Implementation plan",
            "plan": [
                {"step": "Write the regression test", "status": "completed"},
                {"step": "Change the default", "status": "in_progress"},
            ],
        }),
    )]);
    let plan = tool_id(&transcript, "update_plan");
    reset_running_durations(&mut transcript);
    render(&mut transcript, 80, 10);

    assert!(expanded(&transcript, plan));
    assert_eq!(
        layout_text(&transcript, plan),
        [
            "  ▼ ◌ Plan  1/2 complete · Change the default · 0ms",
            "    │ Implementation plan",
            "    │ ● Write the regression test",
            "    │ ◐ Change the default",
            "    └ 1/2 complete",
            "",
        ]
    );
}

#[test]
fn clicking_a_tool_summary_focuses_and_expands_it() {
    let mut transcript = Transcript::new();
    shell(&mut transcript, 1, "done");
    let tool = tool_id(&transcript, "exec_command");
    render(&mut transcript, 60, 8);
    let row = transcript.hits.expandable_rows()[0];
    let command = transcript
        .expandable_command(&mouse(MouseEventKind::Down(MouseButton::Left), 10, row))
        .unwrap();

    transcript.update(TranscriptEvent::Expandable(command));
    let screen = render(&mut transcript, 60, 8);

    assert!(transcript.expandables_focused());
    assert!(expanded(&transcript, tool));
    assert_eq!(
        screen.rows[0],
        format!("{:>60}", "↑↓ item · Enter toggle · Esc back")
    );
}

#[test]
fn clicking_a_wrapped_markdown_link_returns_its_destination() {
    let mut transcript = transcript_of([assistant(
        1,
        "[a long local filename](/work/src/main.rs:12)",
    )]);
    render(&mut transcript, 12, 8);
    let start = transcript
        .hits
        .link_start("/work/src/main.rs:12")
        .expect("rendered link should have a hit region");
    let event = mouse(MouseEventKind::Down(MouseButton::Left), start.x, start.y);

    assert_eq!(
        transcript.link_destination(&event).as_deref(),
        Some("/work/src/main.rs:12")
    );
}

#[test]
fn expanding_a_visible_tool_preserves_its_summary_row() {
    let mut transcript =
        transcript_of((1..=10).map(|sequence| user(sequence, format!("before {sequence}"))));
    shell(&mut transcript, 11, "one\ntwo\nthree");
    for sequence in 13..=18 {
        transcript.update(TranscriptEvent::Record(user(
            sequence,
            format!("after {sequence}"),
        )));
    }
    let tool = tool_id(&transcript, "exec_command");
    let top = user_id(&transcript, "before 9");
    detach(&mut transcript, top, 0);
    render(&mut transcript, 60, 10);
    transcript.focus_expandables();
    let before = render(&mut transcript, 60, 10).first_row(tool);

    transcript.update(TranscriptEvent::Expandable(ExpandableCommand::Toggle));
    let after = render(&mut transcript, 60, 10).first_row(tool);

    assert_eq!(after, before);
}

#[test]
fn mouse_scroll_uses_configured_rows_in_both_directions() {
    let transcript = Transcript::new();
    for lines in [1, 3, 8, u16::MAX] {
        for (kind, direction) in [
            (MouseEventKind::ScrollUp, -1),
            (MouseEventKind::ScrollDown, 1),
        ] {
            let command =
                transcript.scroll_command(&mouse(kind, 0, 0), NonZeroU16::new(lines).unwrap());
            assert!(
                matches!(command, Some(ScrollCommand::Rows(rows)) if rows == direction * i32::from(lines))
            );
        }
    }
}

#[test]
fn page_and_mouse_scrolling_detach_then_return_to_tail() {
    let mut transcript = numbered_prompts(20);
    let last = user_id(&transcript, "line 20");
    render(&mut transcript, 30, 6);

    scroll(&mut transcript, key(KeyCode::PageUp, KeyModifiers::NONE));
    let scrolled = render(&mut transcript, 30, 6);
    assert!(matches!(
        transcript.viewport.state(),
        ScrollState::Detached(_)
    ));
    assert!(!scrolled.shows(last));

    scroll(&mut transcript, mouse(MouseEventKind::ScrollDown, 0, 0));
    scroll(&mut transcript, key(KeyCode::End, KeyModifiers::CONTROL));
    let tail = render(&mut transcript, 30, 6);
    assert_eq!(transcript.viewport.state(), ScrollState::Follow);
    assert!(tail.shows(last));
}

#[test]
fn scrolling_down_near_the_tail_keeps_the_viewport_filled() {
    let mut transcript = numbered_prompts(2);
    render(&mut transcript, 30, 6);
    scroll(&mut transcript, key(KeyCode::PageUp, KeyModifiers::NONE));
    render(&mut transcript, 30, 6);

    scroll(&mut transcript, mouse(MouseEventKind::ScrollDown, 0, 0));
    let screen = render(&mut transcript, 30, 6);

    assert_eq!(
        screen.entries(),
        [
            user_id(&transcript, "line 1"),
            user_id(&transcript, "line 2")
        ]
    );
}

#[test]
fn incoming_records_follow_when_the_viewport_is_at_the_bottom() {
    let mut transcript = numbered_prompts(20);
    render(&mut transcript, 30, 6);
    transcript.update(TranscriptEvent::Scroll(ScrollCommand::Rows(-4)));
    render(&mut transcript, 30, 6);
    transcript.update(TranscriptEvent::Scroll(ScrollCommand::Rows(4)));
    let bottom = render(&mut transcript, 30, 6);
    assert_eq!(transcript.viewport.state(), ScrollState::Follow);
    assert!(bottom.shows(user_id(&transcript, "line 20")));

    transcript.update(TranscriptEvent::Record(user(21, "new tail")));
    let screen = render(&mut transcript, 30, 6);

    assert!(screen.shows(user_id(&transcript, "new tail")));
}

#[test]
fn incoming_records_do_not_move_a_detached_viewport() {
    let mut transcript = numbered_prompts(20);
    render(&mut transcript, 30, 6);
    transcript.update(TranscriptEvent::Scroll(ScrollCommand::Rows(-4)));
    let detached = render(&mut transcript, 30, 6);

    transcript.update(TranscriptEvent::Record(user(21, "new tail")));
    let screen = render(&mut transcript, 30, 6);

    assert!(matches!(
        transcript.viewport.state(),
        ScrollState::Detached(_)
    ));
    assert_eq!(screen.anchors, detached.anchors);
    assert!(!screen.shows(user_id(&transcript, "new tail")));
    assert_eq!(transcript.viewport.new_updates(), 1);
    assert_eq!(screen.rows[0], "┃ line 16↓ 1 update · Ctrl+End");
}

#[test]
fn generic_activity_is_only_rendered_by_the_composer() {
    let mut transcript = transcript_of([agent(1, AgentEventKind::RunStarted)]);
    assert_eq!(transcript.activity().status.as_deref(), Some("Thinking…"));

    let screen = render(&mut transcript, 30, 4);

    assert_eq!(screen.rows, render(&mut Transcript::new(), 30, 4).rows);
}

#[test]
fn active_tool_keeps_its_inline_spinner_without_a_status_row() {
    let mut transcript = Transcript::new();
    let update = transcript.update(TranscriptEvent::Record(tool_call(
        1,
        "call-1",
        "exec_command",
        shell_arguments(),
    )));

    assert_eq!(
        update.effects,
        [TranscriptEffect {
            active: false,
            status: Some("Running exec command…".to_owned()),
        }]
    );

    let screen = render(&mut transcript, 30, 4);
    let tool_row = screen.first_row(tool_id(&transcript, "exec_command"));

    assert_eq!(
        screen.buffer[(4, u16::try_from(tool_row).unwrap())].symbol(),
        "⠋"
    );
    assert!(screen.unanchored_text().is_empty());
}

#[test]
fn active_tool_uses_a_monotonic_timer_until_the_reported_duration_arrives() {
    let mut transcript = transcript_of([tool_call_at(
        1,
        unix_time_ms().saturating_sub(10_000),
        "call-1",
        "exec_command",
        shell_arguments(),
    )]);
    let tool = tool_id(&transcript, "exec_command");
    let timer = transcript.running_tool_timers[&tool];
    let live_duration = |transcript: &mut Transcript| {
        render(transcript, 50, 4);
        transcript.cache.cached(tool).unwrap().live_duration_ns
    };
    let nanos = |duration: Duration| u64::try_from(duration.as_nanos()).unwrap();

    assert!(
        (Duration::from_secs(10)..Duration::from_secs(11)).contains(&timer.elapsed_at_observation)
    );
    assert_eq!(
        live_duration(&mut transcript),
        Some(nanos(timer.elapsed_at_observation))
    );

    let frame_at = timer.observed_at + Duration::from_millis(1_234);
    let update = transcript.update(TranscriptEvent::AnimationFrame(frame_at));
    assert_eq!(update.render, RenderRequest::Streaming);
    assert_eq!(
        live_duration(&mut transcript),
        Some(nanos(timer.elapsed(frame_at)))
    );

    transcript.update(TranscriptEvent::Record(shell_result(
        2,
        "call-1",
        2_500_000_000,
        "done",
    )));
    assert!(transcript.running_tool_timers.is_empty());
    assert_eq!(live_duration(&mut transcript), None);
    assert!(matches!(
        &transcript.model.entry(tool).unwrap().kind,
        EntryKind::Tool(tool) if tool.duration_ns == Some(2_500_000_000)
    ));
}

#[test]
fn single_code_workflow_child_renders_as_a_standalone_expandable_tool() {
    let mut transcript = transcript_of([
        tool_call(
            1,
            "workflow",
            "exec",
            json!("await tools.exec_command({cmd: 'cargo test'})"),
        ),
        tool_call(
            2,
            "workflow/code-1",
            "exec_command",
            json!({"cmd": "cargo test"}),
        ),
    ]);
    let child = tool_id(&transcript, "exec_command");

    let running = render(&mut transcript, 80, 5);
    let child_row = running.first_row(child);
    assert_eq!(running.entries(), [child]);
    assert_eq!(running.rows[child_row + 1], "");

    transcript.update(TranscriptEvent::Record(shell_result(
        3,
        "workflow/code-1",
        10,
        "ok",
    )));
    let completed = render(&mut transcript, 80, 5);
    assert_eq!(transcript.model.entries().len(), 2);
    assert_eq!(completed.entries(), [child]);
    assert_eq!(
        completed.rows[completed.first_row(child)],
        "  ▶ ✓ Shell  $ cargo test · exit 0 · 0ms"
    );

    transcript.focus_expandables();
    transcript.update(TranscriptEvent::Expandable(ExpandableCommand::Toggle));
    render(&mut transcript, 80, 8);
    assert!(expanded(&transcript, child));
}

#[test]
fn code_workflow_promotes_a_standalone_tool_when_the_second_child_arrives() {
    let mut transcript = transcript_of([
        tool_call(
            1,
            "workflow",
            "exec",
            json!("await tools.exec_command({cmd: 'cargo test'})"),
        ),
        tool_call(
            2,
            "workflow/code-1",
            "exec_command",
            json!({"cmd": "cargo test"}),
        ),
    ]);
    let workflow = tool_id(&transcript, "exec");
    let first = tool_id(&transcript, "exec_command");

    reset_running_durations(&mut transcript);
    let single = render(&mut transcript, 80, 5);
    assert_eq!(single.entries(), [first]);
    assert_eq!(
        single.rows[single.first_row(first)],
        "  ▶ ⠋ Shell  $ cargo test · 0ms"
    );

    transcript.update(TranscriptEvent::Record(tool_call(
        3,
        "workflow/code-2",
        "memory",
        json!({"operation": "scan", "query": "test"}),
    )));
    reset_running_durations(&mut transcript);
    let batch = render(&mut transcript, 80, 8);
    let second = tool_id(&transcript, "memory");
    let batch_row = batch.first_row(workflow);

    assert_eq!(batch.entries(), [workflow, first, second]);
    assert_eq!(
        batch.rows[batch_row..],
        [
            "  ▶ ⠋ Batch  2 tools · 0ms",
            "  ├─  ▶ ⠋ Shell  $ cargo test · 0ms",
            "  └─  ▶ ⠋ Memory scan  test · 0ms",
            "",
        ]
    );
}

#[test]
fn final_multiline_workflow_child_connects_through_its_last_row() {
    let mut transcript = transcript_of([
        tool_call(
            1,
            "workflow",
            "exec",
            json!(
                "await tools.memory({operation: 'scan'}); await tools.exec_command({cmd: 'cargo check'})"
            ),
        ),
        tool_call(
            2,
            "workflow/code-1",
            "memory",
            json!({"operation": "scan", "query": "transcript"}),
        ),
        tool_call(
            3,
            "workflow/code-2",
            "custom_operation",
            json!({"prompt": "inspect every crate in the workspace"}),
        ),
        agent_with_payload(
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
        ),
    ]);

    reset_running_durations(&mut transcript);
    let screen = render(&mut transcript, 60, 8);

    assert_eq!(
        screen.rows,
        [
            "",
            "  ▶ ⠋ Batch  2 tools · 0ms",
            "  ├─  ▶ ⠋ Memory scan  transcript · 0ms",
            "  ├─  ▶ × Custom operation  inspect every crate in the",
            "  │       workspace · error[E0277]: the size for values of",
            "  │       type `Self` cannot be known at compilation time ·",
            "  └─      1.2s",
            "",
        ]
    );
}

#[test]
fn code_workflow_children_remain_visible_at_narrow_widths() {
    let mut transcript = transcript_of([
        tool_call(
            1,
            "workflow",
            "exec",
            json!("await tools.exec_command({cmd: 'pwd'})"),
        ),
        tool_call(2, "workflow/code-1", "exec_command", json!({"cmd": "pwd"})),
    ]);
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
    let recorded_at = unix_time_ms();
    let mut transcript = transcript_of([
        tool_call_at(1, recorded_at, "shell", "exec_command", shell_arguments()),
        agent_with_payload_at(
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
        ),
    ]);
    let id = transcript
        .model
        .running_tool_ids()
        .next()
        .expect("yielded shell should remain active");
    transcript.cache.set_expanded(id, true);
    render(&mut transcript, 50, 10);
    let cached = transcript.cache.cached(id).unwrap();
    let details = cached.lines[cached.tool_summary_lines..].to_vec();
    let timer = transcript.running_tool_timers[&id];

    let frame_at = timer.observed_at + Duration::from_millis(1_234);
    transcript.update(TranscriptEvent::AnimationFrame(frame_at));
    render(&mut transcript, 50, 10);

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
    let update = transcript.update(TranscriptEvent::Record(tool_call(
        1,
        "call-1",
        "wait",
        json!({"cell_id": "12"}),
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

    let screen = render(&mut transcript, 60, 8);
    let tool = screen.first_row(tool_id(&transcript, "exec_command"));

    assert_eq!(
        screen.first_row(user_id(&transcript, "next message")),
        tool + 2
    );
    assert_eq!(screen.rows[tool + 1], "");
}
