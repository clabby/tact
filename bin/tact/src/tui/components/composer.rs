//! Multiline prompt editing and Pi-style composer rendering.
//!
//! [Composer] owns the draft ([draft::DraftBuffer]), prompt history recall,
//! and the chrome drawn around the draft: workspace, context usage, turn
//! timers, activity and task status, live sessions, and the clickable effort,
//! speed, model, and subagent controls. It consumes [ComposerEvent]s from the
//! terminal and the session and reports each change through [ComposerUpdate],
//! which yields a [ComposerEffect] when the user submits a prompt, runs a shell
//! command with a leading `!`, or asks to edit the draft externally. The chrome
//! regions recorded during the last frame resolve clicks through
//! [Composer::chrome_target].

mod draft;
mod history;
mod layout;

use super::{
    node::{Component, ComponentUpdate, RenderRequest},
    selection::{TextRange, TextSpan},
    waved_text::WavedText,
};
use crate::{
    app::{
        config::{ReasoningEffort, ReasoningMode, Speed},
        installation::InstallationKind,
        theme::Theme,
    },
    core::{
        context::{ContextBudget, MODEL_WINDOW_TOKENS},
        prompt::Submission,
    },
    tui::format::{format_turn_duration, shorten_home},
};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
pub(crate) use draft::ComposerDraft;
use draft::{Direction, DraftBuffer};
use history::PromptHistory;
use layout::grapheme_at_column;
use nanocodex::{HarnessModel as Model, Model as CodexModel};
use ratatui::{
    Frame,
    buffer::Buffer,
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use std::{
    collections::VecDeque,
    fmt::{self, Display, Formatter, Write as _},
    ops::Range,
    path::Path,
    time::{Duration, Instant},
};
use unicode_width::UnicodeWidthStr;

const MIN_CONTENT_ROWS: usize = 3;
const MAX_CONTENT_ROWS: usize = 6;

/// The footer badge of a build that is not an official release: red for a source build and blue for
/// a pre-release published from `main`.
fn build_badge(installation: &InstallationKind) -> Option<(&'static str, Color)> {
    match installation {
        InstallationKind::Development => Some((" ◉ dev ", Color::Red)),
        InstallationKind::PreRelease { .. } => Some((" ◉ pre-release ", Color::Blue)),
        _ => None,
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ComposerEffect {
    Submit(Submission),
    RunShell(String),
    OpenDraftEditor,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ComposerChromeTarget {
    Effort,
    Speed,
    Model,
    Subagents,
}

/// What the draft in the composer is for. Modes other than [`InputMode::Prompt`] show their keys
/// in the status line.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum InputMode {
    /// A prompt for the agent.
    #[default]
    Prompt,
    /// Optional guidance for a reflection, which starts when the draft is submitted.
    Reflection,
    /// A queued message being edited in place.
    EditingQueued,
}

impl InputMode {
    const fn hint(self) -> Option<&'static str> {
        match self {
            Self::Prompt => None,
            Self::Reflection => Some("Reflection instructions · enter start · esc cancel"),
            Self::EditingQueued => Some("editing queued message · enter save · esc cancel"),
        }
    }
}

/// The sessions hosted by this process, of which `running` have a turn or shell command in flight.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LiveSessions {
    pub(crate) live: usize,
    pub(crate) running: usize,
}

impl Display for LiveSessions {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} sessions, {} running",
            self.live, self.running
        )
    }
}

pub(crate) enum ComposerEvent {
    Terminal(Event),
    PasteImage(String),
    ContextBudget(ContextBudget),
    ReplaceRange {
        range: Range<usize>,
        text: String,
    },
    ReplaceDraft(String),
    /// Submits the draft exactly as plain Enter does.
    Submit,
    SetEffort(ReasoningEffort),
    SetModel(Model),
    SetReasoningMode(ReasoningMode),
    SetSpeed(Speed),
    InputMode(InputMode),
    Activity {
        active: bool,
        status: Option<String>,
        now: Instant,
    },
    /// Shows the status of a task that blocks the composer, or clears it with `None`.
    TaskStatus {
        status: Option<String>,
        now: Instant,
    },
    /// Summarizes this process's live sessions when there is more than one.
    LiveSessions(Option<LiveSessions>),
    ActiveSubagents {
        count: usize,
        now: Instant,
    },
    TurnStarted {
        elapsed: Duration,
        now: Instant,
    },
    TurnFinished,
    TurnsCleared,
    AnimationFrame(Instant),
}

pub(crate) struct Composer {
    draft: DraftBuffer,
    /// The first visual line shown in the draft area.
    scroll: usize,
    /// The draft width of the last frame, which vertical movement wraps at.
    last_width: usize,
    context_tokens: u64,
    context_window_tokens: u64,
    workspace: String,
    thinking: ReasoningEffort,
    model: Model,
    reasoning_mode: ReasoningMode,
    speed: Speed,
    input_mode: InputMode,
    /// The transient status of the running turn.
    activity_wave: Option<WavedText>,
    /// The status of a task that blocks the composer.
    task_wave: Option<WavedText>,
    live_sessions: Option<LiveSessions>,
    active_subagents: usize,
    subagent_wave: Option<WavedText>,
    turn_timers: VecDeque<TurnTimer>,
    /// The clickable chrome controls drawn in the last frame.
    chrome_hits: Vec<(ComposerChromeTarget, Rect)>,
    history: PromptHistory,
}

struct TurnTimer {
    observed_at: Instant,
    elapsed_at_observation: Duration,
    displayed_seconds: u64,
}

impl TurnTimer {
    fn new(elapsed: Duration, now: Instant) -> Self {
        Self {
            observed_at: now,
            elapsed_at_observation: elapsed,
            displayed_seconds: elapsed.as_secs(),
        }
    }

    fn advance(&mut self, now: Instant) -> bool {
        let displayed_seconds = self.elapsed(now).as_secs();
        if self.displayed_seconds == displayed_seconds {
            return false;
        }
        self.displayed_seconds = displayed_seconds;
        true
    }

    fn elapsed(&self, now: Instant) -> Duration {
        self.elapsed_at_observation
            .saturating_add(now.saturating_duration_since(self.observed_at))
    }

    fn deadline(&self) -> Instant {
        let elapsed_subsecond = self.elapsed_at_observation.subsec_nanos();
        let until_next_second = Duration::from_nanos(1_000_000_000 - u64::from(elapsed_subsecond));
        self.observed_at
            + until_next_second
            + Duration::from_secs(
                self.displayed_seconds
                    .saturating_sub(self.elapsed_at_observation.as_secs()),
            )
    }

    fn label(&self) -> String {
        format_turn_duration(self.displayed_seconds.saturating_mul(1_000_000_000))
    }
}

/// The content of the composer's top border, which [`Composer::render_chrome`] lays out:
/// status text from the left, and the turn timer and clickable controls from the right.
#[derive(Debug, Eq, PartialEq)]
struct StatusLine {
    /// Context usage as a rounded percentage of the window, such as `12%/272k`.
    context: String,
    input_mode: Option<&'static str>,
    /// The status of a task that blocks the composer.
    task: Option<String>,
    /// The transient status of the running turn.
    activity: Option<String>,
    subagents: usize,
    live_sessions: Option<LiveSessions>,
    /// The elapsed time of the oldest running turn.
    turn_timer: Option<String>,
    model: Model,
    effort: ReasoningEffort,
    /// The speed the model runs at, which is lower than requested when the model lacks that tier.
    speed: Speed,
    /// Whether the pro badge is shown; only Codex models have a pro reasoning mode.
    pro: bool,
}

/// The result of a composer update.
///
/// Unlike [`ComponentUpdate`](super::node::ComponentUpdate), the composer reports whether it
/// changed rather than how urgently to redraw: the same change, such as a new context budget, is
/// urgent when it answers a keypress and can wait for the next frame when it arrives with streamed
/// output, and only the caller knows which applies. An update yields at most one effect.
pub(crate) struct ComposerUpdate {
    pub(crate) effect: Option<ComposerEffect>,
    pub(crate) changed: bool,
}

impl Composer {
    pub(super) fn set_workspace(&mut self, workspace: &Path) {
        self.workspace = shorten_home(workspace);
    }

    pub(crate) fn new(workspace: &Path, thinking: ReasoningEffort) -> Self {
        Self {
            draft: DraftBuffer::default(),
            scroll: 0,
            last_width: 78,
            context_tokens: 0,
            context_window_tokens: MODEL_WINDOW_TOKENS,
            workspace: shorten_home(workspace),
            thinking,
            model: Model::Codex(CodexModel::Sol),
            reasoning_mode: ReasoningMode::Standard,
            speed: Speed::Standard,
            input_mode: InputMode::Prompt,
            activity_wave: None,
            task_wave: None,
            live_sessions: None,
            active_subagents: 0,
            subagent_wave: None,
            turn_timers: VecDeque::new(),
            chrome_hits: Vec::new(),
            history: PromptHistory::default(),
        }
    }

    pub(crate) const fn context_tokens(&self) -> u64 {
        self.context_tokens
    }

    pub(crate) const fn context_budget(&self) -> ContextBudget {
        ContextBudget {
            active_tokens: self.context_tokens,
            window_tokens: self.context_window_tokens,
        }
    }

    /// The draft's images as (marker, data URL) pairs in text order.
    pub(crate) fn images(&self) -> impl Iterator<Item = (&str, &str)> {
        self.draft.images()
    }

    /// Appends an image marker at the end of the draft, as pasting the image there would.
    pub(crate) fn append_image(&mut self, data_url: String) {
        self.history.detach();
        self.draft.append_image(data_url);
    }

    pub(crate) fn update(&mut self, event: ComposerEvent) -> ComposerUpdate {
        match event {
            ComposerEvent::Terminal(Event::Key(key)) => self.handle_key(key),
            ComposerEvent::Terminal(Event::Paste(text)) => {
                self.history.detach();
                self.draft.insert(&text);
                ComposerUpdate::changed()
            }
            ComposerEvent::Terminal(_) => ComposerUpdate::unchanged(),
            ComposerEvent::Submit => self.submit(),
            ComposerEvent::PasteImage(data_url) => {
                self.history.detach();
                self.draft.insert_image(data_url);
                ComposerUpdate::changed()
            }
            ComposerEvent::ContextBudget(budget) => {
                let changed = self.context_tokens != budget.active_tokens
                    || self.context_window_tokens != budget.window_tokens;
                self.context_tokens = budget.active_tokens;
                self.context_window_tokens = budget.window_tokens;
                ComposerUpdate::from_change(changed)
            }
            ComposerEvent::ReplaceRange { range, text } => {
                self.history.detach();
                self.draft.remove_range(range);
                self.draft.insert(&text);
                ComposerUpdate::changed()
            }
            ComposerEvent::ReplaceDraft(draft) => {
                self.history.detach();
                self.replace_draft(draft);
                ComposerUpdate::changed()
            }
            ComposerEvent::SetEffort(effort) => {
                if self.thinking == effort {
                    return ComposerUpdate::unchanged();
                }
                self.thinking = effort;
                ComposerUpdate::changed()
            }
            ComposerEvent::SetModel(model) => {
                if self.model == model {
                    return ComposerUpdate::unchanged();
                }
                self.model = model;
                ComposerUpdate::changed()
            }
            ComposerEvent::SetReasoningMode(mode) => {
                if self.reasoning_mode == mode {
                    return ComposerUpdate::unchanged();
                }
                self.reasoning_mode = mode;
                ComposerUpdate::changed()
            }
            ComposerEvent::SetSpeed(speed) => {
                if self.speed == speed {
                    return ComposerUpdate::unchanged();
                }
                self.speed = speed;
                ComposerUpdate::changed()
            }
            ComposerEvent::InputMode(mode) => {
                if self.input_mode == mode {
                    return ComposerUpdate::unchanged();
                }
                self.input_mode = mode;
                ComposerUpdate::changed()
            }
            ComposerEvent::Activity {
                active,
                status,
                now,
            } => {
                let status = if active { status } else { None };
                ComposerUpdate::from_change(replace_wave(
                    &mut self.activity_wave,
                    status,
                    Color::Cyan,
                    now,
                ))
            }
            ComposerEvent::TaskStatus { status, now } => ComposerUpdate::from_change(replace_wave(
                &mut self.task_wave,
                status,
                Color::Green,
                now,
            )),
            ComposerEvent::LiveSessions(summary) => {
                if self.live_sessions == summary {
                    return ComposerUpdate::unchanged();
                }
                self.live_sessions = summary;
                ComposerUpdate::changed()
            }
            ComposerEvent::ActiveSubagents { count, now } => {
                if self.active_subagents == count {
                    return ComposerUpdate::unchanged();
                }
                self.active_subagents = count;
                self.subagent_wave = (count > 0).then(|| {
                    let mut wave = WavedText::new(format!("{count} subagents"), Color::Yellow);
                    wave.set_active(true, now);
                    wave
                });
                ComposerUpdate::changed()
            }
            ComposerEvent::TurnStarted { elapsed, now } => {
                self.turn_timers.push_back(TurnTimer::new(elapsed, now));
                ComposerUpdate::changed()
            }
            ComposerEvent::TurnFinished => {
                if self.turn_timers.pop_front().is_none() {
                    return ComposerUpdate::unchanged();
                }
                ComposerUpdate::changed()
            }
            ComposerEvent::TurnsCleared => {
                if self.turn_timers.is_empty() {
                    return ComposerUpdate::unchanged();
                }
                self.turn_timers.clear();
                ComposerUpdate::changed()
            }
            ComposerEvent::AnimationFrame(now) => {
                let activity_changed = self
                    .activity_wave
                    .as_mut()
                    .is_some_and(|wave| wave.advance(now));
                let task_changed = self
                    .task_wave
                    .as_mut()
                    .is_some_and(|wave| wave.advance(now));
                let subagent_changed = self
                    .subagent_wave
                    .as_mut()
                    .is_some_and(|wave| wave.advance(now));
                let mut timer_changed = false;
                for timer in &mut self.turn_timers {
                    timer_changed |= timer.advance(now);
                }
                ComposerUpdate::from_change(
                    activity_changed || task_changed || subagent_changed || timer_changed,
                )
            }
        }
    }

    pub(super) fn chrome_target(&self, position: Position) -> Option<ComposerChromeTarget> {
        self.chrome_hits
            .iter()
            .find(|(_, area)| area.contains(position))
            .map(|(target, _)| *target)
    }

    #[cfg(test)]
    fn chrome_area(&self, target: ComposerChromeTarget) -> Option<Rect> {
        self.chrome_hits
            .iter()
            .find(|(hit, _)| *hit == target)
            .map(|(_, area)| *area)
    }

    pub(crate) fn animation_deadline(&self) -> Option<Instant> {
        self.activity_wave
            .as_ref()
            .and_then(WavedText::animation_deadline)
            .into_iter()
            .chain(
                self.task_wave
                    .as_ref()
                    .and_then(WavedText::animation_deadline),
            )
            .chain(
                self.subagent_wave
                    .as_ref()
                    .and_then(WavedText::animation_deadline),
            )
            .chain(self.turn_timers.iter().map(TurnTimer::deadline))
            .min()
    }

    pub(crate) fn desired_height(&mut self, width: u16) -> u16 {
        if width < 2 {
            return 1;
        }

        let content_width = usize::from(width.saturating_sub(2)).max(1);
        let rows = self
            .draft
            .visual_layout(content_width)
            .lines
            .len()
            .clamp(MIN_CONTENT_ROWS, MAX_CONTENT_ROWS);
        u16::try_from(rows + 2).unwrap_or(u16::MAX)
    }

    pub(crate) fn render(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        self.render_focused(frame, area, theme, true);
    }

    pub(crate) fn render_focused(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        theme: &Theme,
        focused: bool,
    ) {
        self.render_focused_with_selection(frame, area, theme, focused, None);
    }

    pub(super) fn render_focused_with_selection(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        theme: &Theme,
        focused: bool,
        selection: Option<TextRange>,
    ) {
        if area.is_empty() {
            return;
        }
        if area.width < 2 || area.height < 3 {
            self.render_narrow(frame, area, theme, focused, selection);
            return;
        }

        let content_width = usize::from(area.width - 2).max(1);
        self.last_width = content_width;
        let (cursor_row, cursor_column, line_count) = {
            let layout = self.draft.visual_layout(content_width);
            (layout.cursor_row, layout.cursor_column, layout.lines.len())
        };
        let visible_rows = usize::from(area.height - 2);
        if selection.is_none() {
            self.keep_cursor_visible(cursor_row, visible_rows, line_count);
        } else {
            self.clamp_scroll(visible_rows, line_count);
        }

        let buffer = frame.buffer_mut();
        buffer.set_style(area, Style::default().fg(theme.text()));
        self.render_chrome(buffer, area, theme);
        let border = self.border_style(theme);
        let selected =
            selection.and_then(|selection| selection.source_range(0, self.draft.text().len()));

        for row in 0..visible_rows {
            let y = area.y + 1 + u16::try_from(row).unwrap_or(u16::MAX);
            draw_symbol(buffer, area.x, y, "│", border);
            draw_symbol(buffer, area.right() - 1, y, "│", border);

            let Some(line) = self
                .draft
                .cached_layout()
                .and_then(|layout| layout.lines.get(self.scroll + row))
            else {
                continue;
            };
            let position = Position::new(area.x + 1, y);
            self.draft
                .render_line(buffer, position, line.start..line.end, content_width, theme);
            if let Some(selected) = selected.clone() {
                self.draft.render_selection(
                    buffer,
                    position,
                    line.start..line.end,
                    selected,
                    content_width,
                );
            }
        }

        let cursor_row = cursor_row.saturating_sub(self.scroll);
        let cursor_x = area.x + 1 + u16::try_from(cursor_column).unwrap_or(u16::MAX);
        let cursor_y = area.y + 1 + u16::try_from(cursor_row).unwrap_or(u16::MAX);
        let max_cursor_x = area.right().saturating_sub(2);
        if focused && selection.is_none() {
            frame.set_cursor_position(Position::new(cursor_x.min(max_cursor_x), cursor_y));
        }
    }

    pub(super) fn selection_span(&mut self, position: Position, area: Rect) -> Option<TextSpan> {
        if area.is_empty() {
            return None;
        }

        let width = usize::from(area.width).max(1);
        self.last_width = width;
        let position = Position::new(
            position.x.clamp(area.x, area.right().saturating_sub(1)),
            position.y.clamp(area.y, area.bottom().saturating_sub(1)),
        );
        let row = self.scroll + usize::from(position.y - area.y);
        let column = usize::from(position.x - area.x);
        let line = self.draft.visual_layout(width).lines.get(row)?.clone();
        let range = grapheme_at_column(self.draft.text(), &line, column);
        Some(TextSpan::new(0, range.start, range.end))
    }

    pub(super) fn selection_text(&self, selection: TextRange) -> Option<String> {
        let text = self.draft.text();
        let range = selection.source_range(0, text.len())?;
        text.get(range).map(ToOwned::to_owned)
    }

    pub(super) fn scroll_selection(&mut self, rows: isize, area: Rect) -> bool {
        if area.is_empty() {
            return false;
        }

        let width = usize::from(area.width).max(1);
        self.last_width = width;
        let line_count = self.draft.visual_layout(width).lines.len();
        let visible_rows = usize::from(area.height);
        let maximum = line_count.saturating_sub(visible_rows);
        let scroll = self.scroll.saturating_add_signed(rows).min(maximum);
        if scroll == self.scroll {
            return false;
        }
        self.scroll = scroll;
        true
    }

    pub(crate) fn draft(&self) -> &str {
        self.draft.text()
    }

    pub(crate) const fn input_mode(&self) -> InputMode {
        self.input_mode
    }

    pub(crate) const fn effort(&self) -> ReasoningEffort {
        self.thinking
    }

    pub(crate) const fn model(&self) -> Model {
        self.model
    }

    /// Requested speed, retained when the model uses a lower tier.
    pub(crate) const fn speed(&self) -> Speed {
        self.speed
    }

    pub(crate) const fn reasoning_mode(&self) -> ReasoningMode {
        self.reasoning_mode
    }

    pub(crate) const fn cursor(&self) -> usize {
        self.draft.cursor()
    }

    pub(crate) fn cursor_is_at_token_boundary(&self) -> bool {
        self.draft.cursor_is_at_token_boundary()
    }

    /// Replaces the draft text. An image survives while its marker still occurs in the new text,
    /// so an edit made elsewhere (the web interface or an external editor) keeps the images whose
    /// markers it left alone.
    pub(crate) fn replace_draft(&mut self, draft: String) {
        self.draft.replace(draft);
        self.scroll = 0;
    }

    pub(crate) fn take_submission(&mut self) -> Option<Submission> {
        let submission = self.draft.take_submission()?;
        self.scroll = 0;
        Some(submission)
    }

    pub(crate) fn take_draft(&mut self) -> Option<ComposerDraft> {
        let draft = self.draft.take()?;
        self.history.detach();
        self.scroll = 0;
        Some(draft)
    }

    pub(crate) fn restore_draft(&mut self, draft: ComposerDraft) {
        self.draft.restore(draft);
        self.history.detach();
        self.scroll = 0;
    }

    fn handle_key(&mut self, key: KeyEvent) -> ComposerUpdate {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return ComposerUpdate::unchanged();
        }

        if key.modifiers == KeyModifiers::CONTROL {
            if matches!(key.code, KeyCode::Char('a' | 'b' | 'e' | 'f' | 'k')) {
                self.history.detach();
            }
            return match key.code {
                KeyCode::Char('a') => ComposerUpdate::from_change(
                    self.draft.move_to_logical_edge(Direction::Backward),
                ),
                KeyCode::Char('b') => {
                    ComposerUpdate::from_change(self.draft.move_grapheme(Direction::Backward))
                }
                KeyCode::Char('e') => {
                    ComposerUpdate::from_change(self.draft.move_to_logical_edge(Direction::Forward))
                }
                KeyCode::Char('f') => {
                    ComposerUpdate::from_change(self.draft.move_grapheme(Direction::Forward))
                }
                KeyCode::Char('g') => {
                    ComposerUpdate::effect(ComposerEffect::OpenDraftEditor, false)
                }
                KeyCode::Char('j') => {
                    self.history.detach();
                    self.draft.insert("\n");
                    ComposerUpdate::changed()
                }
                KeyCode::Char('k') => {
                    ComposerUpdate::from_change(self.draft.delete_to_logical_line_end())
                }
                KeyCode::Char('n') => {
                    ComposerUpdate::from_change(self.move_vertical(Direction::Forward))
                }
                KeyCode::Char('p') => {
                    ComposerUpdate::from_change(self.move_vertical(Direction::Backward))
                }
                _ => ComposerUpdate::unchanged(),
            };
        }
        // Prevent unsupported Ctrl chords from falling through as text input.
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return ComposerUpdate::unchanged();
        }

        let detaches_history = matches!(
            key.code,
            KeyCode::Char(_)
                | KeyCode::Left
                | KeyCode::Right
                | KeyCode::Home
                | KeyCode::End
                | KeyCode::Backspace
                | KeyCode::Delete
        ) || key.code == KeyCode::Enter
            && key.modifiers.contains(KeyModifiers::SHIFT);
        if detaches_history {
            self.history.detach();
        }

        match key.code {
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.draft.insert("\n");
                ComposerUpdate::changed()
            }
            KeyCode::Enter => self.submit(),
            KeyCode::Char('b') if key.modifiers == KeyModifiers::ALT => {
                ComposerUpdate::from_change(self.draft.move_word(Direction::Backward))
            }
            KeyCode::Char('f') if key.modifiers == KeyModifiers::ALT => {
                ComposerUpdate::from_change(self.draft.move_word(Direction::Forward))
            }
            KeyCode::Char(character) => {
                self.draft.insert(character.encode_utf8(&mut [0; 4]));
                ComposerUpdate::changed()
            }
            KeyCode::Left => {
                ComposerUpdate::from_change(self.draft.move_grapheme(Direction::Backward))
            }
            KeyCode::Right => {
                ComposerUpdate::from_change(self.draft.move_grapheme(Direction::Forward))
            }
            KeyCode::Up => ComposerUpdate::from_change(self.move_vertical(Direction::Backward)),
            KeyCode::Down => ComposerUpdate::from_change(self.move_vertical(Direction::Forward)),
            KeyCode::Home => ComposerUpdate::from_change(
                self.draft
                    .move_to_visual_edge(Direction::Backward, self.last_width),
            ),
            KeyCode::End => ComposerUpdate::from_change(
                self.draft
                    .move_to_visual_edge(Direction::Forward, self.last_width),
            ),
            KeyCode::Backspace if key.modifiers.contains(KeyModifiers::ALT) => {
                ComposerUpdate::from_change(self.draft.delete_word_before_cursor())
            }
            KeyCode::Backspace => ComposerUpdate::from_change(self.draft.backspace()),
            KeyCode::Delete => ComposerUpdate::from_change(self.draft.delete()),
            _ => ComposerUpdate::unchanged(),
        }
    }

    fn submit(&mut self) -> ComposerUpdate {
        let trimmed = self.draft.text().trim();
        if trimmed.is_empty() {
            return ComposerUpdate::unchanged();
        }

        if !self.draft.has_images() && self.draft.text().starts_with('!') {
            let command = trimmed.trim_start_matches('!').trim().to_owned();
            if command.is_empty() {
                return ComposerUpdate::unchanged();
            }
            self.history.record(format!("!{command}"));
            self.replace_draft(String::new());
            return ComposerUpdate::effect(ComposerEffect::RunShell(command), true);
        }

        let prompt = self
            .take_submission()
            .expect("non-empty composer draft must produce a submission");
        self.history.record(prompt.display_text().to_owned());
        ComposerUpdate::effect(ComposerEffect::Submit(prompt), true)
    }

    /// Moves between visual lines, or through prompt history from the first
    /// line upward and while history is being browsed.
    fn move_vertical(&mut self, direction: Direction) -> bool {
        let browsing = self.history.is_browsing();
        if !browsing && self.draft.move_vertical(direction, self.last_width) {
            return true;
        }
        let prompt = match direction {
            Direction::Backward => self.history.previous(self.draft.text()),
            Direction::Forward if browsing => self.history.next(),
            Direction::Forward => None,
        };
        let Some(prompt) = prompt else {
            return false;
        };
        self.replace_draft(prompt);
        true
    }

    fn keep_cursor_visible(&mut self, cursor_row: usize, visible: usize, line_count: usize) {
        if visible == 0 {
            self.scroll = 0;
            return;
        }
        if cursor_row < self.scroll {
            self.scroll = cursor_row;
        } else if cursor_row >= self.scroll + visible {
            self.scroll = cursor_row + 1 - visible;
        }

        self.clamp_scroll(visible, line_count);
    }

    fn clamp_scroll(&mut self, visible: usize, line_count: usize) {
        self.scroll = self.scroll.min(line_count.saturating_sub(visible));
    }

    fn render_narrow(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        theme: &Theme,
        focused: bool,
        selection: Option<TextRange>,
    ) {
        let width = usize::from(area.width).max(1);
        let (cursor_row, cursor_column, line_count) = {
            let layout = self.draft.visual_layout(width);
            (layout.cursor_row, layout.cursor_column, layout.lines.len())
        };
        if selection.is_none() {
            self.scroll = 0;
        } else {
            self.clamp_scroll(1, line_count);
        }
        let scroll = self.scroll;
        let line = self.draft.visual_layout(width).lines[scroll].clone();

        let buffer = frame.buffer_mut();
        buffer.set_style(area, Style::default().fg(theme.text()));
        let position = Position::new(area.x, area.y);
        self.draft
            .render_line(buffer, position, line.start..line.end, width, theme);
        if let Some(selected) =
            selection.and_then(|selection| selection.source_range(0, self.draft.text().len()))
        {
            self.draft
                .render_selection(buffer, position, line.start..line.end, selected, width);
        }
        if focused && selection.is_none() {
            let cursor_column = if cursor_row == scroll {
                u16::try_from(cursor_column).unwrap_or(u16::MAX)
            } else {
                0
            };
            let cursor_x = area
                .x
                .saturating_add(cursor_column.min(area.width.saturating_sub(1)));
            frame.set_cursor_position(Position::new(cursor_x, area.y));
        }
    }

    fn status_line(&self) -> StatusLine {
        let window = self.context_window_tokens;
        let capacity = if window.is_multiple_of(1_000_000) {
            format!("{}m", window / 1_000_000)
        } else if window.is_multiple_of(1_000) {
            format!("{}k", window / 1_000)
        } else {
            window.to_string()
        };
        StatusLine {
            context: format!(
                "{}%/{capacity}",
                context_percent(self.context_tokens, window)
            ),
            input_mode: self.input_mode.hint(),
            task: self.task_wave.as_ref().map(|wave| wave.text().to_owned()),
            activity: self
                .activity_wave
                .as_ref()
                .map(|wave| wave.text().to_owned()),
            subagents: self.active_subagents,
            live_sessions: self.live_sessions,
            turn_timer: self.turn_timers.front().map(TurnTimer::label),
            model: self.model,
            effort: self.thinking,
            speed: self.speed.for_model(self.model),
            pro: self.reasoning_mode == ReasoningMode::Pro && matches!(self.model, Model::Codex(_)),
        }
    }

    fn render_chrome(&mut self, buffer: &mut Buffer, area: Rect, theme: &Theme) {
        self.chrome_hits.clear();
        let shell_mode = self.draft.text().starts_with('!');
        let border = self.border_style(theme);
        let top = area.y;
        let bottom = area.bottom() - 1;

        for x in area.x..area.right() {
            draw_symbol(buffer, x, top, "─", border);
            draw_symbol(buffer, x, bottom, "─", border);
        }
        draw_symbol(buffer, area.x, top, "╭", border);
        draw_symbol(buffer, area.right() - 1, top, "╮", border);
        draw_symbol(buffer, area.x, bottom, "╰", border);
        draw_symbol(buffer, area.right() - 1, bottom, "╯", border);

        if area.width < 4 {
            return;
        }

        let content_start = area.x + 2;
        let content_width = usize::from(area.width - 4);
        let content_end = content_start + u16::try_from(content_width).unwrap_or(u16::MAX);
        let status = self.status_line();
        let usage_prefix = format!(" {} ", status.context);
        let mut usage_before_activity = usage_prefix.clone();
        if let Some(hint) = status.input_mode {
            write!(usage_before_activity, "{hint} ").expect("writing to a String cannot fail");
        }
        if let Some(task) = &status.task {
            write!(usage_before_activity, "{task} ").expect("writing to a String cannot fail");
        }
        let mut usage_before_subagents = usage_before_activity.clone();
        if let Some(activity) = &status.activity {
            write!(usage_before_subagents, "{activity} ").expect("writing to a String cannot fail");
        }
        let mut usage = usage_before_subagents.clone();
        if status.subagents > 0 {
            write!(usage, "{} subagents ", status.subagents)
                .expect("writing to a String cannot fail");
        }
        if let Some(sessions) = status.live_sessions {
            write!(usage, "{sessions} ").expect("writing to a String cannot fail");
        }
        let model = format!(" {} ", status.model);
        let timer = status
            .turn_timer
            .as_deref()
            .map(|label| format!(" {label} "))
            .unwrap_or_default();
        let effort = format!(" {} ", status.effort.as_str());

        // Nerd Fonts: md-turtle, md-rabbit, and md-rocket.
        let speed = match status.speed {
            Speed::Standard => "󰳗 ",
            Speed::Fast => "󰤇 ",
            Speed::Ultrafast => "󰑣 ",
        };
        let pro_mode = status.pro.then_some("pro ");
        let right_width = timer.width()
            + model.width()
            + effort.width()
            + speed.width()
            + pro_mode.map_or(0, UnicodeWidthStr::width);
        let right_start = content_start
            + u16::try_from(content_width.saturating_sub(right_width)).unwrap_or(u16::MAX);

        let usage_space = usize::from(right_start.saturating_sub(content_start)).saturating_sub(1);
        buffer.set_stringn(
            content_start,
            top,
            usage,
            usage_space,
            Style::default().fg(theme.muted()),
        );
        if let Some(wave) = &self.task_wave {
            let x = content_start + u16::try_from(usage_prefix.width()).unwrap_or(u16::MAX);
            wave.draw(buffer, x, top, right_start);
        }
        if let Some(wave) = &self.activity_wave {
            let x =
                content_start + u16::try_from(usage_before_activity.width()).unwrap_or(u16::MAX);
            wave.draw(buffer, x, top, right_start);
        }
        if let Some(wave) = &self.subagent_wave {
            let wave_x =
                content_start + u16::try_from(usage_before_subagents.width()).unwrap_or(u16::MAX);
            let wave_width = u16::try_from(wave.width())
                .unwrap_or(u16::MAX)
                .min(right_start.saturating_sub(wave_x));
            if wave_width > 0 {
                self.chrome_hits.push((
                    ComposerChromeTarget::Subagents,
                    Rect::new(wave_x, top, wave_width, 1),
                ));
            }
            wave.draw(buffer, wave_x, top, right_start);
        }
        buffer.set_stringn(
            right_start,
            top,
            &timer,
            usize::from(content_end.saturating_sub(right_start)),
            Style::default().fg(theme.muted()),
        );
        let model_start = right_start + u16::try_from(timer.width()).unwrap_or(u16::MAX);
        if model_start < content_end {
            let width = u16::try_from(model.width())
                .unwrap_or(u16::MAX)
                .min(content_end.saturating_sub(model_start));
            self.chrome_hits.push((
                ComposerChromeTarget::Model,
                Rect::new(model_start, top, width, 1),
            ));
        }
        buffer.set_stringn(
            model_start,
            top,
            &model,
            usize::from(content_end.saturating_sub(model_start)),
            Style::default().fg(theme.model(status.model)),
        );
        let effort_start = model_start + u16::try_from(model.width()).unwrap_or(u16::MAX);
        if effort_start < content_end {
            let width = u16::try_from(effort.width())
                .unwrap_or(u16::MAX)
                .min(content_end.saturating_sub(effort_start));
            self.chrome_hits.push((
                ComposerChromeTarget::Effort,
                Rect::new(effort_start, top, width, 1),
            ));
            buffer.set_stringn(
                effort_start,
                top,
                &effort,
                usize::from(content_end - effort_start),
                Style::default()
                    .fg(theme.effort(status.effort))
                    .add_modifier(Modifier::BOLD),
            );
        }
        let speed_start = effort_start + u16::try_from(effort.width()).unwrap_or(u16::MAX);
        let speed_width = u16::try_from(speed.width()).unwrap_or(u16::MAX);
        let natural_pro_mode_start = speed_start + speed_width;
        let pro_mode_width =
            u16::try_from(pro_mode.map_or(0, UnicodeWidthStr::width)).unwrap_or(u16::MAX);
        let pro_mode_start = if pro_mode.is_some() {
            natural_pro_mode_start.min(
                content_end
                    .saturating_sub(pro_mode_width)
                    .max(content_start),
            )
        } else {
            natural_pro_mode_start
        };
        if speed_start < content_end && speed_start.saturating_add(speed_width) <= pro_mode_start {
            self.chrome_hits.push((
                ComposerChromeTarget::Speed,
                Rect::new(speed_start, top, speed_width, 1),
            ));
            buffer.set_stringn(
                speed_start,
                top,
                speed,
                usize::from(content_end - speed_start),
                Style::default()
                    .fg(theme.speed(status.speed))
                    .add_modifier(Modifier::BOLD),
            );
        }
        if let Some(pro_mode) = pro_mode
            && pro_mode_start < content_end
        {
            buffer.set_stringn(
                pro_mode_start,
                top,
                pro_mode,
                usize::from(content_end - pro_mode_start),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            );
        }
        let directory = format!(" {} ", self.workspace);
        let directory_width = directory.width().min(content_width);
        let directory_start =
            content_end.saturating_sub(u16::try_from(directory_width).unwrap_or(u16::MAX));
        let badge = build_badge(crate::app::installation::current()).filter(|(text, _)| {
            text.width() <= usize::from(directory_start.saturating_sub(content_start))
        });
        let badge_width = badge.map_or(0, |(text, _)| text.width());
        let badge_start =
            directory_start.saturating_sub(u16::try_from(badge_width).unwrap_or(u16::MAX));
        let hint_space = usize::from(badge_start.saturating_sub(content_start));
        let entry_hint = entry_hint(theme, self.draft.text().is_empty());
        if entry_hint.width() <= hint_space {
            buffer.set_line(
                content_start,
                bottom,
                &entry_hint,
                u16::try_from(hint_space).unwrap_or(u16::MAX),
            );
        }
        if shell_mode {
            buffer.set_stringn(
                content_start,
                bottom,
                " shell ",
                hint_space,
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            );
        }
        if let Some((text, color)) = badge {
            buffer.set_stringn(
                badge_start,
                bottom,
                text,
                badge_width,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            );
        }
        buffer.set_stringn(
            directory_start,
            bottom,
            directory,
            directory_width,
            Style::default().fg(theme.muted()),
        );
    }

    fn border_style(&self, theme: &Theme) -> Style {
        Style::default().fg(if self.task_wave.is_some() {
            Color::Green
        } else if self.draft.text().starts_with('!') {
            Color::Yellow
        } else {
            theme.border()
        })
    }
}

fn entry_hint(theme: &Theme, include_actions: bool) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    if include_actions {
        spans.extend([
            Span::styled("/", Style::reset()),
            Span::styled(" actions · ", Style::default().fg(theme.muted())),
        ]);
    }
    spans.extend([
        Span::styled("@", Style::reset()),
        Span::styled(" paths · ", Style::default().fg(theme.muted())),
        Span::styled("@@", Style::reset()),
        Span::styled(" sessions ", Style::default().fg(theme.muted())),
    ]);
    Line::from(spans)
}

impl Component for Composer {
    type Event = ComposerEvent;
    type Effect = ComposerEffect;

    fn update(&mut self, event: Self::Event) -> ComponentUpdate<Self::Effect> {
        let update = Composer::update(self, event);
        ComponentUpdate {
            effects: update.effect.into_iter().collect(),
            render: if update.changed {
                RenderRequest::Immediate
            } else {
                RenderRequest::None
            },
        }
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        Composer::render(self, frame, area, theme);
    }
}

impl ComposerUpdate {
    fn unchanged() -> Self {
        Self {
            effect: None,
            changed: false,
        }
    }

    fn changed() -> Self {
        Self {
            effect: None,
            changed: true,
        }
    }

    fn effect(effect: ComposerEffect, changed: bool) -> Self {
        Self {
            effect: Some(effect),
            changed,
        }
    }

    fn from_change(changed: bool) -> Self {
        Self {
            effect: None,
            changed,
        }
    }
}

fn context_percent(tokens: u64, window: u64) -> u64 {
    tokens.saturating_mul(100).saturating_add(window / 2) / window.max(1)
}

/// Replaces a status wave when its text changes, starting the new wave
/// immediately. Returns whether the status changed.
fn replace_wave(
    wave: &mut Option<WavedText>,
    status: Option<String>,
    color: Color,
    now: Instant,
) -> bool {
    if wave.as_ref().map(WavedText::text) == status.as_deref() {
        return false;
    }
    *wave = status.map(|status| {
        let mut replacement = WavedText::new(status, color);
        replacement.set_active(true, now);
        replacement
    });
    true
}

fn draw_symbol(buffer: &mut Buffer, x: u16, y: u16, symbol: &str, style: Style) {
    buffer[(x, y)].set_symbol(symbol).set_style(style);
}

#[cfg(test)]
mod tests {
    use super::{
        super::selection::{Selection, Surface, TextRange},
        Composer, ComposerChromeTarget, ComposerEffect, ComposerEvent, InputMode, LiveSessions,
        StatusLine, build_badge, context_percent,
    };
    use crate::{
        app::{
            config::{ReasoningEffort, ReasoningMode, Speed},
            installation::{self, InstallationKind},
            theme::Theme,
        },
        core::context::ContextBudget,
    };
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use nanocodex::{
        ClaudeModel, HarnessModel as Model, Model as CodexModel,
        agent::input::{PromptInput, UserInput},
    };
    use ratatui::{
        Terminal,
        backend::TestBackend,
        buffer::Cell,
        layout::{Position, Rect},
        style::{Color, Modifier},
    };
    use std::{
        path::Path,
        time::{Duration, Instant},
    };
    use unicode_width::UnicodeWidthStr;

    fn new_composer() -> Composer {
        Composer::new(Path::new("/work"), ReasoningEffort::Medium)
    }

    /// The status line of [`new_composer`].
    fn idle_status() -> StatusLine {
        StatusLine {
            context: "0%/272k".to_owned(),
            input_mode: None,
            task: None,
            activity: None,
            subagents: 0,
            live_sessions: None,
            turn_timer: None,
            model: Model::Codex(CodexModel::Sol),
            effort: ReasoningEffort::Medium,
            speed: Speed::Standard,
            pro: false,
        }
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> ComposerEvent {
        ComposerEvent::Terminal(Event::Key(KeyEvent::new(code, modifiers)))
    }

    fn render(composer: &mut Composer, width: u16, height: u16) -> Terminal<TestBackend> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| composer.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        terminal
    }

    fn render_with_selection(
        composer: &mut Composer,
        width: u16,
        height: u16,
        selection: TextRange,
    ) -> Terminal<TestBackend> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                composer.render_focused_with_selection(
                    frame,
                    frame.area(),
                    &Theme::default(),
                    true,
                    Some(selection),
                );
            })
            .unwrap();
        terminal
    }

    fn rows(terminal: &Terminal<TestBackend>) -> Vec<String> {
        let buffer = terminal.backend().buffer();
        buffer
            .content
            .chunks(usize::from(buffer.area.width))
            .map(|cells| cells.iter().map(|cell| cell.symbol()).collect())
            .collect()
    }

    /// The cells on row `y` that display the first occurrence of `text`, which must follow only
    /// single-width symbols on that row.
    fn cells_of<'a>(terminal: &'a Terminal<TestBackend>, y: u16, text: &str) -> &'a [Cell] {
        let buffer = terminal.backend().buffer();
        let row = &rows(terminal)[usize::from(y)];
        let x = row[..row.find(text).expect("text should be rendered")].width();
        let start = usize::from(y) * usize::from(buffer.area.width) + x;
        &buffer.content[start..start + text.width()]
    }

    /// A footer row for `/work` written as development builds render it; other builds draw border
    /// in place of the development badge.
    fn footer(development: &str) -> String {
        if installation::current().is_development() {
            development.to_owned()
        } else {
            development.replace(" ◉ dev ", "───────")
        }
    }

    #[test]
    fn empty_composer_matches_the_pi_chrome() {
        let mut composer = new_composer();
        assert_eq!(composer.status_line(), idle_status());

        let terminal = render(&mut composer, 60, 5);
        assert_eq!(
            rows(&terminal),
            [
                "╭─ 0%/272k ──────────────────────── gpt-6.1-sol  medium 󰳗 ─╮".to_owned(),
                "│                                                          │".to_owned(),
                "│                                                          │".to_owned(),
                "│                                                          │".to_owned(),
                footer("╰─ / actions · @ paths · @@ sessions ─────── ◉ dev  /work ─╯"),
            ]
        );

        let actions = cells_of(&terminal, 4, "/ actions");
        assert_eq!(actions[0].fg, Color::Reset);
        assert_eq!(actions[2].fg, Theme::default().muted());
    }

    #[test]
    fn top_border_lays_out_status_then_timer_and_controls() {
        let mut composer = new_composer();
        let now = Instant::now();
        for event in [
            ComposerEvent::ContextBudget(ContextBudget {
                active_tokens: 34_000,
                window_tokens: 1_000_000,
            }),
            ComposerEvent::TaskStatus {
                status: Some("Preparing handoff…".to_owned()),
                now,
            },
            ComposerEvent::Activity {
                active: true,
                status: Some("Thinking…".to_owned()),
                now,
            },
            ComposerEvent::ActiveSubagents { count: 2, now },
            ComposerEvent::LiveSessions(Some(LiveSessions {
                live: 3,
                running: 1,
            })),
            ComposerEvent::TurnStarted {
                elapsed: Duration::from_secs(65),
                now,
            },
            ComposerEvent::SetSpeed(Speed::Fast),
            ComposerEvent::SetReasoningMode(ReasoningMode::Pro),
        ] {
            composer.update(event);
        }

        assert_eq!(
            composer.status_line(),
            StatusLine {
                context: "3%/1m".to_owned(),
                task: Some("Preparing handoff…".to_owned()),
                activity: Some("Thinking…".to_owned()),
                subagents: 2,
                live_sessions: Some(LiveSessions {
                    live: 3,
                    running: 1
                }),
                turn_timer: Some("1m 5s".to_owned()),
                speed: Speed::Fast,
                pro: true,
                ..idle_status()
            }
        );
        assert_eq!(
            rows(&render(&mut composer, 120, 5))[0],
            "╭─ 3%/1m Preparing handoff… Thinking… 2 subagents 3 sessions, 1 running ──────────── 1m 5s  gpt-6.1-sol  medium 󰤇 pro ─╮"
        );
    }

    #[test]
    fn input_mode_hint_follows_context_usage() {
        let mut composer = new_composer();
        composer.update(ComposerEvent::InputMode(InputMode::Reflection));

        assert_eq!(
            composer.status_line(),
            StatusLine {
                input_mode: Some("Reflection instructions · enter start · esc cancel"),
                ..idle_status()
            }
        );
        assert_eq!(
            rows(&render(&mut composer, 90, 5))[0],
            "╭─ 0%/272k Reflection instructions · enter start · esc cancel ─── gpt-6.1-sol  medium 󰳗 ─╮"
        );
    }

    #[test]
    fn composer_chrome_uses_the_model_palette() {
        for model in [CodexModel::Luna, CodexModel::Sol, CodexModel::Astra].map(Model::Codex) {
            let mut composer = new_composer();
            composer.update(ComposerEvent::SetModel(model));
            assert_eq!(composer.status_line().model, model);

            let terminal = render(&mut composer, 60, 5);
            let label = cells_of(&terminal, 0, model.as_str());
            assert!(
                label
                    .iter()
                    .all(|cell| cell.fg == Theme::default().model(model))
            );
        }
    }

    #[test]
    fn task_status_turns_the_border_green() {
        let mut composer = new_composer();
        composer.update(ComposerEvent::TaskStatus {
            status: Some("Preparing handoff…".to_owned()),
            now: Instant::now(),
        });

        assert_eq!(
            composer.status_line(),
            StatusLine {
                task: Some("Preparing handoff…".to_owned()),
                ..idle_status()
            }
        );
        let terminal = render(&mut composer, 80, 5);
        assert_eq!(terminal.backend().buffer()[(0, 0)].fg, Color::Green);
    }

    #[test]
    fn turn_timer_advances_each_second_until_the_turn_finishes() {
        let mut composer = new_composer();
        let started_at = Instant::now();
        composer.update(ComposerEvent::TurnStarted {
            elapsed: Duration::from_secs(65),
            now: started_at,
        });
        assert_eq!(composer.status_line().turn_timer.as_deref(), Some("1m 5s"));

        let update = composer.update(ComposerEvent::AnimationFrame(
            started_at + Duration::from_secs(2),
        ));
        assert!(update.changed);
        assert_eq!(composer.status_line().turn_timer.as_deref(), Some("1m 7s"));

        composer.update(ComposerEvent::TurnFinished);
        assert_eq!(composer.status_line().turn_timer, None);
    }

    #[test]
    fn completing_one_run_keeps_the_next_active_run_timed() {
        let mut composer = new_composer();
        let now = Instant::now();
        composer.update(ComposerEvent::TurnStarted {
            elapsed: Duration::from_secs(65),
            now,
        });
        composer.update(ComposerEvent::TurnStarted {
            elapsed: Duration::from_secs(5),
            now,
        });
        composer.update(ComposerEvent::AnimationFrame(now + Duration::from_secs(2)));

        composer.update(ComposerEvent::TurnFinished);

        assert_eq!(composer.status_line().turn_timer.as_deref(), Some("7s"));
        assert!(composer.animation_deadline().is_some());
    }

    #[test]
    fn speed_icon_displays_the_effective_tier_and_is_clickable() {
        let mut composer = new_composer();
        composer.update(ComposerEvent::SetSpeed(Speed::Ultrafast));
        for (model, effective, icon) in [
            (Model::Codex(CodexModel::Astra), Speed::Ultrafast, "󰑣"),
            (Model::Codex(CodexModel::Sol), Speed::Ultrafast, "󰑣"),
            (Model::Codex(CodexModel::Luna), Speed::Fast, "󰤇"),
            (Model::Claude(ClaudeModel::Haiku55), Speed::Standard, "󰳗"),
            (Model::Claude(ClaudeModel::Opus55), Speed::Fast, "󰤇"),
            (Model::Claude(ClaudeModel::Sonnet55), Speed::Standard, "󰳗"),
            (Model::Claude(ClaudeModel::Fable51), Speed::Standard, "󰳗"),
        ] {
            composer.update(ComposerEvent::SetModel(model));
            assert_eq!(composer.status_line().speed, effective);
            assert_eq!(composer.speed(), Speed::Ultrafast);
            let terminal = render(&mut composer, 72, 5);
            let hit = composer
                .chrome_area(ComposerChromeTarget::Speed)
                .expect("speed should have a hit target");
            assert_eq!(
                composer.chrome_target(Position::new(hit.x, hit.y)),
                Some(ComposerChromeTarget::Speed)
            );
            let cell = &terminal.backend().buffer()[(hit.x, hit.y)];
            assert_eq!(cell.symbol(), icon);
            assert_eq!(cell.fg, Theme::default().speed(effective));
            assert!(cell.modifier.contains(Modifier::BOLD));
        }
    }

    #[test]
    fn claude_models_show_their_effective_speed_and_hide_pro() {
        for (model, speed) in [
            (Model::Claude(ClaudeModel::Haiku55), Speed::Standard),
            (Model::Claude(ClaudeModel::Sonnet55), Speed::Standard),
            (Model::Claude(ClaudeModel::Opus55), Speed::Fast),
            (Model::Claude(ClaudeModel::Fable51), Speed::Standard),
        ] {
            let mut composer = Composer::new(Path::new("/work"), ReasoningEffort::Max);
            composer.update(ComposerEvent::SetModel(model));
            composer.update(ComposerEvent::SetSpeed(Speed::Ultrafast));
            composer.update(ComposerEvent::SetReasoningMode(ReasoningMode::Pro));

            assert_eq!(
                composer.status_line(),
                StatusLine {
                    model,
                    effort: ReasoningEffort::Max,
                    speed,
                    ..idle_status()
                }
            );
        }
    }

    #[test]
    fn pro_mode_places_a_green_badge_after_speed() {
        let mut composer = new_composer();
        composer.update(ComposerEvent::SetSpeed(Speed::Fast));
        composer.update(ComposerEvent::SetReasoningMode(ReasoningMode::Pro));
        assert!(composer.status_line().pro);

        let terminal = render(&mut composer, 60, 5);
        assert_eq!(
            rows(&terminal)[0],
            "╭─ 0%/272k ──────────────────── gpt-6.1-sol  medium 󰤇 pro ─╮"
        );
        for cell in cells_of(&terminal, 0, "pro") {
            assert_eq!(cell.fg, Color::Green);
            assert!(cell.modifier.contains(Modifier::BOLD));
        }
    }

    #[test]
    fn narrow_composer_prioritizes_the_complete_pro_badge() {
        let mut composer = new_composer();
        composer.update(ComposerEvent::SetSpeed(Speed::Fast));
        composer.update(ComposerEvent::SetReasoningMode(ReasoningMode::Pro));

        let terminal = render(&mut composer, 28, 5);
        assert_eq!(rows(&terminal)[0], "╭─ gpt-6.1-sol  mediumpro ─╮");
        assert!(composer.chrome_area(ComposerChromeTarget::Speed).is_none());
        assert!(
            cells_of(&terminal, 0, "pro")
                .iter()
                .all(|cell| cell.fg == Color::Green)
        );

        assert_eq!(rows(&render(&mut composer, 6, 5))[0], "╭─pr─╮");
    }

    #[test]
    fn pro_badge_follows_the_reasoning_mode() {
        let mut composer = new_composer();
        composer.update(ComposerEvent::SetReasoningMode(ReasoningMode::Pro));
        assert!(composer.status_line().pro);

        composer.update(ComposerEvent::SetReasoningMode(ReasoningMode::Standard));
        assert_eq!(composer.status_line(), idle_status());
    }

    #[test]
    fn development_badge_matches_the_installation_kind() {
        let mut composer = new_composer();
        let terminal = render(&mut composer, 60, 5);
        assert_eq!(
            rows(&terminal)[4],
            footer("╰─ / actions · @ paths · @@ sessions ─────── ◉ dev  /work ─╯")
        );

        if installation::current().is_development() {
            for cell in cells_of(&terminal, 4, "◉ dev") {
                assert_eq!(cell.fg, Color::Red);
                assert!(cell.modifier.contains(Modifier::BOLD));
            }
        }
    }

    #[test]
    fn only_builds_that_are_not_official_releases_carry_a_badge() {
        let pre_release = InstallationKind::PreRelease {
            revision: "0123456789ab".to_owned(),
        };
        assert_eq!(
            build_badge(&InstallationKind::Development),
            Some((" ◉ dev ", Color::Red))
        );
        assert_eq!(
            build_badge(&pre_release),
            Some((" ◉ pre-release ", Color::Blue))
        );
        assert_eq!(build_badge(&InstallationKind::ReleaseArchive), None);
    }

    #[test]
    fn entry_hint_keeps_file_and_session_shortcuts_visible_while_typing() {
        let mut composer = new_composer();
        assert_eq!(
            rows(&render(&mut composer, 60, 5))[4],
            footer("╰─ / actions · @ paths · @@ sessions ─────── ◉ dev  /work ─╯")
        );

        composer.replace_draft("hello".to_owned());
        assert_eq!(
            rows(&render(&mut composer, 60, 5))[4],
            footer("╰─ @ paths · @@ sessions ─────────────────── ◉ dev  /work ─╯")
        );

        composer.replace_draft(String::new());
        assert_eq!(
            rows(&render(&mut composer, 20, 5))[4],
            footer("╰─── ◉ dev  /work ─╯")
        );
    }

    #[test]
    fn active_turn_waves_the_transient_status_after_context_usage() {
        let mut composer = new_composer();
        composer.update(ComposerEvent::Activity {
            active: true,
            status: Some("Running exec command…".to_owned()),
            now: Instant::now(),
        });

        assert_eq!(
            composer.status_line(),
            StatusLine {
                activity: Some("Running exec command…".to_owned()),
                ..idle_status()
            }
        );
        let terminal = render(&mut composer, 80, 5);
        assert!(
            cells_of(&terminal, 0, "Running exec command…")
                .iter()
                .any(|cell| cell.fg == Color::Cyan)
        );
        assert!(composer.animation_deadline().is_some());
    }

    #[test]
    fn active_subagents_wave_after_the_transient_status() {
        let mut composer = new_composer();
        let now = Instant::now();
        composer.update(ComposerEvent::Activity {
            active: true,
            status: Some("Thinking…".to_owned()),
            now,
        });
        composer.update(ComposerEvent::ActiveSubagents { count: 2, now });

        assert_eq!(
            composer.status_line(),
            StatusLine {
                activity: Some("Thinking…".to_owned()),
                subagents: 2,
                ..idle_status()
            }
        );
        let terminal = render(&mut composer, 72, 5);
        let subagents = cells_of(&terminal, 0, "2 subagents");
        assert!(subagents.iter().any(|cell| cell.fg == Color::Yellow));
        let hit = composer
            .chrome_area(ComposerChromeTarget::Subagents)
            .expect("subagents should have a hit target");
        assert_eq!(usize::from(hit.width), subagents.len());
        assert!(composer.animation_deadline().is_some());
    }

    #[test]
    fn composer_grows_from_three_through_six_rows() {
        let mut composer = new_composer();
        assert_eq!(composer.desired_height(20), 5);

        composer.replace_draft("1\n2\n3\n4\n5\n6".to_owned());
        assert_eq!(composer.desired_height(20), 8);

        composer.replace_draft("1\n2\n3\n4\n5\n6\n7".to_owned());
        assert_eq!(composer.desired_height(20), 8);
    }

    #[test]
    fn overflow_scrolls_to_keep_the_cursor_visible() {
        let mut composer = new_composer();
        composer.replace_draft("one\ntwo\nthree\nfour\nfive\nsix\nseven".to_owned());
        let terminal = render(&mut composer, 30, 8);

        assert_eq!(composer.scroll, 1);
        assert_eq!(
            rows(&terminal)[1..7],
            [
                "│two                         │",
                "│three                       │",
                "│four                        │",
                "│five                        │",
                "│six                         │",
                "│seven                       │",
            ]
        );
    }

    #[test]
    fn resize_reflows_wrapped_text() {
        let mut composer = new_composer();
        composer.replace_draft("alpha beta gamma delta".to_owned());

        render(&mut composer, 14, 5);
        assert_eq!(composer.desired_height(14), 5);
        assert_eq!(composer.last_width, 12);

        render(&mut composer, 8, 6);
        assert_eq!(composer.desired_height(8), 6);
        assert_eq!(composer.last_width, 6);
    }

    #[test]
    fn cursor_movement_respects_graphemes_and_display_width() {
        let mut composer = new_composer();
        composer.replace_draft("a界e\u{301}".to_owned());
        composer.update(key(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(composer.cursor(), 4);
        composer.update(key(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(composer.cursor(), 1);

        let terminal = render(&mut composer, 20, 5);
        assert_eq!(terminal.backend().cursor_position(), Position::new(2, 1));
    }

    #[test]
    fn paste_and_editor_replacement_preserve_multiline_text() {
        let mut composer = new_composer();
        composer.update(ComposerEvent::Terminal(Event::Paste("one\ntwo".to_owned())));
        assert_eq!(composer.draft(), "one\ntwo");

        composer.update(ComposerEvent::ReplaceDraft("edited\ndraft".to_owned()));
        assert_eq!(composer.draft(), "edited\ndraft");
        assert_eq!(composer.cursor(), composer.draft().len());
    }

    #[test]
    fn paste_and_editor_replacement_normalize_carriage_returns() {
        let mut composer = new_composer();
        composer.update(ComposerEvent::Terminal(Event::Paste(
            "one\r\ntwo\rthree".to_owned(),
        )));
        assert_eq!(composer.draft(), "one\ntwo\nthree");

        composer.update(ComposerEvent::ReplaceDraft("edited\r\ndraft".to_owned()));
        assert_eq!(composer.draft(), "edited\ndraft");
        assert_eq!(composer.cursor(), composer.draft().len());
    }

    #[test]
    fn pasted_controls_are_visible_without_changing_the_submission() {
        let mut composer = new_composer();
        let pasted = "one\ttwo\u{1b}three";
        composer.update(ComposerEvent::Terminal(Event::Paste(pasted.to_owned())));

        let terminal = render(&mut composer, 40, 5);
        assert_eq!(
            rows(&terminal)[1],
            "│one    two�three                      │"
        );
        assert_eq!(terminal.backend().cursor_position(), Position::new(17, 1));

        let submission = composer.take_submission().unwrap();
        assert_eq!(submission.display_text(), pasted);
    }

    #[test]
    fn pasted_images_render_as_numbered_blue_tokens_and_submit_as_images() {
        let mut composer = new_composer();
        composer.update(ComposerEvent::Terminal(Event::Paste("inspect ".to_owned())));
        composer.update(ComposerEvent::PasteImage(
            "data:image/png;base64,first".to_owned(),
        ));
        composer.update(ComposerEvent::PasteImage(
            "data:image/png;base64,second".to_owned(),
        ));

        assert_eq!(composer.draft(), "inspect [Image #1][Image #2]");
        let terminal = render(&mut composer, 50, 5);
        let buffer = terminal.backend().buffer();
        for x in 9..29 {
            assert_eq!(buffer[(x, 1)].fg, Color::Blue);
        }

        let update = composer.update(key(KeyCode::Enter, KeyModifiers::NONE));
        let Some(ComposerEffect::Submit(submission)) = update.effect else {
            panic!("image prompt should submit");
        };
        assert_eq!(submission.display_text(), "inspect [Image #1][Image #2]");
        let PromptInput::Content(content) = submission.agent_prompt().instruction else {
            panic!("image prompt should use multimodal content");
        };
        assert!(matches!(&content[0], UserInput::Text { text } if text == "inspect "));
        assert!(
            matches!(&content[1], UserInput::Image { image_url, .. } if image_url.ends_with("first"))
        );
        assert!(
            matches!(&content[2], UserInput::Image { image_url, .. } if image_url.ends_with("second"))
        );
    }

    #[test]
    fn deleting_an_image_token_removes_its_attachment_atomically() {
        let mut composer = new_composer();
        composer.update(ComposerEvent::PasteImage(
            "data:image/png;base64,removed".to_owned(),
        ));

        composer.update(key(KeyCode::Backspace, KeyModifiers::NONE));

        assert!(composer.draft().is_empty());
        assert_eq!(composer.images().count(), 0);
    }

    #[test]
    fn option_backspace_deletes_the_previous_word() {
        let mut composer = new_composer();
        composer.replace_draft("one two  ".to_owned());

        let update = composer.update(key(KeyCode::Backspace, KeyModifiers::ALT));

        assert!(update.changed);
        assert_eq!(composer.draft(), "one ");
        assert_eq!(composer.cursor(), composer.draft().len());
    }

    #[test]
    fn option_backspace_removes_an_image_attachment_with_its_token() {
        let mut composer = new_composer();
        composer.update(ComposerEvent::Terminal(Event::Paste("inspect ".to_owned())));
        composer.update(ComposerEvent::PasteImage(
            "data:image/png;base64,removed".to_owned(),
        ));

        composer.update(key(KeyCode::Backspace, KeyModifiers::ALT));

        assert_eq!(composer.draft(), "inspect ");
        assert_eq!(composer.images().count(), 0);
    }

    #[test]
    fn readline_shortcuts_move_by_character_and_stay_on_the_logical_line() {
        let mut composer = new_composer();
        composer.replace_draft("one\ntwo\nthree".to_owned());
        composer.draft.set_cursor("one\nt".len());

        composer.update(key(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert_eq!(composer.cursor(), "one\n".len());
        let update = composer.update(key(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert!(!update.changed);
        assert_eq!(composer.cursor(), "one\n".len());

        composer.update(key(KeyCode::Char('e'), KeyModifiers::CONTROL));
        assert_eq!(composer.cursor(), "one\ntwo".len());
        let update = composer.update(key(KeyCode::Char('e'), KeyModifiers::CONTROL));
        assert!(!update.changed);
        assert_eq!(composer.cursor(), "one\ntwo".len());

        composer.update(key(KeyCode::Char('b'), KeyModifiers::CONTROL));
        assert_eq!(composer.cursor(), "one\ntw".len());
        composer.update(key(KeyCode::Char('f'), KeyModifiers::CONTROL));
        assert_eq!(composer.cursor(), "one\ntwo".len());
    }

    #[test]
    fn ctrl_k_deletes_to_logical_line_end_then_removes_the_newline() {
        let mut composer = new_composer();
        composer.replace_draft("one\ntwo three\nfour".to_owned());
        composer.draft.set_cursor("one\ntwo".len());

        let update = composer.update(key(KeyCode::Char('k'), KeyModifiers::CONTROL));
        assert!(update.changed);
        assert_eq!(composer.draft(), "one\ntwo\nfour");
        assert_eq!(composer.cursor(), "one\ntwo".len());

        composer.update(key(KeyCode::Char('k'), KeyModifiers::CONTROL));
        assert_eq!(composer.draft(), "one\ntwofour");

        composer.draft.set_cursor(composer.draft().len());
        let update = composer.update(key(KeyCode::Char('k'), KeyModifiers::CONTROL));
        assert!(!update.changed);
        assert_eq!(composer.draft(), "one\ntwofour");
    }

    #[test]
    fn ctrl_k_uses_logical_lines_in_wrapped_unicode_text() {
        let mut composer = new_composer();
        composer.replace_draft("界 alpha beta gamma\nnext".to_owned());
        composer.draft.set_cursor("界 alpha".len());
        render(&mut composer, 8, 6);

        composer.update(key(KeyCode::Char('k'), KeyModifiers::CONTROL));

        assert_eq!(composer.draft(), "界 alpha\nnext");
        assert_eq!(composer.cursor(), "界 alpha".len());
        assert!(composer.draft.cached_layout().is_none());
    }

    #[test]
    fn ctrl_k_removes_images_and_shifts_later_attachments() {
        let mut composer = new_composer();
        composer.update(ComposerEvent::Terminal(Event::Paste("é ".to_owned())));
        let cursor = composer.cursor();
        composer.update(ComposerEvent::PasteImage(
            "data:image/png;base64,removed".to_owned(),
        ));
        composer.update(ComposerEvent::Terminal(Event::Paste(
            " tail\nkeep ".to_owned(),
        )));
        composer.update(ComposerEvent::PasteImage(
            "data:image/png;base64,kept".to_owned(),
        ));
        composer.draft.set_cursor(cursor);

        composer.update(key(KeyCode::Char('k'), KeyModifiers::CONTROL));

        assert_eq!(composer.draft(), "é \nkeep [Image #2]");
        assert_eq!(
            composer.images().collect::<Vec<_>>(),
            [("[Image #2]", "data:image/png;base64,kept")]
        );
        assert_eq!(
            composer.draft.image_ranges().first(),
            Some(&("é \nkeep ".len()..composer.draft().len()))
        );
    }

    #[test]
    fn readline_shortcuts_require_exact_modifiers() {
        let mut composer = new_composer();
        composer.replace_draft("abcd".to_owned());
        composer.draft.set_cursor(2);

        let update = composer.update(key(
            KeyCode::Char('b'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ));
        assert!(!update.changed);
        assert_eq!(composer.draft(), "abcd");
        assert_eq!(composer.cursor(), 2);

        composer.update(key(
            KeyCode::Char('b'),
            KeyModifiers::ALT | KeyModifiers::SHIFT,
        ));
        assert_eq!(composer.draft(), "abbcd");
        assert_eq!(composer.cursor(), 3);
    }

    #[test]
    fn readline_word_movement_skips_delimiters_between_alphanumeric_words() {
        let mut composer = new_composer();
        composer.replace_draft("foo...bar".to_owned());

        composer.update(key(KeyCode::Char('b'), KeyModifiers::ALT));
        assert_eq!(composer.cursor(), "foo...".len());
        composer.update(key(KeyCode::Char('b'), KeyModifiers::ALT));
        assert_eq!(composer.cursor(), 0);

        composer.update(key(KeyCode::Char('f'), KeyModifiers::ALT));
        assert_eq!(composer.cursor(), "foo".len());
        composer.update(key(KeyCode::Char('f'), KeyModifiers::ALT));
        assert_eq!(composer.cursor(), composer.draft().len());

        composer.replace_draft("can't".to_owned());
        composer.update(key(KeyCode::Char('b'), KeyModifiers::ALT));
        assert_eq!(composer.cursor(), "can'".len());
        composer.update(key(KeyCode::Char('b'), KeyModifiers::ALT));
        assert_eq!(composer.cursor(), 0);

        composer.replace_draft("alpha   beta".to_owned());
        composer.draft.set_cursor("alpha ".len());
        composer.update(key(KeyCode::Char('f'), KeyModifiers::ALT));
        assert_eq!(composer.cursor(), composer.draft().len());
        composer.draft.set_cursor("alpha  ".len());
        composer.update(key(KeyCode::Char('b'), KeyModifiers::ALT));
        assert_eq!(composer.cursor(), 0);

        composer.replace_draft("你好".to_owned());
        composer.draft.set_cursor(0);
        composer.update(key(KeyCode::Char('f'), KeyModifiers::ALT));
        assert_eq!(composer.cursor(), composer.draft().len());
        composer.update(key(KeyCode::Char('b'), KeyModifiers::ALT));
        assert_eq!(composer.cursor(), 0);
    }

    #[test]
    fn readline_word_movement_treats_images_as_atomic() {
        let mut composer = new_composer();
        composer.update(ComposerEvent::Terminal(Event::Paste("inspect ".to_owned())));
        composer.update(ComposerEvent::PasteImage(
            "data:image/png;base64,attached".to_owned(),
        ));

        composer.update(key(KeyCode::Char('b'), KeyModifiers::ALT));
        assert_eq!(composer.cursor(), "inspect ".len());
        composer.update(key(KeyCode::Char('f'), KeyModifiers::ALT));
        assert_eq!(composer.cursor(), composer.draft().len());
    }

    #[test]
    fn readline_vertical_movement_treats_images_as_atomic() {
        let mut composer = new_composer();
        composer.update(ComposerEvent::PasteImage(
            "data:image/png;base64,attached".to_owned(),
        ));
        render(&mut composer, 5, 5);

        composer.update(key(KeyCode::Char('a'), KeyModifiers::CONTROL));
        composer.update(key(KeyCode::Char('n'), KeyModifiers::CONTROL));
        assert_eq!(composer.cursor(), composer.draft().len());
        composer.update(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert_eq!(composer.cursor(), 0);

        let mut composer = new_composer();
        composer.update(ComposerEvent::Terminal(Event::Paste("a".to_owned())));
        composer.update(ComposerEvent::PasteImage(
            "data:image/png;base64,attached".to_owned(),
        ));
        composer.update(ComposerEvent::Terminal(Event::Paste("\n12345".to_owned())));

        composer.update(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert_eq!(composer.cursor(), "a".len());
        composer.update(key(KeyCode::Char('n'), KeyModifiers::CONTROL));
        assert_eq!(composer.cursor(), composer.draft().len());

        let mut composer = new_composer();
        composer.update(ComposerEvent::Terminal(Event::Paste(
            "123456789\na".to_owned(),
        )));
        composer.update(ComposerEvent::PasteImage(
            "data:image/png;base64,attached".to_owned(),
        ));
        composer.update(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
        composer.update(key(KeyCode::Char('b'), KeyModifiers::CONTROL));
        composer.update(key(KeyCode::Char('f'), KeyModifiers::CONTROL));

        composer.update(key(KeyCode::Char('n'), KeyModifiers::CONTROL));
        assert_eq!(composer.cursor(), composer.draft().len());
    }

    #[test]
    fn readline_vertical_shortcuts_restore_the_unsent_draft_after_history() {
        let mut composer = new_composer();
        composer.replace_draft("older".to_owned());
        composer.update(key(KeyCode::Enter, KeyModifiers::NONE));
        composer.replace_draft("newer".to_owned());
        composer.update(key(KeyCode::Enter, KeyModifiers::NONE));
        composer.replace_draft("top\nbottom".to_owned());

        composer.update(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert_eq!(composer.draft(), "top\nbottom");
        assert_eq!(composer.cursor(), "top".len());
        composer.update(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert_eq!(composer.draft(), "newer");
        composer.update(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert_eq!(composer.draft(), "older");
        composer.update(key(KeyCode::Char('n'), KeyModifiers::CONTROL));
        assert_eq!(composer.draft(), "newer");
        composer.update(key(KeyCode::Char('n'), KeyModifiers::CONTROL));
        assert_eq!(composer.draft(), "top\nbottom");
        assert_eq!(composer.cursor(), composer.draft().len());
    }

    #[test]
    fn readline_horizontal_shortcuts_detach_recalled_history() {
        for (code, modifiers) in [
            (KeyCode::Char('a'), KeyModifiers::CONTROL),
            (KeyCode::Char('b'), KeyModifiers::CONTROL),
            (KeyCode::Char('k'), KeyModifiers::CONTROL),
            (KeyCode::Char('b'), KeyModifiers::ALT),
        ] {
            let mut composer = new_composer();
            composer.replace_draft("previous".to_owned());
            composer.update(key(KeyCode::Enter, KeyModifiers::NONE));
            composer.update(key(KeyCode::Up, KeyModifiers::NONE));

            composer.update(key(code, modifiers));
            composer.update(key(KeyCode::Char('n'), KeyModifiers::CONTROL));

            assert_eq!(composer.draft(), "previous");
        }
    }

    #[test]
    fn submission_trims_nonempty_prompts_and_preserves_empty_drafts() {
        let mut composer = new_composer();
        composer.replace_draft("  inspect this  \n".to_owned());
        let update = composer.update(key(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(
            update.effect,
            Some(ComposerEffect::Submit("inspect this".to_owned().into()))
        );
        assert!(composer.draft().is_empty());

        composer.replace_draft("   \n".to_owned());
        let update = composer.update(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(update.effect, None);
        assert_eq!(composer.draft(), "   \n");
    }

    #[test]
    fn leading_bang_uses_yellow_shell_chrome_and_submits_only_the_command() {
        let mut composer = new_composer();
        composer.replace_draft("!  printf hello  ".to_owned());

        let terminal = render(&mut composer, 80, 5);
        let buffer = terminal.backend().buffer();
        for position in [(0, 0), (79, 0), (0, 2), (79, 2), (0, 4), (79, 4)] {
            assert_eq!(buffer[position].fg, Color::Yellow);
        }
        assert_eq!(
            rows(&terminal)[0],
            "╭─ 0%/272k ──────────────────────────────────────────── gpt-6.1-sol  medium 󰳗 ─╮"
        );
        assert_eq!(
            rows(&terminal)[4],
            footer(
                "╰─ shell s · @@ sessions ─────────────────────────────────────── ◉ dev  /work ─╯"
            )
        );

        let update = composer.update(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            update.effect,
            Some(ComposerEffect::RunShell("printf hello".to_owned()))
        );
        assert!(composer.draft().is_empty());
    }

    #[test]
    fn bang_without_a_command_is_not_submitted() {
        let mut composer = new_composer();
        composer.replace_draft("!   ".to_owned());

        let update = composer.update(key(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(update.effect, None);
        assert_eq!(composer.draft(), "!   ");
    }

    #[test]
    fn arrows_cycle_submitted_prompts_and_restore_the_unsent_draft() {
        let mut composer = new_composer();
        for prompt in ["first", "second"] {
            composer.replace_draft(prompt.to_owned());
            composer.update(key(KeyCode::Enter, KeyModifiers::NONE));
        }
        composer.replace_draft("unfinished\nline".to_owned());

        composer.update(key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(composer.draft(), "unfinished\nline");
        composer.update(key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(composer.draft(), "second");
        composer.update(key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(composer.draft(), "first");
        composer.update(key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(composer.draft(), "second");
        composer.update(key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(composer.draft(), "unfinished\nline");
    }

    #[test]
    fn editing_a_recalled_prompt_detaches_it_from_history() {
        let mut composer = new_composer();
        composer.replace_draft("previous".to_owned());
        composer.update(key(KeyCode::Enter, KeyModifiers::NONE));

        composer.update(key(KeyCode::Up, KeyModifiers::NONE));
        composer.update(key(KeyCode::Char('!'), KeyModifiers::NONE));
        composer.update(key(KeyCode::Down, KeyModifiers::NONE));

        assert_eq!(composer.draft(), "previous!");
    }

    #[test]
    fn multiline_and_control_effect_keys_are_distinct() {
        let mut composer = new_composer();
        composer.update(key(KeyCode::Enter, KeyModifiers::SHIFT));
        composer.update(key(KeyCode::Char('j'), KeyModifiers::CONTROL));
        assert_eq!(composer.draft(), "\n\n");

        assert_eq!(
            composer
                .update(key(KeyCode::Char('g'), KeyModifiers::CONTROL))
                .effect,
            Some(ComposerEffect::OpenDraftEditor)
        );
    }

    #[test]
    fn editing_keys_follow_visual_lines_and_grapheme_boundaries() {
        let mut composer = new_composer();
        composer.replace_draft("abc\ndef".to_owned());
        render(&mut composer, 20, 5);

        composer.update(key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(composer.cursor(), 3);
        composer.update(key(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(composer.cursor(), 0);
        composer.update(key(KeyCode::Delete, KeyModifiers::NONE));
        assert_eq!(composer.draft(), "bc\ndef");
        composer.update(key(KeyCode::End, KeyModifiers::NONE));
        composer.update(key(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(composer.draft(), "b\ndef");
    }

    #[test]
    fn wrapping_prefers_words_and_hard_wraps_long_words() {
        let mut composer = new_composer();
        composer.replace_draft("alpha betaabcdefgh".to_owned());
        let terminal = render(&mut composer, 8, 6);

        assert_eq!(
            rows(&terminal)[1..5],
            ["│alpha │", "│betaab│", "│cdefgh│", "│      │"]
        );
    }

    #[test]
    fn semantic_selection_preserves_source_across_soft_and_hard_wraps() {
        let mut composer = new_composer();
        composer.replace_draft("abcdef\ngh".to_owned());
        let area = Rect::new(10, 5, 3, 3);
        let anchor = composer.selection_span(Position::new(11, 5), area).unwrap();
        let head = composer.selection_span(Position::new(11, 7), area).unwrap();
        let mut selection = Selection::default();
        selection.begin(Surface::Composer, anchor);
        selection.drag(head);

        assert_eq!(
            composer
                .selection_text(selection.range().unwrap())
                .as_deref(),
            Some("bcdef\ngh")
        );
    }

    #[test]
    fn selection_scrolling_keeps_the_semantic_range_visible_without_cursor_follow() {
        let mut composer = new_composer();
        composer.replace_draft("line 1\nline 2\nline 3\nline 4".to_owned());
        render(&mut composer, 12, 4);
        assert_eq!(composer.scroll, 2);

        let content = Rect::new(1, 1, 10, 2);
        assert!(composer.scroll_selection(-1, content));
        let anchor = composer
            .selection_span(Position::new(1, 1), content)
            .unwrap();
        let head = composer
            .selection_span(Position::new(6, 2), content)
            .unwrap();
        let mut selection = Selection::default();
        selection.begin(Surface::Composer, anchor);
        selection.drag(head);
        let range = selection.range().unwrap();

        let terminal = render_with_selection(&mut composer, 12, 4, range);

        assert_eq!(composer.scroll, 1);
        assert_eq!(
            composer.selection_text(range).as_deref(),
            Some("line 2\nline 3")
        );
        assert_eq!(terminal.backend().buffer()[(1, 1)].bg, Color::Yellow);
        assert_eq!(terminal.backend().buffer()[(6, 2)].bg, Color::Yellow);
        assert_eq!(terminal.backend().cursor_position(), Position::new(0, 0));
    }

    #[test]
    fn narrow_selection_matches_the_visible_draft_text() {
        let mut composer = new_composer();
        composer.replace_draft("first\nsecond\nthird".to_owned());
        render(&mut composer, 12, 4);

        let terminal = render(&mut composer, 2, 2);
        let visible = terminal.backend().buffer()[(0, 0)].symbol().to_owned();
        let area = Rect::new(0, 0, 2, 1);
        let span = composer.selection_span(Position::new(0, 0), area).unwrap();
        let mut selection = Selection::default();
        selection.begin(Surface::Composer, span);
        selection.drag(span);

        assert_eq!(
            composer
                .selection_text(selection.range().unwrap())
                .as_deref(),
            Some(visible.as_str())
        );
    }

    #[test]
    fn context_percentage_is_rounded() {
        assert_eq!(context_percent(0, 272_000), 0);
        assert_eq!(context_percent(136_000, 272_000), 50);
        assert_eq!(context_percent(1_400, 272_000), 1);
    }

    #[test]
    fn narrow_rendering_truncates_without_panicking() {
        let mut composer = new_composer();
        composer.replace_draft("abcdef".to_owned());

        let terminal = render(&mut composer, 3, 2);

        assert_eq!(rows(&terminal)[0], "abc");
    }
}
