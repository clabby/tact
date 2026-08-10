//! Criterion baselines for foundational TUI operations.

#![allow(dead_code, unused_imports)]

#[path = "../app/config.rs"]
pub(crate) mod config;
#[path = "../app/error.rs"]
pub(crate) mod error;
#[path = "../app/installation.rs"]
pub(crate) mod installation;
#[path = "../core/extensions/subagents/model.rs"]
mod subagent_model;

mod app {
    pub(crate) use crate::{config, error, installation};
}

mod core {
    pub(crate) mod extensions {
        #[derive(Clone, Debug, Eq, PartialEq)]
        pub(crate) struct Skill {
            name: String,
            description: String,
        }

        impl Skill {
            pub(crate) fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
                Self {
                    name: name.into(),
                    description: description.into(),
                }
            }

            pub(crate) fn name(&self) -> &str {
                &self.name
            }

            pub(crate) fn description(&self) -> &str {
                &self.description
            }
        }

        pub(crate) mod memory {
            #[derive(Clone, Copy, Debug, Eq, PartialEq)]
            pub(crate) struct MemoryKey {
                pub(crate) id: i64,
                pub(crate) version: u64,
            }

            #[derive(Clone, Debug, Eq, PartialEq)]
            pub(crate) struct MemoryRecord {
                pub(crate) key: MemoryKey,
                pub(crate) content: String,
                pub(crate) created_at_ms: i64,
                pub(crate) updated_at_ms: i64,
                pub(crate) last_scanned_at_ms: Option<i64>,
                pub(crate) scan_count: u64,
                pub(crate) last_used_at_ms: Option<i64>,
                pub(crate) use_count: u64,
                pub(crate) probation_until_ms: Option<i64>,
            }
        }

        pub(crate) mod subagents {
            pub(crate) use crate::subagent_model::{
                AgentDescriptor, AgentId, AgentMessage, AgentMessageUpdate, AgentStatus,
                AgentThread, AgentUpdate, MessageDeliveryState, MessageDisposition, MessageId,
                MessagePriority, MessagePurpose, MessageSender, ThreadId,
            };
        }
    }
}

#[path = "components/mod.rs"]
mod components;
mod context;
mod format;
mod pane;
mod prompt;
mod session;
mod spinner;
mod storage;
mod theme;
#[path = "transcript/mod.rs"]
pub(crate) mod transcript;

// `config.rs` uses the production module path while this benchmark compiles the
// same internal modules directly into its private target.
mod tui {
    pub(crate) use crate::{
        context, format, pane, prompt, session, spinner, storage, theme, transcript,
    };
}

use components::{AppEvent, AppNode, RootNode};
use config::{ReasoningEffort, ReasoningMode};
use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use nanocodex::agent::events::{AgentEvent, AgentEventKind};
use pane::PaneId;
use ratatui::{Terminal, backend::TestBackend};
use serde_json::{json, value::to_raw_value};
use std::{
    hint::black_box,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tempfile::TempDir;
use theme::Theme;
use transcript::{LocalEvent, SessionStarted, TranscriptRecord, TurnId};

const WIDTH: u16 = 120;
const HEIGHT: u16 = 40;
const TARGET_SEMANTIC_EVENTS: u64 = 15_000;
const UNRELATED_SESSIONS: u64 = 6;
const UNRELATED_SEMANTIC_EVENTS: u64 = 4_000;

struct Harness {
    app: AppNode,
    terminal: Terminal<TestBackend>,
}

struct PersistenceFixture {
    _directory: TempDir,
    config_path: PathBuf,
    workspace: PathBuf,
    session_id: String,
}

impl PersistenceFixture {
    fn semantic_archive() -> Self {
        Self::archive()
    }

    fn mixed_archive() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        let workspace = PathBuf::from("/benchmark-workspace");
        let session_id = "mixed-session".to_owned();
        write_mixed_session_segment(&config_path, &session_id, &workspace, 1_000);
        save_benchmark_checkpoint(&config_path, &session_id);

        Self {
            _directory: directory,
            config_path,
            workspace,
            session_id,
        }
    }

    fn archive() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        let workspace = PathBuf::from("/benchmark-workspace");
        let session_id = "target-session".to_owned();

        write_session(
            &config_path,
            &session_id,
            &workspace,
            TARGET_SEMANTIC_EVENTS,
        );
        save_benchmark_checkpoint(&config_path, &session_id);

        for index in 0..UNRELATED_SESSIONS {
            let unrelated = format!("unrelated-session-{index}");
            write_session(
                &config_path,
                &unrelated,
                &workspace,
                UNRELATED_SEMANTIC_EVENTS,
            );
            save_benchmark_checkpoint(&config_path, &unrelated);
        }

        Self {
            _directory: directory,
            config_path,
            workspace,
            session_id,
        }
    }

    fn load_transcript(&self) -> Vec<Arc<TranscriptRecord>> {
        session::load_transcript(&self.config_path, &self.session_id).unwrap()
    }
}

fn mixed_projection_records(turns: u64) -> Vec<Arc<TranscriptRecord>> {
    let mut records = Vec::new();
    let mut sequence = 2_u64;
    for turn in 0..turns {
        records.push(local_record(
            sequence,
            LocalEvent::UserSubmitted {
                id: TurnId::new(turn.saturating_add(1)),
                text: format!("inspect subsystem {turn} and summarize the relevant behavior"),
            },
        ));
        sequence = sequence.saturating_add(1);
        records.push(agent_record(
            sequence,
            AgentEventKind::RunStarted,
            json!({}),
        ));
        sequence = sequence.saturating_add(1);

        for delta in 0..4 {
            records.push(agent_record(
                sequence,
                AgentEventKind::AssistantDelta,
                json!({
                    "model_call_index": turn,
                    "item_id": format!("message-{turn}"),
                    "phase": "final_answer",
                    "text": format!("turn {turn} response chunk {delta} with useful detail. "),
                }),
            ));
            sequence = sequence.saturating_add(1);
        }
        records.push(agent_record(
            sequence,
            AgentEventKind::AssistantMessage,
            json!({
                "model_call_index": turn,
                "item_id": format!("message-{turn}"),
                "phase": "final_answer",
                "text": format!("completed response for turn {turn}"),
            }),
        ));
        sequence = sequence.saturating_add(1);

        if turn % 4 == 0 {
            let call_id = format!("tool-{turn}");
            records.push(agent_record(
                sequence,
                AgentEventKind::ToolCall,
                json!({
                    "call_id": call_id,
                    "tool": "exec_command",
                    "arguments": {"cmd": format!("cargo test package-{turn}")},
                }),
            ));
            sequence = sequence.saturating_add(1);
            records.push(agent_record(
                sequence,
                AgentEventKind::ToolResult,
                json!({
                    "call_id": format!("tool-{turn}"),
                    "tool": "exec_command",
                    "status": "completed",
                    "duration_ns": 1_000_000_u64,
                    "result": {
                        "output": format!("test output for package {turn}\n").repeat(16),
                        "exit_code": 0,
                    },
                    "metadata": null,
                }),
            ));
            sequence = sequence.saturating_add(1);
        }

        records.push(agent_record(
            sequence,
            AgentEventKind::RunCompleted,
            json!({}),
        ));
        sequence = sequence.saturating_add(1);
    }
    records
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
            payload: to_raw_value(&payload).unwrap().into(),
        },
    ))
}

fn write_session(config_path: &Path, session_id: &str, workspace: &Path, event_count: u64) {
    let mut records = Vec::with_capacity(event_count.saturating_add(1) as usize);
    let started = local_record(
        1,
        LocalEvent::SessionStarted(SessionStarted {
            session_id: session_id.to_owned(),
            parent_session_id: None,
            model: nanocodex::oai::MODEL.to_owned(),
            effort: ReasoningEffort::Medium,
            reasoning_mode: ReasoningMode::Standard,
            fast_mode: false,
            workspace: workspace.to_path_buf(),
            application_version: "benchmark".to_owned(),
        }),
    );
    records.push(started);

    let padding = "x".repeat(512);
    for index in 0..event_count {
        let sequence = index.saturating_add(2);
        let record = agent_record(
            sequence,
            AgentEventKind::ToolResult,
            json!({
                "call_id": format!("benchmark-{index}"),
                "output": padding,
                "sequence": index,
            }),
        );
        records.push(record);
    }
    storage::SessionStorage::open(config_path)
        .unwrap()
        .append_records(session_id, &records)
        .unwrap();
}

fn write_mixed_session_segment(config_path: &Path, session_id: &str, workspace: &Path, turns: u64) {
    let mut records = Vec::new();
    let started = local_record(
        1,
        LocalEvent::SessionStarted(SessionStarted {
            session_id: session_id.to_owned(),
            parent_session_id: None,
            model: nanocodex::oai::MODEL.to_owned(),
            effort: ReasoningEffort::Medium,
            reasoning_mode: ReasoningMode::Standard,
            fast_mode: false,
            workspace: workspace.to_path_buf(),
            application_version: "benchmark".to_owned(),
        }),
    );
    records.push(started);
    records.extend(mixed_projection_records(turns));
    storage::SessionStorage::open(config_path)
        .unwrap()
        .append_records(session_id, &records)
        .unwrap();
}

fn save_benchmark_checkpoint(config_path: &Path, session_id: &str) {
    let snapshot = serde_json::from_value(json!({
        "version": 1,
        "model": nanocodex::oai::MODEL,
        "lineage_id": session_id,
        "prompt_cache_key": "benchmark-cache-key",
        "workspace": "/benchmark-workspace",
        "request_prefix": [
            {"type": "additional_tools", "role": "developer", "tools": []},
            {"type": "message", "role": "developer", "content": []}
        ],
        "canonical_context": {
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "benchmark"}]
        },
        "history": [{
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "benchmark"}]
        }]
    }))
    .unwrap();
    let resume_state =
        session::encode_checkpoint(&snapshot, "benchmark instructions", false).unwrap();
    storage::SessionStorage::open(config_path)
        .unwrap()
        .append_records_and_resume_state(session_id, &[], Some(&resume_state))
        .unwrap();
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
    let fixture = PersistenceFixture::semantic_archive();
    let mixed_fixture = PersistenceFixture::mixed_archive();
    let restored_records = fixture.load_transcript();
    let mixed_records = mixed_projection_records(1_000);
    let mut sessions = criterion.benchmark_group("session");
    sessions.sample_size(10);
    sessions.measurement_time(Duration::from_secs(5));

    sessions.bench_function("catalog_semantic_archive", |bencher| {
        bencher.iter(|| {
            black_box(session::list(&fixture.config_path, &fixture.workspace, true).unwrap());
        });
    });

    sessions.bench_function("load_known_semantic_transcript", |bencher| {
        bencher.iter(|| black_box(fixture.load_transcript()));
    });

    sessions.bench_function("load_mixed_transcript", |bencher| {
        bencher.iter(|| black_box(mixed_fixture.load_transcript()));
    });

    sessions.bench_function("project_semantic_transcript", |bencher| {
        bencher.iter_batched(
            || {
                (
                    RootNode::new(&fixture.workspace, ReasoningEffort::Medium),
                    restored_records.clone(),
                )
            },
            |(mut root, records)| {
                root.restore_session(
                    &fixture.workspace,
                    ReasoningEffort::Medium,
                    ReasoningMode::Standard,
                    ReasoningMode::Standard,
                    false,
                    records,
                );
                black_box(root);
            },
            BatchSize::LargeInput,
        );
    });

    sessions.bench_function("project_mixed_transcript", |bencher| {
        bencher.iter_batched(
            || {
                (
                    RootNode::new(&fixture.workspace, ReasoningEffort::Medium),
                    mixed_records.clone(),
                )
            },
            |(mut root, records)| {
                root.restore_session(
                    &fixture.workspace,
                    ReasoningEffort::Medium,
                    ReasoningMode::Standard,
                    ReasoningMode::Standard,
                    false,
                    records,
                );
                black_box(root);
            },
            BatchSize::LargeInput,
        );
    });

    sessions.bench_function("resume_semantic_transcript", |bencher| {
        bencher.iter(|| {
            let records = fixture.load_transcript();
            let mut root = RootNode::new(&fixture.workspace, ReasoningEffort::Medium);
            root.restore_session(
                &fixture.workspace,
                ReasoningEffort::Medium,
                ReasoningMode::Standard,
                ReasoningMode::Standard,
                false,
                records,
            );
            black_box(root);
        });
    });
    sessions.bench_function("resume_mixed_transcript", |bencher| {
        bencher.iter(|| {
            let records = mixed_fixture.load_transcript();
            let mut root = RootNode::new(&mixed_fixture.workspace, ReasoningEffort::Medium);
            root.restore_session(
                &mixed_fixture.workspace,
                ReasoningEffort::Medium,
                ReasoningMode::Standard,
                ReasoningMode::Standard,
                false,
                records,
            );
            black_box(root);
        });
    });

    sessions.finish();
}

criterion_group!(tui, benchmarks);
criterion_main!(tui);
