//! Criterion baselines for foundational TUI operations.
//!
//! The binary crate has no library target, so this benchmark compiles the modules it measures
//! directly. The module tree mirrors the binary's so that their crate-relative imports resolve
//! unchanged.

#![allow(dead_code, unused_imports)]

#[cfg(test)]
fn install_tls_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[path = "../src/app"]
mod app {
    pub(crate) mod auth;
    pub(crate) mod browser;
    pub(crate) mod config;
    pub(crate) mod error;
    pub(crate) mod installation;
    pub(crate) mod model;
    pub(crate) mod secret;
    pub(crate) mod theme;
    pub(crate) mod update;
}

#[path = "../src/core"]
mod core {
    pub(crate) mod context;
    pub(crate) mod live_sessions;
    pub(crate) mod pane;
    pub(crate) mod prompt;
    pub(crate) mod protocol;
    pub(crate) mod session;
    pub(crate) mod storage;
    pub(crate) mod titles;
    pub(crate) mod transcript;

    pub(crate) mod extensions {
        mod skills;

        pub(crate) use skills::{Skill, SkillMatches};
    }
}

#[path = "../src/search.rs"]
mod search;

#[path = "../src/vcs/mod.rs"]
mod vcs;

#[path = "../src/tui"]
mod tui {
    pub(crate) mod components;
    pub(crate) mod format;
    pub(crate) mod spinner;
}

#[path = "../src/web"]
mod web {
    pub(crate) mod bridge;
    pub(crate) mod tailscale;
}

use crate::{
    app::{
        config::{ReasoningEffort, ReasoningMode, Speed},
        theme::Theme,
    },
    core::{
        context::ContextDiagnostics,
        pane::PaneId,
        session, storage,
        transcript::{LocalEvent, SessionStarted, TranscriptRecord, TurnId, UserSubmitted},
    },
    tui::components::{AppEvent, AppNode, RootNode},
};
use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use nanocode::{
    Lineage, ModelSelection, SessionCheckpoint, Thinking,
    agent::events::{AgentEvent, AgentEventKind},
    durability::{
        OwnerId, SessionRecord, SessionStore as SessionCatalog, SessionWrite, StateCommit,
        StateStore, SummaryUpdate,
    },
};
use ratatui::{Terminal, backend::TestBackend};
use rusqlite::Connection;
use serde_json::{json, value::to_raw_value};
use std::{
    hint::black_box,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tempfile::TempDir;

const WIDTH: u16 = 120;
const HEIGHT: u16 = 40;
const TARGET_SEMANTIC_EVENTS: u64 = 15_000;
const UNRELATED_SESSIONS: u64 = 6;
const UNRELATED_SEMANTIC_EVENTS: u64 = 4_000;
const BATCH_PROMPTS: u64 = 19;
/// About 256 KiB of conversation history in a commit benchmark's resume snapshot.
const RESUME_HISTORY_ITEMS: usize = 256;

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
        session::SessionStore::new(&self.config_path)
            .load_transcript(&self.session_id)
            .unwrap()
            .into_records()
    }
}

fn mixed_projection_records(turns: u64) -> Vec<Arc<TranscriptRecord>> {
    let mut records = Vec::new();
    let mut sequence = 2_u64;
    for turn in 0..turns {
        records.push(local_record(
            sequence,
            LocalEvent::UserSubmitted(UserSubmitted {
                id: TurnId::new(turn.saturating_add(1)),
                text: format!("inspect subsystem {turn} and summarize the relevant behavior"),
            }),
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
            let output = format!("test output for package {turn}\n").repeat(16);
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
                    "result": format!(
                        "Wall time: 0.0000 seconds\nProcess exited with code 0\nOutput:\n{output}"
                    ),
                    "structured_result": {
                        "output": output.as_str(),
                        "exit_code": 0,
                        "wall_time_seconds": 0.0,
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
                LocalEvent::UserSubmitted(UserSubmitted {
                    id: TurnId::new(sequence),
                    text: format!("transcript entry {sequence} with enough text to wrap"),
                }),
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
                    "result": format!(
                        "Wall time: 0.0000 seconds\nProcess exited with code 0\nOutput:\n{output}"
                    ),
                    "structured_result": {
                        "output": output.as_str(),
                        "exit_code": 0,
                        "wall_time_seconds": 0.0,
                    },
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
                "structured_result": {},
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
            parent_sequence: None,
            model: nanocode::oai::MODEL.to_owned(),
            effort: ReasoningEffort::Medium,
            reasoning_mode: ReasoningMode::Standard,
            speed: Speed::Standard,
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
    commit_fixture(config_path, session_id, &records, None);
}

fn write_mixed_session_segment(config_path: &Path, session_id: &str, workspace: &Path, turns: u64) {
    let mut records = Vec::new();
    let started = local_record(
        1,
        LocalEvent::SessionStarted(SessionStarted {
            session_id: session_id.to_owned(),
            parent_session_id: None,
            parent_sequence: None,
            model: nanocode::oai::MODEL.to_owned(),
            effort: ReasoningEffort::Medium,
            reasoning_mode: ReasoningMode::Standard,
            speed: Speed::Standard,
            workspace: workspace.to_path_buf(),
            application_version: "benchmark".to_owned(),
        }),
    );
    records.push(started);
    records.extend(mixed_projection_records(turns));
    commit_fixture(config_path, session_id, &records, None);
}

fn save_benchmark_checkpoint(config_path: &Path, session_id: &str) {
    commit_fixture(
        config_path,
        session_id,
        &[],
        Some(benchmark_resume_state(session_id, 1)),
    );
}

/// An encoded resume state whose conversation history holds `history_items` messages of about
/// 1 KiB each.
fn benchmark_resume_state(session_id: &str, history_items: usize) -> Vec<u8> {
    let history = (0..history_items)
        .map(|index| {
            json!({
                "type": "message",
                "role": if index % 2 == 0 { "user" } else { "assistant" },
                "content": [{"type": "input_text", "text": format!("{index} {}", "y".repeat(1_000))}]
            })
        })
        .collect::<Vec<_>>();
    let conversation = json!({
        "version": 1,
        "model": nanocode::oai::MODEL,
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
        "history": history
    });
    let snapshot = SessionCheckpoint::native(
        session_id,
        ModelSelection::Codex(nanocode::Model::Astra),
        Thinking::Medium,
        Lineage::root(session_id),
        session_id,
        None,
        true,
        json!({"service_tier": "standard", "conversation": conversation}),
    );
    session::encode_checkpoint(
        &session::AgentSnapshot::from(snapshot),
        "benchmark instructions",
        false,
    )
    .unwrap()
}

/// Commits records and an optional resume state through the session store in one transaction,
/// as a journal batch does.
fn commit_fixture(
    config_path: &Path,
    session_id: &str,
    records: &[Arc<TranscriptRecord>],
    resume_state: Option<Vec<u8>>,
) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let mut store = storage::SqliteStateStore::new(config_path).unwrap();
        let owned = store.acquire(session_id, OwnerId::new()).await.unwrap();
        let (events, rows) = storage::transcript_commit(records.iter().map(AsRef::as_ref)).unwrap();
        store
            .stage(session_id, storage::HostBatch { rows, resume_state })
            .unwrap();
        store
            .commit(
                session_id,
                &owned.owner,
                owned.state.revision,
                StateCommit {
                    payload: "{}",
                    records: &[],
                    events: &events,
                    summary: &SummaryUpdate::Unchanged,
                },
            )
            .await
            .unwrap();
    });
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
            black_box(
                session::SessionStore::new(&fixture.config_path)
                    .list(std::slice::from_ref(&fixture.workspace), true)
                    .unwrap(),
            );
        });
    });

    sessions.bench_function("load_known_semantic_transcript", |bencher| {
        bencher.iter(|| black_box(fixture.load_transcript()));
    });

    sessions.bench_function("load_mixed_transcript", |bencher| {
        bencher.iter(|| black_box(mixed_fixture.load_transcript()));
    });

    sessions.bench_function("observe_semantic_context", |bencher| {
        bencher.iter(|| {
            let mut diagnostics = ContextDiagnostics::default();
            for record in &restored_records {
                black_box(diagnostics.observe(record));
            }
            black_box(diagnostics);
        });
    });

    sessions.bench_function("observe_mixed_context", |bencher| {
        bencher.iter(|| {
            let mut diagnostics = ContextDiagnostics::default();
            for record in &mixed_records {
                black_box(diagnostics.observe(record));
            }
            black_box(diagnostics);
        });
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
                    Speed::Standard,
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
                    Speed::Standard,
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
                Speed::Standard,
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
                Speed::Standard,
                records,
            );
            black_box(root);
        });
    });

    // Steady-state commits of one prebuilt journal batch: prompts, a large tool result and a
    // realistic resume snapshot. Both variants commit the same inputs; the session variant adds
    // the durable session driver that the journal writer uses.
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let commit_directory = tempfile::tempdir().unwrap();
    let commit_config = commit_directory.path().join("config.toml");
    let tool_output = "x".repeat(64 * 1024);
    let batch = (0..BATCH_PROMPTS)
        .map(|index| {
            local_record(
                index.saturating_add(2),
                LocalEvent::UserSubmitted(UserSubmitted {
                    id: TurnId::new(index),
                    text: format!("benchmark prompt {index}"),
                }),
            )
        })
        .chain(std::iter::once(agent_record(
            BATCH_PROMPTS.saturating_add(2),
            AgentEventKind::ToolResult,
            json!({"call_id": "call", "output": tool_output}),
        )))
        .collect::<Vec<_>>();
    let (batch_events, batch_rows) =
        storage::transcript_commit(batch.iter().map(AsRef::as_ref)).unwrap();
    let batch_host = storage::HostBatch {
        rows: batch_rows,
        resume_state: Some(benchmark_resume_state("commit", RESUME_HISTORY_ITEMS)),
    };
    eprintln!(
        "session/commit_batch_*: {} events, {} resume bytes",
        batch_events.len(),
        batch_host.resume_state.as_ref().map_or(0, Vec::len)
    );

    let start = |session_id: &str| {
        vec![local_record(
            1,
            LocalEvent::SessionStarted(benchmark_start(session_id, commit_directory.path())),
        )]
    };
    commit_fixture(&commit_config, "store", &start("store"), None);
    let mut store = storage::SqliteStateStore::new(&commit_config).unwrap();
    let owned = runtime
        .block_on(store.acquire("store", OwnerId::new()))
        .unwrap();
    let first_revision = owned.state.revision;
    let mut revision = first_revision;
    let mut iterations = 0_u64;
    sessions.bench_function("commit_batch_store", |bencher| {
        bencher.iter(|| {
            store.stage("store", batch_host.clone()).unwrap();
            revision = runtime
                .block_on(store.commit(
                    "store",
                    &owned.owner,
                    revision,
                    StateCommit {
                        payload: "{}",
                        records: &[],
                        events: &batch_events,
                        summary: &SummaryUpdate::Unchanged,
                    },
                ))
                .unwrap()
                .revision;
            iterations += 1;
        });
    });
    eprintln!(
        "session/commit_batch_store: {} commits in {iterations} iterations",
        revision - first_revision
    );

    let handle = store.writer_handle();
    let catalog = {
        let _runtime = runtime.enter();
        SessionCatalog::new(handle.clone()).unwrap()
    };
    let durable = runtime
        .block_on(catalog.session(SessionRecord::root(
            "session",
            ModelSelection::Codex(nanocode::Model::Astra),
            Some(commit_directory.path().to_path_buf()),
        )))
        .unwrap();
    let (start_events, _) =
        storage::transcript_commit(start("session").iter().map(AsRef::as_ref)).unwrap();
    let first_revision = runtime
        .block_on(durable.append(SessionWrite {
            events: start_events,
            metadata: None,
        }))
        .unwrap()
        .revision;
    let mut revision = first_revision;
    let mut iterations = 0_u64;
    sessions.bench_function("commit_batch_session", |bencher| {
        bencher.iter(|| {
            handle.stage("session", batch_host.clone()).unwrap();
            revision = runtime
                .block_on(durable.append(SessionWrite {
                    events: batch_events.clone(),
                    metadata: None,
                }))
                .unwrap()
                .revision;
            iterations += 1;
        });
    });
    eprintln!(
        "session/commit_batch_session: {} commits in {iterations} iterations",
        revision - first_revision
    );

    // The first write to a session stored before the durability contract opens the store and
    // numbers every stored row once.
    let legacy_directory = tempfile::tempdir().unwrap();
    let legacy_config = legacy_directory.path().join("config.toml");
    write_session(
        &legacy_config,
        "legacy",
        legacy_directory.path(),
        TARGET_SEMANTIC_EVENTS,
    );
    let legacy_database = storage::database_path(&legacy_config);
    Connection::open(&legacy_database)
        .unwrap()
        .execute_batch(
            "UPDATE events SET seq = NULL, revision = NULL;
             DELETE FROM durable_event_sequences; DELETE FROM durable_states;
             DELETE FROM durable_owners; PRAGMA wal_checkpoint(TRUNCATE);",
        )
        .unwrap();
    sessions.bench_function("first_write_adopts_legacy_session", |bencher| {
        bencher.iter_batched(
            || {
                let directory = tempfile::tempdir().unwrap();
                let config = directory.path().join("config.toml");
                let database = storage::database_path(&config);
                std::fs::create_dir_all(database.parent().unwrap()).unwrap();
                std::fs::copy(&legacy_database, &database).unwrap();
                (directory, config)
            },
            |(directory, config)| {
                runtime.block_on(async {
                    let mut store = storage::SqliteStateStore::new(&config).unwrap();
                    black_box(store.acquire("legacy", OwnerId::new()).await.unwrap());
                });
                directory
            },
            BatchSize::PerIteration,
        );
    });

    sessions.finish();
}

fn benchmark_start(session_id: &str, workspace: &Path) -> SessionStarted {
    SessionStarted {
        session_id: session_id.to_owned(),
        parent_session_id: None,
        parent_sequence: None,
        model: nanocode::oai::MODEL.to_owned(),
        effort: ReasoningEffort::Medium,
        reasoning_mode: ReasoningMode::Standard,
        speed: Speed::Standard,
        workspace: workspace.to_path_buf(),
        application_version: "benchmark".to_owned(),
    }
}

criterion_group!(tui, benchmarks);
criterion_main!(tui);
