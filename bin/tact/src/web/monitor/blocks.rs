//! The status blocks the model composes for a session's tile, and their validation.
//!
//! Blocks are untrusted model output. Each one is checked on its own: a block that is unknown,
//! malformed, or cites figures the model did not read is dropped, the rest are kept, and every
//! string is cut to its limit. One bad block never fails the whole check.

use super::input::InputSession;
use crate::web::activity::clip;
use serde::Serialize;
use serde_json::{Map, Value};
use std::{cmp::Reverse, collections::HashSet};

/// The blocks a note keeps, most important first.
const MAX_BLOCKS: usize = 4;
const MAX_TEXT_CHARS: usize = 240;
const MAX_LABEL_CHARS: usize = 60;
const MAX_UNIT_CHARS: usize = 20;
const MAX_VALUE_CHARS: usize = 24;
const MAX_TITLE_CHARS: usize = 60;
const MAX_CHECKLIST_ITEMS: usize = 8;
const MAX_ITEM_CHARS: usize = 100;
const MAX_HISTORY_ITEMS: usize = 4;
const MAX_DETAIL_CHARS: usize = 120;
const MAX_QUESTION_CHARS: usize = 300;
const MAX_OPTIONS: usize = 5;
const MAX_OPTION_CHARS: usize = 80;
const MAX_CODE_LABEL_CHARS: usize = 40;
const MAX_CODE_LINES: usize = 4;
const MAX_PROGRESS_TOTAL: u64 = 1_000_000;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum StatusBlock {
    /// One or two sentences: a finding, a blocker, a risk, a result.
    Text { text: String, tone: Tone },
    /// Countable progress toward the goal.
    Progress {
        label: String,
        done: u64,
        total: u64,
        unit: Option<String>,
    },
    /// A measured value, optionally with the value before it.
    Metric {
        label: String,
        value: String,
        previous: Option<String>,
        tone: Tone,
    },
    /// A planned sequence, in planned order.
    Checklist {
        title: Option<String>,
        items: Vec<ChecklistItem>,
    },
    /// Recent steps and whether they worked, newest first.
    History { items: Vec<HistoryItem> },
    /// What the agent asks the developer, with the choices it offered.
    Question { text: String, options: Vec<String> },
    /// A short literal: a command, an assertion, an error line, an output.
    Code { label: Option<String>, text: String },
    /// Live data the browser fills in from the stream.
    Live { source: LiveSource },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Tone {
    Neutral,
    Success,
    Warning,
    Danger,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(super) struct ChecklistItem {
    label: String,
    state: ItemState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ItemState {
    Done,
    Active,
    Pending,
    Failed,
    Skipped,
}

/// One recent step and whether it worked.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(super) struct HistoryItem {
    action: String,
    outcome: Outcome,
    detail: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Outcome {
    Worked,
    Failed,
    Running,
    Unclear,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum LiveSource {
    Message,
    Plan,
    Tool,
    Prompt,
    Agents,
    Error,
}

/// The first [`MAX_BLOCKS`] valid blocks of a note about `session`; a missing or malformed list
/// is empty.
pub(super) fn blocks(note: &Value, session: &InputSession) -> Vec<StatusBlock> {
    note.get("blocks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|block| StatusBlock::read(block.as_object()?, session))
        .take(MAX_BLOCKS)
        .collect()
}

impl StatusBlock {
    fn read(block: &Map<String, Value>, session: &InputSession) -> Option<Self> {
        let field = |key: &str, max: usize| text(block.get(key), max);
        let tone = || {
            block
                .get("tone")
                .and_then(Value::as_str)
                .map_or(Tone::Neutral, Tone::parse)
        };
        let read = match block.get("type")?.as_str()? {
            "text" => Self::Text {
                text: field("text", MAX_TEXT_CHARS)?,
                tone: tone(),
            },
            "progress" => {
                let done = block.get("done")?.as_u64()?;
                let total = block.get("total")?.as_u64()?;
                if total == 0 || total > MAX_PROGRESS_TOTAL || done > total {
                    return None;
                }
                Self::Progress {
                    label: field("label", MAX_LABEL_CHARS)?,
                    done,
                    total,
                    unit: field("unit", MAX_UNIT_CHARS),
                }
            }
            "metric" => Self::Metric {
                label: field("label", MAX_LABEL_CHARS)?,
                value: figure(block.get("value"))??,
                previous: figure(block.get("previous"))?,
                tone: tone(),
            },
            "checklist" => {
                let items: Vec<ChecklistItem> = block
                    .get("items")?
                    .as_array()?
                    .iter()
                    .filter_map(|item| {
                        Some(ChecklistItem {
                            label: text(item.get("label"), MAX_ITEM_CHARS)?,
                            state: item
                                .get("state")
                                .and_then(Value::as_str)
                                .map_or(ItemState::Pending, ItemState::parse),
                        })
                    })
                    .take(MAX_CHECKLIST_ITEMS)
                    .collect();
                if items.is_empty() {
                    return None;
                }
                Self::Checklist {
                    title: field("title", MAX_TITLE_CHARS),
                    items,
                }
            }
            "history" => {
                let items = history(block.get("items")?.as_array()?, session.items);
                if items.is_empty() {
                    return None;
                }
                Self::History { items }
            }
            "question" => {
                let mut options: Vec<String> = block
                    .get("options")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|option| text(Some(option), MAX_OPTION_CHARS))
                    .take(MAX_OPTIONS)
                    .collect();
                order_as_offered(&mut options, session.message.as_deref());
                Self::Question {
                    text: field("text", MAX_QUESTION_CHARS)?,
                    options,
                }
            }
            "code" => {
                let literal = text(block.get("text"), usize::MAX)?;
                let lines: Vec<&str> = literal.lines().take(MAX_CODE_LINES).collect();
                Self::Code {
                    label: field("label", MAX_CODE_LABEL_CHARS),
                    text: clip(&lines.join("\n"), MAX_TEXT_CHARS),
                }
            }
            "live" => Self::Live {
                source: LiveSource::parse(block.get("source")?.as_str()?)?,
            },
            _ => return None,
        };
        read.figures_were_read(session).then_some(read)
    }

    /// Whether every figure of a progress or metric block appears in the session's input, so
    /// the model cannot present a number it did not read. Other blocks have no figures.
    fn figures_were_read(&self, session: &InputSession) -> bool {
        let read = |text: &str| digit_runs(text).all(|run| session.numbers.contains(run));
        match self {
            Self::Progress { done, total, .. } => {
                read(&done.to_string()) && read(&total.to_string())
            }
            Self::Metric {
                value, previous, ..
            } => read(value) && previous.as_deref().is_none_or(read),
            _ => true,
        }
    }
}

/// The newest [`MAX_HISTORY_ITEMS`] steps with an action text, newest first. Models do not
/// reliably keep the requested order, so steps are ordered by the transcript item each one cites
/// as its `at` (sequence numbers 1 to `items`, larger is newer). Steps that cite no such item
/// follow in the model's order.
fn history(steps: &[Value], items: usize) -> Vec<HistoryItem> {
    let newest = u64::try_from(items).unwrap_or(u64::MAX);
    let mut cited: Vec<(Option<u64>, HistoryItem)> = steps
        .iter()
        .filter_map(|step| {
            let at = step
                .get("at")
                .and_then(Value::as_u64)
                .filter(|at| (1..=newest).contains(at));
            let item = HistoryItem {
                action: text(step.get("action"), MAX_ITEM_CHARS)?,
                outcome: step
                    .get("outcome")
                    .and_then(Value::as_str)
                    .map_or(Outcome::Unclear, Outcome::parse),
                detail: text(step.get("detail"), MAX_DETAIL_CHARS),
            };
            Some((at, item))
        })
        .collect();
    cited.sort_by_key(|(at, _)| (at.is_none(), Reverse(*at)));
    cited
        .into_iter()
        .map(|(_, item)| item)
        .take(MAX_HISTORY_ITEMS)
        .collect()
}

/// A trimmed, non-empty string cut to `max` characters.
pub(super) fn text(value: Option<&Value>, max: usize) -> Option<String> {
    let text = value?.as_str()?.trim();
    (!text.is_empty()).then(|| clip(text, max))
}

/// A metric value or previous value: a short figure, which models also write as a bare number.
/// `Some(None)` when it is absent or blank, and `None`, which drops the block, when it is not a
/// short figure. A longer value is prose that belongs in a text block, and cutting it would
/// change its meaning.
fn figure(value: Option<&Value>) -> Option<Option<String>> {
    let figure = match value {
        None | Some(Value::Null) => return Some(None),
        Some(Value::Number(number)) => number.to_string(),
        Some(Value::String(text)) => text.trim().to_owned(),
        Some(_) => return None,
    };
    if figure.is_empty() {
        return Some(None);
    }
    (figure.chars().count() <= MAX_VALUE_CHARS).then_some(Some(figure))
}

/// Orders a question's options by where each first appears in the agent's newest message, so
/// they read as the agent offered them whatever order the model wrote. Options the message does
/// not contain follow in the model's order.
fn order_as_offered(options: &mut [String], message: Option<&str>) {
    let Some(message) = message else {
        return;
    };
    let message = message.to_lowercase();
    options.sort_by_cached_key(|option| {
        let at = message.find(&option.to_lowercase());
        (at.is_none(), at)
    });
}

/// The maximal runs of ASCII digits in `text`.
pub(super) fn digit_runs(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !c.is_ascii_digit())
        .filter(|run| !run.is_empty())
}

/// Every digit run of `text`, for checking figures against what the model read.
pub(super) fn numbers_in(text: &str, numbers: &mut HashSet<String>) {
    numbers.extend(digit_runs(text).map(str::to_owned));
}

impl Tone {
    fn parse(value: &str) -> Self {
        match value {
            "success" => Self::Success,
            "warning" => Self::Warning,
            "danger" => Self::Danger,
            _ => Self::Neutral,
        }
    }
}

impl ItemState {
    fn parse(value: &str) -> Self {
        match value {
            "done" => Self::Done,
            "active" => Self::Active,
            "failed" => Self::Failed,
            "skipped" => Self::Skipped,
            _ => Self::Pending,
        }
    }
}

impl Outcome {
    /// Unknown values read as `Unclear`: the evidence did not say.
    fn parse(value: &str) -> Self {
        match value {
            "worked" => Self::Worked,
            "failed" => Self::Failed,
            "running" => Self::Running,
            _ => Self::Unclear,
        }
    }
}

impl LiveSource {
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "message" => Self::Message,
            "plan" => Self::Plan,
            "tool" => Self::Tool,
            "prompt" => Self::Prompt,
            "agents" => Self::Agents,
            "error" => Self::Error,
            _ => return None,
        })
    }
}
