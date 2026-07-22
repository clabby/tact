//! Criterion baselines for foundational TUI operations.

#![allow(dead_code, unused_imports)]

#[path = "../config.rs"]
mod config;
#[path = "../error.rs"]
mod error;
#[path = "../subagents.rs"]
mod subagents;

#[path = "components/mod.rs"]
mod components;
mod context;
mod format;
mod pane;
mod prompt;
mod session;
mod spinner;
mod theme;
#[path = "transcript/mod.rs"]
pub(crate) mod transcript;

// `config.rs` uses the production module path while this benchmark compiles the
// same internal modules directly into its private target.
mod tui {
    pub(crate) use crate::{context, format, pane, prompt, session, spinner, theme, transcript};
}

use components::{AppEvent, AppNode, RootNode};
use config::ReasoningEffort;
use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use nanocodex::{AgentEvent, AgentEventKind};
use pane::PaneId;
use ratatui::{Terminal, backend::TestBackend};
use serde_json::{json, value::to_raw_value};
use std::{hint::black_box, path::Path, sync::Arc};
use theme::Theme;
use transcript::{LocalEvent, TranscriptRecord, TurnId};

const WIDTH: u16 = 120;
const HEIGHT: u16 = 40;

struct Harness {
    app: AppNode,
    terminal: Terminal<TestBackend>,
}

impl Harness {
    fn new(draft: &str, present_first_frame: bool) -> Self {
        let root = RootNode::new(Path::new("/workspace"), ReasoningEffort::Medium);
        let workspace = Path::new("/workspace").to_path_buf();
        let mut app = AppNode::new(Theme::default(), workspace, root);
        drop(app.update(AppEvent::EditorDraft {
            pane: PaneId::Main,
            draft: draft.to_owned(),
        }));
        let terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).unwrap();
        let mut harness = Self { app, terminal };
        if present_first_frame {
            harness.render();
        }
        harness
    }

    fn render(&mut self) {
        self.terminal.draw(|frame| self.app.render(frame)).unwrap();
    }

    fn with_transcript(entries: u64) -> Self {
        let mut harness = Self::new("", false);
        for sequence in 1..=entries {
            harness.apply_record(local_record(
                sequence,
                LocalEvent::UserSubmitted {
                    id: TurnId::new(sequence),
                    text: format!("transcript entry {sequence} with enough text to wrap"),
                },
            ));
        }
        harness.render();
        harness
    }

    fn with_tools(entries: u64, output_bytes: usize) -> Self {
        let mut harness = Self::new("", false);
        let output = "x".repeat(output_bytes);
        for index in 0..entries {
            let sequence = index.saturating_mul(2).saturating_add(1);
            let call_id = format!("shell-{index}");
            harness.apply_record(agent_record(
                sequence,
                AgentEventKind::ToolCall,
                json!({
                    "call_id": call_id,
                    "tool": "exec_command",
                    "arguments": {"cmd": format!("cargo test package-{index}")},
                }),
            ));
            harness.apply_record(agent_record(
                sequence + 1,
                AgentEventKind::ToolResult,
                json!({
                    "call_id": call_id,
                    "tool": "exec_command",
                    "status": "completed",
                    "duration_ns": 1_000_000_u64,
                    "result": {"output": output.as_str(), "exit_code": 0},
                    "metadata": null,
                }),
            ));
        }
        harness.render();
        harness
    }

    fn with_patch(hunks: usize) -> Self {
        let mut harness = Self::new("", false);
        let mut patch = String::from("*** Begin Patch\n*** Update File: src/example.rs\n");
        for index in 0..hunks {
            patch.push_str(&format!(
                "@@ -{index},2 +{index},2 @@ fn example_{index}()\n let before = {index};\n-old_call(before);\n+new_call(before);\n"
            ));
        }
        patch.push_str("*** End Patch");
        harness.apply_record(agent_record(
            1,
            AgentEventKind::ToolCall,
            json!({
                "call_id": "patch",
                "tool": "apply_patch",
                "arguments": patch,
            }),
        ));
        harness.apply_record(agent_record(
            2,
            AgentEventKind::ToolResult,
            json!({
                "call_id": "patch",
                "tool": "apply_patch",
                "status": "completed",
                "duration_ns": 1_000_000_u64,
                "result": "Done!",
                "metadata": null,
            }),
        ));
        harness.render();
        harness
    }

    fn apply_record(&mut self, record: Arc<TranscriptRecord>) {
        drop(self.app.update(AppEvent::Transcript {
            pane: PaneId::Main,
            record,
        }));
    }
}

fn local_record(sequence: u64, event: LocalEvent) -> Arc<TranscriptRecord> {
    Arc::new(TranscriptRecord::from_local(sequence, sequence, event).unwrap())
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
            request_id: Arc::from("benchmark"),
            seq: sequence,
            kind,
            payload: to_raw_value(&payload).unwrap(),
        },
    ))
}

fn benchmarks(criterion: &mut Criterion) {
    criterion.bench_function("tui/app_root_first_120x40_frame", |bencher| {
        bencher.iter(|| {
            let mut harness = Harness::new("", false);
            harness.render();
            black_box(harness);
        });
    });

    criterion.bench_function("tui/empty_idle_render", |bencher| {
        bencher.iter_batched_ref(
            || Harness::new("", true),
            Harness::render,
            BatchSize::SmallInput,
        );
    });

    criterion.bench_function("tui/single_character_update_and_render", |bencher| {
        bencher.iter_batched_ref(
            || Harness::new("", true),
            |harness| {
                black_box(
                    harness
                        .app
                        .update(AppEvent::Terminal(Event::Key(KeyEvent::new(
                            KeyCode::Char('x'),
                            KeyModifiers::NONE,
                        )))),
                );
                harness.render();
            },
            BatchSize::SmallInput,
        );
    });

    criterion.bench_function("tui/six_row_composer_render", |bencher| {
        bencher.iter_batched_ref(
            || Harness::new("one\ntwo\nthree\nfour\nfive\nsix", false),
            Harness::render,
            BatchSize::SmallInput,
        );
    });

    let large_draft = (0..500)
        .map(|line| format!("large draft line {line} with enough text to wrap"))
        .collect::<Vec<_>>()
        .join("\n");
    criterion.bench_function("tui/scrolled_large_draft_render", |bencher| {
        bencher.iter_batched_ref(
            || Harness::new(&large_draft, false),
            Harness::render,
            BatchSize::SmallInput,
        );
    });

    criterion.bench_function("tui/transcript_large_tail_render", |bencher| {
        bencher.iter_batched_ref(
            || Harness::with_transcript(2_000),
            Harness::render,
            BatchSize::SmallInput,
        );
    });

    criterion.bench_function("tui/transcript_page_up_and_render", |bencher| {
        bencher.iter_batched_ref(
            || Harness::with_transcript(2_000),
            |harness| {
                drop(
                    harness
                        .app
                        .update(AppEvent::Terminal(Event::Key(KeyEvent::new(
                            KeyCode::PageUp,
                            KeyModifiers::NONE,
                        )))),
                );
                harness.render();
            },
            BatchSize::SmallInput,
        );
    });

    criterion.bench_function("tui/transcript_streaming_delta_and_render", |bencher| {
        bencher.iter_batched(
            || {
                let mut harness = Harness::new("", true);
                harness.apply_record(agent_record(1, AgentEventKind::RunStarted, json!({})));
                harness
            },
            |mut harness| {
                harness.apply_record(agent_record(
                    2,
                    AgentEventKind::AssistantDelta,
                    json!({
                        "model_call_index": 1,
                        "item_id": "message",
                        "phase": "final_answer",
                        "text": "one streamed token",
                    }),
                ));
                harness.render();
                black_box(harness);
            },
            BatchSize::SmallInput,
        );
    });

    criterion.bench_function("tui/transcript_collapsed_tools_render", |bencher| {
        bencher.iter_batched_ref(
            || Harness::with_tools(100, 4 * 1024),
            Harness::render,
            BatchSize::SmallInput,
        );
    });

    criterion.bench_function("tui/transcript_expand_large_tool", |bencher| {
        bencher.iter_batched_ref(
            || Harness::with_tools(1, 50 * 1024),
            |harness| {
                drop(
                    harness
                        .app
                        .update(AppEvent::Terminal(Event::Key(KeyEvent::new(
                            KeyCode::Tab,
                            KeyModifiers::NONE,
                        )))),
                );
                drop(
                    harness
                        .app
                        .update(AppEvent::Terminal(Event::Key(KeyEvent::new(
                            KeyCode::Enter,
                            KeyModifiers::NONE,
                        )))),
                );
                harness.render();
            },
            BatchSize::SmallInput,
        );
    });

    criterion.bench_function("tui/transcript_expand_highlighted_patch", |bencher| {
        bencher.iter_batched_ref(
            || Harness::with_patch(50),
            |harness| {
                drop(
                    harness
                        .app
                        .update(AppEvent::Terminal(Event::Key(KeyEvent::new(
                            KeyCode::Tab,
                            KeyModifiers::NONE,
                        )))),
                );
                drop(
                    harness
                        .app
                        .update(AppEvent::Terminal(Event::Key(KeyEvent::new(
                            KeyCode::Enter,
                            KeyModifiers::NONE,
                        )))),
                );
                harness.render();
            },
            BatchSize::SmallInput,
        );
    });
}

criterion_group!(tui, benchmarks);
criterion_main!(tui);
