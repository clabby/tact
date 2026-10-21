//! What each live session is doing right now, as the Monitor shows it.
//!
//! An activity condenses one session's projection: its status, the newest prompt, message, and
//! tool call, tallies of the current turn, plan progress, the latest error, and counts of
//! subagents and queued prompts. The hub recomputes a session's activity only after a publication
//! touched that session, and a recomputation reads only the current turn plus a few remembered
//! entries of earlier turns, so its cost does not grow with the length of the history.

use super::{
    hub::elapsed_ns_since,
    outcome::ToolOutcome,
    wire::{WireStatus, WireToolState, tool_summary},
};
use crate::core::{
    context::ContextBudget,
    transcript::{EntryId, EntryKind, ToolEntry, ToolState, TranscriptEntry, TranscriptModel},
};
use serde::{Serialize, Serializer};
use serde_json::Value;
use tact_subagents::{AgentStatus, SubagentRoster};

const MAX_PROMPT_CHARS: usize = 600;
const MAX_ERROR_CHARS: usize = 600;
const MAX_PLAN_STEP_CHARS: usize = 200;
/// Longer messages keep their opening and their ending, where a closing question usually is.
const MESSAGE_EXCERPT: Excerpt = Excerpt {
    limit: 1_800,
    head: 1_200,
    tail: 500,
};

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(super) struct SessionActivity {
    session: String,
    status: Option<WireStatus>,
    turn_elapsed_ns: TurnStart,
    prompt: Option<ActivityPrompt>,
    message: Option<ActivityMessage>,
    tool: Option<ActivityTool>,
    turn: TurnTally,
    plan: Option<PlanProgress>,
    error: Option<String>,
    agents: AgentCounts,
    queued: usize,
    context: Option<ContextBudget>,
}

/// When the oldest running turn started. Activities compare by start time, so the passage of
/// time alone never makes one differ; it serializes as the time elapsed when the event is written.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TurnStart(Option<u64>);

impl Serialize for TurnStart {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.map(elapsed_ns_since).serialize(serializer)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct ActivityPrompt {
    text: String,
    at_ms: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct ActivityMessage {
    text: String,
    /// The transcript model does not record the message phase, so this is always false, as in
    /// transcript entries.
    commentary: bool,
    at_ms: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct ActivityTool {
    name: String,
    summary: String,
    state: WireToolState,
    at_ms: Option<u64>,
    outcome: Option<ToolOutcome>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
struct TurnTally {
    tools: usize,
    failed: usize,
    patches: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct PlanProgress {
    done: usize,
    total: usize,
    current: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
struct AgentCounts {
    running: usize,
    total: usize,
}

/// The parts of a live session an activity is computed from.
pub(super) struct ActivitySource<'a> {
    pub(super) session: &'a str,
    pub(super) model: &'a TranscriptModel,
    pub(super) roster: &'a SubagentRoster,
    pub(super) queued: usize,
    pub(super) context: Option<ContextBudget>,
}

/// One session's activity and what its recomputation reuses.
#[derive(Default)]
pub(super) struct ActivityCache {
    /// `None` once a publication touched the session since the activity was computed.
    current: Option<SessionActivity>,
    earlier: Option<Earlier>,
    /// The newest tool call's projection and the revision it was made from. Summaries and
    /// outcomes read whole patches and outputs, so they are made once per revision.
    tool: Option<(EntryId, u64, ActivityTool)>,
    /// How often the whole history before the newest prompt was scanned, so tests can tell a
    /// reused memo from a rescan.
    #[cfg(test)]
    pub(super) full_scans: usize,
}

/// The newest qualifying entries before the prompt `boundary`, for a turn that has not produced
/// its own yet.
///
/// History before a prompt can still change after the prompt arrives, in three ways:
///
/// - A tool that was running before the prompt changes in place when it ends. Its state is read
///   through the remembered id, but a hidden `wait` can also become a newly visible entry the
///   memo does not remember: it is shown when it fails.
/// - A Code Mode cell is hidden in place behind its first call, which
///   [`Earlier::still_qualifies`] notices when the memo remembers the cell.
/// - Entries cross the prompt, which moves it to another position: a cell that started before the
///   prompt and makes more calls after it regroups them under itself, even when its finished run
///   left it running and it no longer counts as a running tool; old message threads are removed.
///
/// A memo is therefore reused only when no tool before its prompt that can change the shape of
/// the history, a Code Mode cell or a hidden `wait`, was running when it was computed
/// (`settled`), its prompt is still at `boundary_index`, and its entries still qualify. Other
/// running tools, such as a shell, only change in place. Otherwise the history before the prompt
/// is scanned again, which reads every entry before it. Settled history costs one lookup per
/// running tool and nothing more; while a cell or a hidden `wait` before the prompt runs, or a
/// cell keeps regrouping calls across it, every recomputation rescans.
struct Earlier {
    boundary: EntryId,
    boundary_index: usize,
    settled: bool,
    message: Option<EntryId>,
    tool: Option<EntryId>,
    plan: Option<EntryId>,
}

/// Indices of the newest qualifying entries found by a scan.
#[derive(Default)]
struct Newest {
    message: Option<usize>,
    tool: Option<usize>,
    plan: Option<usize>,
}

impl ActivityCache {
    /// Marks the activity stale after a publication touched the session.
    pub(super) fn invalidate(&mut self) {
        self.current = None;
    }

    /// Forgets everything derived from the transcript, after its entries were removed or the
    /// transcript was replaced, so remembered entry ids may no longer mean the same entries.
    pub(super) fn reset(&mut self) {
        *self = Self::default();
    }

    pub(super) fn get(&mut self, source: &ActivitySource<'_>) -> &SessionActivity {
        if self.current.is_none() {
            self.current = Some(self.compute(source));
        }
        self.current.as_ref().expect("computed above")
    }

    fn compute(&mut self, source: &ActivitySource<'_>) -> SessionActivity {
        let model = source.model;
        let entries = model.entries();
        let mut newest = Newest::default();
        let mut turn = TurnTally::default();
        let mut prompt = None;
        for (index, entry) in entries.iter().enumerate().rev() {
            if matches!(entry.kind, EntryKind::User { .. }) && !entry.hidden {
                prompt = Some(index);
                break;
            }
            newest.note(index, entry);
            if let Some(tool) = top_level_tool(entry) {
                turn.tools += 1;
                turn.failed += usize::from(tool.state == ToolState::Failed);
                turn.patches +=
                    usize::from(tool.name == "apply_patch" && tool.state == ToolState::Succeeded);
            }
        }
        let complete = newest.message.is_some() && newest.tool.is_some() && newest.plan.is_some();
        if let Some(prompt) = prompt.filter(|_| !complete) {
            let earlier = self.earlier(model, prompt);
            let index = |id: Option<EntryId>| id.and_then(|id| model.index_of(id));
            newest.message = newest.message.or_else(|| index(earlier.message));
            newest.tool = newest.tool.or_else(|| index(earlier.tool));
            newest.plan = newest.plan.or_else(|| index(earlier.plan));
        }

        let status = model.transient().map(WireStatus::from);
        let error = match &status {
            Some(WireStatus::Error { message }) => Some(clip(message, MAX_ERROR_CHARS)),
            _ => entries
                .iter()
                .rev()
                .find(|entry| !entry.hidden)
                .and_then(|entry| match &entry.kind {
                    EntryKind::Error { message } => Some(clip(message, MAX_ERROR_CHARS)),
                    _ => None,
                }),
        };
        SessionActivity {
            session: source.session.to_owned(),
            status,
            turn_elapsed_ns: TurnStart(model.turn_started_at_unix_ms()),
            prompt: prompt.and_then(|index| {
                let entry = &entries[index];
                let EntryKind::User { text, .. } = &entry.kind else {
                    return None;
                };
                Some(ActivityPrompt {
                    text: clip(text, MAX_PROMPT_CHARS),
                    at_ms: entry.recorded_at_unix_ms,
                })
            }),
            message: newest.message.and_then(|index| {
                let entry = &entries[index];
                let EntryKind::Assistant { text, .. } = &entry.kind else {
                    return None;
                };
                Some(ActivityMessage {
                    text: MESSAGE_EXCERPT.apply(text),
                    commentary: false,
                    at_ms: entry.recorded_at_unix_ms,
                })
            }),
            tool: newest.tool.and_then(|index| self.tool(&entries[index])),
            turn,
            plan: newest.plan.and_then(|index| match &entries[index].kind {
                EntryKind::Tool(tool) => plan_progress(&tool.arguments),
                _ => None,
            }),
            error,
            agents: AgentCounts {
                running: source
                    .roster
                    .agents
                    .iter()
                    .filter(|agent| {
                        matches!(agent.status, AgentStatus::Pending | AgentStatus::Running)
                    })
                    .count(),
                total: source.roster.agents.len(),
            },
            queued: source.queued,
            context: source.context,
        }
    }

    /// The newest entries of each kind before the prompt at `prompt`. When the newest prompt
    /// moved forward, only the turns between the old and the new prompt are scanned.
    fn earlier(&mut self, model: &TranscriptModel, prompt: usize) -> &Earlier {
        let entries = model.entries();
        let boundary = entries[prompt].id;
        let settled = !model.running_tool_ids().any(|id| {
            model
                .index_of(id)
                .is_some_and(|index| index < prompt && changes_shape(&entries[index]))
        });
        let remembered = self
            .earlier
            .take()
            .filter(|earlier| {
                earlier.settled && model.index_of(earlier.boundary) == Some(earlier.boundary_index)
            })
            .and_then(|earlier| {
                if earlier.boundary == boundary {
                    return Some(Earlier { settled, ..earlier });
                }
                let start = earlier.boundary_index;
                (start < prompt).then(|| {
                    Earlier::scan(&entries[start..prompt], prompt, boundary, settled).or(earlier)
                })
            })
            .filter(|earlier| earlier.still_qualifies(model));
        let earlier = remembered.unwrap_or_else(|| {
            #[cfg(test)]
            {
                self.full_scans += 1;
            }
            Earlier::scan(&entries[..prompt], prompt, boundary, settled)
        });
        self.earlier.insert(earlier)
    }

    fn tool(&mut self, entry: &TranscriptEntry) -> Option<ActivityTool> {
        let EntryKind::Tool(tool) = &entry.kind else {
            return None;
        };
        let key = (entry.id, entry.revision);
        if let Some((id, revision, cached)) = &self.tool
            && (*id, *revision) == key
        {
            return Some(cached.clone());
        }
        let projected = ActivityTool {
            name: tool.name.clone(),
            summary: tool_summary(tool),
            state: tool.state.into(),
            at_ms: entry.recorded_at_unix_ms,
            outcome: (tool.state != ToolState::Running)
                .then(|| tool.result.as_ref().and_then(ToolOutcome::from_result))
                .flatten(),
        };
        self.tool = Some((entry.id, entry.revision, projected.clone()));
        Some(projected)
    }
}

impl Newest {
    fn note(&mut self, index: usize, entry: &TranscriptEntry) {
        if is_message(entry) {
            self.message.get_or_insert(index);
        }
        if top_level_tool(entry).is_some() {
            self.tool.get_or_insert(index);
        }
        if is_plan(entry) {
            self.plan.get_or_insert(index);
        }
    }
}

impl Earlier {
    /// Scans `entries`, which end right before the prompt `boundary` at `boundary_index`.
    fn scan(
        entries: &[TranscriptEntry],
        boundary_index: usize,
        boundary: EntryId,
        settled: bool,
    ) -> Self {
        let mut newest = Newest::default();
        for (index, entry) in entries.iter().enumerate().rev() {
            newest.note(index, entry);
            if newest.message.is_some() && newest.tool.is_some() && newest.plan.is_some() {
                break;
            }
        }
        let id = |index: Option<usize>| index.map(|index| entries[index].id);
        Self {
            boundary,
            boundary_index,
            settled,
            message: id(newest.message),
            tool: id(newest.tool),
            plan: id(newest.plan),
        }
    }

    /// Fills what a scan of newer turns did not find from older ones.
    fn or(self, older: Self) -> Self {
        Self {
            boundary: self.boundary,
            boundary_index: self.boundary_index,
            settled: self.settled,
            message: self.message.or(older.message),
            tool: self.tool.or(older.tool),
            plan: self.plan.or(older.plan),
        }
    }

    /// Whether every remembered entry still exists and still is what it was remembered as. A Code
    /// Mode cell hidden behind its first call, for example, no longer qualifies.
    fn still_qualifies(&self, model: &TranscriptModel) -> bool {
        let holds = |id: Option<EntryId>, qualifies: fn(&TranscriptEntry) -> bool| {
            id.is_none_or(|id| model.entry(id).is_some_and(qualifies))
        };
        holds(self.message, is_message)
            && holds(self.tool, |entry| top_level_tool(entry).is_some())
            && holds(self.plan, is_plan)
    }
}

/// A complete assistant message the transcript shows.
fn is_message(entry: &TranscriptEntry) -> bool {
    !entry.hidden && matches!(entry.kind, EntryKind::Assistant { complete: true, .. })
}

/// A tool call with a row of its own: shown, and not one of a Code Mode cell's calls.
fn top_level_tool(entry: &TranscriptEntry) -> Option<&ToolEntry> {
    match &entry.kind {
        EntryKind::Tool(tool) if !entry.hidden && entry.parent.is_none() => Some(tool),
        _ => None,
    }
}

fn is_plan(entry: &TranscriptEntry) -> bool {
    matches!(&entry.kind, EntryKind::Tool(tool) if tool.name == "update_plan")
}

/// A running tool whose next records can show, hide, or move entries before a prompt rather than
/// only change its own state: a Code Mode cell, which regroups its calls (whether or not it is
/// itself one of a cell's calls), and a hidden `wait`, which is shown when it fails.
fn changes_shape(entry: &TranscriptEntry) -> bool {
    matches!(&entry.kind, EntryKind::Tool(tool)
        if tool.name == "exec" || (entry.hidden && tool.name == "wait"))
}

/// Progress through an `update_plan` call's steps; `None` when its arguments hold no plan.
fn plan_progress(arguments: &Value) -> Option<PlanProgress> {
    let steps = arguments.get("plan")?.as_array()?;
    let has_status =
        |step: &Value, status: &str| step.get("status").and_then(Value::as_str) == Some(status);
    Some(PlanProgress {
        done: steps
            .iter()
            .filter(|step| has_status(step, "completed"))
            .count(),
        total: steps.len(),
        current: steps
            .iter()
            .find(|step| has_status(step, "in_progress"))
            .and_then(|step| step.get("step")?.as_str())
            .map(|step| clip(step, MAX_PLAN_STEP_CHARS)),
    })
}

/// Shortens `text` to at most `max` characters, ending in an ellipsis when it was cut.
pub(super) fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        None => text.to_owned(),
        Some(_) => {
            let mut clipped: String = text.chars().take(max.saturating_sub(1)).collect();
            clipped.push('…');
            clipped
        }
    }
}

/// Keeps the opening and the ending of a long text, joined by an elided line.
#[derive(Clone, Copy)]
pub(super) struct Excerpt {
    /// Texts of at most this many characters are kept whole.
    pub(super) limit: usize,
    pub(super) head: usize,
    pub(super) tail: usize,
}

impl Excerpt {
    pub(super) fn apply(self, text: &str) -> String {
        if text.char_indices().nth(self.limit).is_none() {
            return text.to_owned();
        }
        let head: String = text.chars().take(self.head).collect();
        let mut tail: Vec<char> = text.chars().rev().take(self.tail).collect();
        tail.reverse();
        format!("{head}\n…\n{}", tail.into_iter().collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::{Excerpt, clip};

    #[test]
    fn clipped_text_keeps_its_length_bound_and_marks_the_cut() {
        assert_eq!(clip("short", 10), "short");
        assert_eq!(clip("exactly", 7), "exactly");
        let clipped = clip(&"界".repeat(20), 10);
        assert_eq!(clipped.chars().count(), 10);
        assert!(clipped.ends_with('…'));
    }

    #[test]
    fn excerpts_keep_the_opening_and_the_ending() {
        let excerpt = Excerpt {
            limit: 10,
            head: 3,
            tail: 2,
        };
        assert_eq!(excerpt.apply("0123456789"), "0123456789");
        assert_eq!(excerpt.apply("0123456789ab?"), "012\n…\nb?");
    }
}
