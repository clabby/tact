//! The document the monitor's model reads: every live session's state and the tail of its
//! transcript, and the notes of the previous check.
//!
//! Transcripts are untrusted data written by agents and tools. The document is JSON, so their text
//! never becomes part of the instructions, and every string and the whole document are bounded.
//! Draft text, images, and tool output beyond an exit code, a test summary, or one failure line
//! are never included.

use super::{Attention, Check, Fingerprint, SessionMark, blocks::numbers_in};
use crate::{
    core::transcript::{EntryKind, ToolEntry, ToolState, TranscriptEntry, TransientStatus},
    web::{
        activity::{Excerpt, clip},
        hub::SessionView,
        outcome::ToolOutcome,
        wire::{SummaryState, tool_summary},
    },
};
use chrono::{DateTime, SecondsFormat};
use serde::Serialize;
use serde_json::Value;
use std::{cmp::Reverse, collections::HashSet};
use tact_subagents::AgentStatus;

/// Sessions beyond this many are left out, least recently active first.
pub(super) const MAX_SESSIONS: usize = 16;
/// The transcript budget of all sessions together, in bytes of JSON.
const TOTAL_BUDGET: usize = 80_000;
/// The most one session may use, in bytes of JSON, when there are few sessions.
const SESSION_BUDGET: usize = 10_000;
/// The newest prompts every session includes, whatever its budget.
const KEPT_PROMPTS: usize = 2;
const MAX_USER_CHARS: usize = 1_500;
const MAX_ASSISTANT_CHARS: usize = 1_200;
/// The newest complete message is usually the agent's report or question, so more of it is kept.
const FINAL_MESSAGE: Excerpt = Excerpt {
    limit: 2_000,
    head: 1_500,
    tail: 500,
};
const MAX_ERROR_CHARS: usize = 600;
const MAX_RESULT_CHARS: usize = 160;
const MAX_TITLE_CHARS: usize = 200;
const MAX_PATH_CHARS: usize = 300;
const MAX_STATUS_CHARS: usize = 300;
const MAX_PLAN_STEPS: usize = 20;
const MAX_PLAN_STEP_CHARS: usize = 160;
const MAX_SUBAGENTS: usize = 16;
const MAX_ROLE_CHARS: usize = 80;

/// The serialized document and the sessions it describes.
pub(super) struct Input {
    pub(super) text: String,
    pub(super) sessions: Vec<InputSession>,
}

/// A session the document describes.
pub(super) struct InputSession {
    pub(super) session: String,
    /// Its transcript items are numbered from 1 to this.
    pub(super) items: usize,
    /// Every digit run in what the model reads about this session, excluding the transcript
    /// items' own sequence numbers. Figures in the model's blocks must come from here.
    pub(super) numbers: HashSet<String>,
    /// The agent's newest complete message, whose wording orders a question's options.
    pub(super) message: Option<String>,
}

#[derive(Serialize)]
struct Document<'a> {
    now: String,
    previous_check: Option<PreviousCheck<'a>>,
    sessions: Vec<SessionInput>,
}

#[derive(Serialize)]
struct PreviousCheck<'a> {
    as_of: String,
    sessions: Vec<PreviousNote<'a>>,
}

#[derive(Serialize)]
struct PreviousNote<'a> {
    session: &'a str,
    attention: Attention,
    goal: Option<&'a str>,
    health: Option<u8>,
}

#[derive(Serialize)]
struct SessionInput {
    session: String,
    title: String,
    model: String,
    workspace: String,
    state: SummaryState,
    unread: bool,
    has_draft: bool,
    queued_prompts: usize,
    turn_running_seconds: Option<u64>,
    last_activity_seconds_ago: u64,
    status: Option<String>,
    plan: Vec<PlanStep>,
    /// Newest first.
    subagents: Vec<Subagent>,
    /// Newest first.
    transcript: Vec<Numbered>,
}

#[derive(Serialize)]
struct PlanStep {
    step: String,
    status: String,
}

#[derive(Serialize)]
struct Subagent {
    role: String,
    status: &'static str,
}

/// A transcript item with its sequence number in this session's input: 1 for the oldest item
/// included, larger for newer ones. History steps in the reply cite the number as their `at`, which lets the
/// server order them without trusting the model's order.
#[derive(Serialize)]
struct Numbered {
    n: usize,
    #[serde(flatten)]
    item: Item,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Item {
    User {
        text: String,
    },
    Assistant {
        text: String,
        commentary: bool,
    },
    Tool {
        name: String,
        summary: String,
        state: &'static str,
        result: Option<String>,
    },
    /// A message another live session sent to this one.
    SessionMessage {
        from: String,
        text: String,
    },
    Interrupted,
    Error {
        text: String,
    },
}

/// Builds the document from the live sessions at `now_ms`.
pub(super) fn build(views: &[SessionView<'_>], previous: Option<&Check>, now_ms: u64) -> Input {
    let mut chosen: Vec<usize> = (0..views.len()).collect();
    if chosen.len() > MAX_SESSIONS {
        chosen.sort_by_key(|&index| Reverse(views[index].summary.last_activity_unix_ms));
        chosen.truncate(MAX_SESSIONS);
        chosen.sort_unstable();
    }
    let budget = SESSION_BUDGET.min(TOTAL_BUDGET / chosen.len().max(1));
    let (sessions, described): (Vec<SessionInput>, Vec<InputSession>) = chosen
        .iter()
        .map(|&index| session(&views[index], budget, now_ms))
        .unzip();
    let document = Document {
        now: rfc3339(now_ms),
        previous_check: previous.map(|check| PreviousCheck {
            as_of: rfc3339(check.as_of_ms),
            sessions: check
                .sessions
                .iter()
                .map(|note| PreviousNote {
                    session: &note.session,
                    attention: note.attention,
                    goal: note.goal.as_deref(),
                    health: note.health,
                })
                .collect(),
        }),
        sessions,
    };
    Input {
        text: serde_json::to_string(&document).expect("the document serializes to JSON"),
        sessions: described,
    }
}

/// What changed in the sessions since a check read them. Elapsed time is deliberately absent.
pub(super) fn fingerprint(views: &[SessionView<'_>]) -> Fingerprint {
    Fingerprint(
        views
            .iter()
            .map(|view| SessionMark {
                session: view.summary.id.clone(),
                transcript: view.transcript_revision,
                state: view.summary.state,
                queued: view.queued,
                unread: view.summary.unread,
                agents: view
                    .roster
                    .agents
                    .iter()
                    .map(|agent| (agent.id, agent_status(&agent.status)))
                    .collect(),
            })
            .collect(),
    )
}

fn session(view: &SessionView<'_>, budget: usize, now_ms: u64) -> (SessionInput, InputSession) {
    let summary = &view.summary;
    let model = view.model;
    let mut input = SessionInput {
        session: summary.id.clone(),
        title: clip(&summary.title, MAX_TITLE_CHARS),
        model: clip(&summary.model, MAX_TITLE_CHARS),
        workspace: clip(&summary.workspace, MAX_PATH_CHARS),
        state: summary.state,
        unread: summary.unread,
        has_draft: summary.has_draft,
        queued_prompts: view.queued,
        turn_running_seconds: model
            .turn_started_at_unix_ms()
            .map(|started| now_ms.saturating_sub(started) / 1_000),
        last_activity_seconds_ago: now_ms.saturating_sub(summary.last_activity_unix_ms) / 1_000,
        status: model.transient().map(status_label),
        plan: Vec::new(),
        subagents: Vec::new(),
        transcript: Vec::new(),
    };
    // Everything about the session shares its budget. The newest prompts get their share first,
    // the plan and the subagents may take half of what is left, and the rest of the transcript
    // fills the remainder.
    let entries = model.entries();
    let available = budget.saturating_sub(json_len(&input));
    let prompts = kept_prompts(entries, available / 2 / KEPT_PROMPTS);
    let prompt_bytes: usize = prompts.iter().map(|(_, prompt)| json_len(prompt)).sum();
    let details = json_len(&input) + available.saturating_sub(prompt_bytes) / 2;
    for step in plan(entries) {
        input.plan.push(step);
        if json_len(&input) > details {
            input.plan.pop();
            break;
        }
    }
    for agent in view.roster.agents.iter().rev().take(MAX_SUBAGENTS) {
        input.subagents.push(Subagent {
            role: clip(&agent.role, MAX_ROLE_CHARS),
            status: agent_status(&agent.status),
        });
        if json_len(&input) > details {
            input.subagents.pop();
            break;
        }
    }
    let fields = to_json(&input);
    input.transcript = transcript(
        entries,
        prompts,
        budget.saturating_sub(fields.len() + prompt_bytes),
    );
    let mut numbers = HashSet::new();
    numbers_in(&fields, &mut numbers);
    for numbered in &input.transcript {
        numbers_in(&to_json(&numbered.item), &mut numbers);
    }
    let described = InputSession {
        session: input.session.clone(),
        items: input.transcript.len(),
        numbers,
        message: model
            .entries()
            .iter()
            .rev()
            .find_map(|entry| match &entry.kind {
                EntryKind::Assistant {
                    text,
                    complete: true,
                } if !entry.hidden => Some(text.clone()),
                _ => None,
            }),
    };
    (input, described)
}

/// The newest prompts, newest first, each shortened to an entry of at most `limit` bytes. They
/// are always kept, so the model knows what the developer asked for even when the agent has been
/// busy for a long time.
fn kept_prompts(entries: &[TranscriptEntry], limit: usize) -> Vec<(usize, Numbered)> {
    entries
        .iter()
        .enumerate()
        .rev()
        .filter_map(|(index, entry)| match &entry.kind {
            EntryKind::User { text, .. } if !entry.hidden => {
                Some((index, fitted_prompt(text, limit, entries.len())))
            }
            _ => None,
        })
        .take(KEPT_PROMPTS)
        .collect()
}

/// The kept `prompts` and the newest other entries that fit `budget` bytes, newest first. The
/// calls a Code Mode cell made are entries of their own beside the cell, so their commands and
/// outcomes reach the model like any other call.
fn transcript(
    entries: &[TranscriptEntry],
    mut prompts: Vec<(usize, Numbered)>,
    budget: usize,
) -> Vec<Numbered> {
    // Entries are sized with the largest number they could carry, so numbering them afterwards
    // never makes them larger.
    let largest = entries.len();
    let newest_message = entries.iter().rposition(|entry| {
        !entry.hidden && matches!(entry.kind, EntryKind::Assistant { complete: true, .. })
    });
    let mut used = 0;
    let mut items = Vec::new();
    let mut full = false;
    for (index, entry) in entries.iter().enumerate().rev() {
        if entry.hidden {
            continue;
        }
        if prompts.first().is_some_and(|(kept, _)| *kept == index) {
            items.push(prompts.remove(0).1);
            if full && prompts.is_empty() {
                break;
            }
            continue;
        }
        if full {
            continue;
        }
        let Some(item) = Item::of(entry, Some(index) == newest_message) else {
            continue;
        };
        let item = Numbered { n: largest, item };
        let size = json_len(&item);
        if used + size > budget {
            full = true;
            if prompts.is_empty() {
                break;
            }
            continue;
        }
        used += size;
        items.push(item);
    }
    let count = items.len();
    for (index, item) in items.iter_mut().enumerate() {
        item.n = count - index;
    }
    items
}

/// A prompt cut on a character boundary to the longest excerpt whose entry, escaped as JSON, takes
/// at most `limit` bytes. An entry too small for any text keeps an empty excerpt.
fn fitted_prompt(text: &str, limit: usize, n: usize) -> Numbered {
    let entry = |chars: usize| Numbered {
        n,
        item: Item::User {
            text: clip(text, chars),
        },
    };
    let (mut shortest, mut longest) = (0, text.chars().count().min(MAX_USER_CHARS));
    while shortest < longest {
        let middle = (shortest + longest).div_ceil(2);
        if json_len(&entry(middle)) <= limit {
            shortest = middle;
        } else {
            longest = middle - 1;
        }
    }
    entry(shortest)
}

impl Item {
    fn of(entry: &TranscriptEntry, newest_message: bool) -> Option<Self> {
        Some(match &entry.kind {
            EntryKind::User { text, .. } => Self::User {
                text: clip(text, MAX_USER_CHARS),
            },
            EntryKind::Assistant {
                text,
                complete: true,
            } => Self::Assistant {
                text: if newest_message {
                    FINAL_MESSAGE.apply(text)
                } else {
                    clip(text, MAX_ASSISTANT_CHARS)
                },
                commentary: false,
            },
            EntryKind::Tool(tool) => Self::Tool {
                name: clip(&tool.name, MAX_ROLE_CHARS),
                summary: tool_summary(tool),
                state: match tool.state {
                    ToolState::Running => "running",
                    ToolState::Succeeded => "succeeded",
                    ToolState::Failed => "failed",
                },
                result: tool_result(tool),
            },
            EntryKind::SessionMessage {
                from_session_id,
                text,
            } => Self::SessionMessage {
                from: clip(from_session_id, MAX_TITLE_CHARS),
                text: clip(text, MAX_ERROR_CHARS),
            },
            EntryKind::Interrupted { .. } => Self::Interrupted,
            EntryKind::Error { message } | EntryKind::ContextCompactionFailed { message } => {
                Self::Error {
                    text: clip(message, MAX_ERROR_CHARS),
                }
            }
            _ => return None,
        })
    }
}

/// How a finished call ended in a few words: the exit code and test summary of a command, or the
/// first line of a failure.
fn tool_result(tool: &ToolEntry) -> Option<String> {
    if tool.state == ToolState::Running {
        return None;
    }
    let result = tool.result.as_ref()?;
    let failed = tool.state == ToolState::Failed;
    if let Some(outcome) = ToolOutcome::from_result(result) {
        let mut parts = Vec::new();
        if let Some(code) = outcome.exit_code {
            parts.push(format!("exit {code}"));
        }
        match outcome.summary {
            Some(summary) => parts.push(summary),
            None if failed => parts.extend(outcome.tail.last().cloned()),
            None => {}
        }
        return (!parts.is_empty()).then(|| clip(&parts.join(" · "), MAX_RESULT_CHARS));
    }
    if !failed {
        return None;
    }
    let text = match result {
        Value::String(text) => text.as_str(),
        other => other.get("error").and_then(Value::as_str)?,
    };
    let line = text.lines().find(|line| !line.trim().is_empty())?;
    Some(clip(line.trim(), MAX_RESULT_CHARS))
}

/// The steps of the newest `update_plan` call.
fn plan(entries: &[TranscriptEntry]) -> Vec<PlanStep> {
    let Some(steps) = entries.iter().rev().find_map(|entry| match &entry.kind {
        EntryKind::Tool(tool) if tool.name == "update_plan" => {
            tool.arguments.get("plan").and_then(Value::as_array)
        }
        _ => None,
    }) else {
        return Vec::new();
    };
    let text = |step: &Value, key: &str, max: usize| {
        step.get(key)
            .and_then(Value::as_str)
            .map(|text| clip(text, max))
            .unwrap_or_default()
    };
    steps
        .iter()
        .take(MAX_PLAN_STEPS)
        .map(|step| PlanStep {
            step: text(step, "step", MAX_PLAN_STEP_CHARS),
            status: text(step, "status", MAX_ROLE_CHARS),
        })
        .collect()
}

fn status_label(status: &TransientStatus) -> String {
    match status {
        TransientStatus::Thinking => "thinking".to_owned(),
        TransientStatus::Responding => "responding".to_owned(),
        TransientStatus::Warming => "warming up".to_owned(),
        TransientStatus::WaitingForBackgroundWork => "waiting for background work".to_owned(),
        TransientStatus::Compacting => "compacting context".to_owned(),
        TransientStatus::Connecting => "connecting".to_owned(),
        TransientStatus::Reconnecting => "reconnecting".to_owned(),
        TransientStatus::Tool(name) => format!("tool {}", clip(name, MAX_ROLE_CHARS)),
        TransientStatus::Retrying {
            next_attempt,
            max_attempts,
            ..
        } => format!("retrying {next_attempt}/{max_attempts}"),
        TransientStatus::Error(message) => {
            format!("error: {}", clip(message, MAX_STATUS_CHARS))
        }
    }
}

pub(super) const fn agent_status(status: &AgentStatus) -> &'static str {
    match status {
        AgentStatus::Pending => "pending",
        AgentStatus::Running => "running",
        AgentStatus::Completed { .. } => "completed",
        AgentStatus::Interrupted => "interrupted",
        AgentStatus::Failed { .. } => "failed",
        AgentStatus::Closing => "closing",
        AgentStatus::Closed => "closed",
    }
}

fn rfc3339(unix_ms: u64) -> String {
    i64::try_from(unix_ms)
        .ok()
        .and_then(DateTime::from_timestamp_millis)
        .unwrap_or_default()
        .to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn json_len(value: &impl Serialize) -> usize {
    to_json(value).len()
}

fn to_json(value: &impl Serialize) -> String {
    serde_json::to_string(value).expect("the document's parts serialize to JSON")
}
