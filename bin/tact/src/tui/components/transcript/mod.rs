//! Scrollable rendering of the persisted agent session.

mod diff;
mod empty;
mod highlight;
mod markdown;
mod message;
mod tool;

use super::{
    node::{Component, ComponentUpdate, RenderRequest},
    selection::{TextRange, TextSpan},
};
use crate::{
    app::config::ReasoningEffort,
    tui::{
        format::{duration_display_tick, format_duration, format_turn_duration},
        spinner::Spinner,
        theme::Theme,
        transcript::{
            EntryId, EntryKind, TranscriptEntry, TranscriptModel, TranscriptRecord, TransientStatus,
        },
    },
};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use empty::EmptyLogo;
use ratatui::{
    Frame,
    buffer::Buffer,
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Widget},
};
use std::{
    collections::{HashMap, hash_map::Entry},
    ops::Range,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tact_subagents::{AgentMessageUpdate, MessageSender};

const EXPANDABLE_FOCUS_HINTS: [&str; 2] =
    ["↑↓ item · Enter toggle · Esc back", "↑↓ item · Enter · Esc"];
const NESTED_TOOL_INDENT: u16 = 4;
const PINNED_PROMPT_MAX_HEIGHT: u16 = 3;
const RETRY_COUNTDOWN_INTERVAL: Duration = Duration::from_millis(100);

pub(crate) enum TranscriptEvent {
    Record(Arc<TranscriptRecord>),
    DirectedMessage {
        perspective: MessageSender,
        update: AgentMessageUpdate,
    },
    AgentStreamClosed,
    Scroll(ScrollCommand),
    JumpToPinnedPrompt,
    FollowTail,
    BlurExpandables,
    Expandable(ExpandableCommand),
    ToggleExpandAll,
    AnimationFrame(Instant),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranscriptEffect {
    pub(crate) active: bool,
    pub(crate) status: Option<String>,
}

pub(crate) struct Transcript {
    model: TranscriptModel,
    cache: LayoutCache,
    scroll: ScrollState,
    pending_scroll: ScrollCommand,
    last_top: Option<Anchor>,
    viewport_height: u16,
    new_updates: u64,
    tool_spinner: Option<Spinner>,
    running_tool_timers: HashMap<EntryId, RunningToolTimer>,
    retry_timer: Option<RetryTimer>,
    expandables_focused: bool,
    selected_expandable: Option<EntryId>,
    expandable_hits: Vec<ExpandableHitRegion>,
    link_hits: Vec<LinkHitRegion>,
    selection_rows: Vec<(u16, Anchor)>,
    transcript_y: u16,
    transcript_x: u16,
    pending_expandable_anchor: Option<PendingExpandableAnchor>,
    empty_logo: EmptyLogo,
    effort: ReasoningEffort,
    pinned_prompt: Option<PinnedPrompt>,
    updates_banner_area: Option<Rect>,
}

struct CachedEntry {
    revision: u64,
    width: u16,
    expanded: bool,
    live_duration_ns: Option<u64>,
    tool_summary_lines: usize,
    lines: Vec<Line<'static>>,
    links: Vec<Vec<markdown::LinkSpan>>,
    selections: Vec<Vec<markdown::SourceSpan>>,
    envelopes: Vec<markdown::SourceEnvelope>,
    selection_source: Option<String>,
}

#[derive(Default)]
struct LayoutCache {
    entries: HashMap<EntryId, CachedEntry>,
    live_tool_durations: HashMap<EntryId, u64>,
    expansion_overrides: HashMap<EntryId, bool>,
    expand_all: Option<bool>,
}

#[derive(Default)]
struct RenderPlan {
    top_padding: u16,
    anchors: Vec<Anchor>,
}

#[derive(Clone, Copy)]
enum ScrollState {
    Follow,
    Detached(Anchor),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Anchor {
    entry: EntryId,
    line: usize,
}

#[derive(Clone, Copy)]
struct ExpandableHitRegion {
    entry: EntryId,
    row: u16,
}

struct LinkHitRegion {
    destination: Arc<str>,
    row: u16,
    start: u16,
    end: u16,
}

#[derive(Clone, Copy)]
enum PendingExpandableAnchor {
    Reveal(EntryId),
    Preserve { entry: EntryId, row: u16 },
}

#[derive(Clone, Copy)]
struct RunningToolTimer {
    observed_at: Instant,
    elapsed_at_observation: Duration,
}

#[derive(Clone, Copy)]
struct RetryTimer {
    deadline: Instant,
    remaining_ns: u64,
    next_frame: Option<Instant>,
}

#[derive(Clone, Copy)]
struct PinnedPrompt {
    entry: EntryId,
    area: Rect,
    offset: usize,
    max_offset: usize,
}

impl RunningToolTimer {
    fn new(started_at_unix_ms: u64, observed_at: Instant, observed_at_unix_ms: u64) -> Self {
        Self {
            observed_at,
            elapsed_at_observation: Duration::from_millis(
                observed_at_unix_ms.saturating_sub(started_at_unix_ms),
            ),
        }
    }

    fn elapsed(self, now: Instant) -> Duration {
        self.elapsed_at_observation
            .saturating_add(now.saturating_duration_since(self.observed_at))
    }
}

impl RetryTimer {
    fn new(now: Instant, delay_ns: u64) -> Self {
        let deadline = now + Duration::from_nanos(delay_ns);
        Self {
            deadline,
            remaining_ns: delay_ns,
            next_frame: Some((now + RETRY_COUNTDOWN_INTERVAL).min(deadline)),
        }
    }

    fn refresh(&mut self, now: Instant) -> bool {
        let previous_tick = duration_display_tick(self.remaining_ns);
        self.remaining_ns = u64::try_from(self.deadline.saturating_duration_since(now).as_nanos())
            .unwrap_or(u64::MAX);
        self.next_frame =
            (self.remaining_ns > 0).then(|| (now + RETRY_COUNTDOWN_INTERVAL).min(self.deadline));
        duration_display_tick(self.remaining_ns) != previous_tick
    }
}

#[derive(Clone, Copy)]
pub(super) enum ExpandableCommand {
    Previous,
    Next,
    Toggle,
    Click { row: u16 },
}

#[derive(Clone, Copy, Default)]
pub(super) enum ScrollCommand {
    #[default]
    None,
    Rows(i32),
    PinnedPromptRows(i32),
    Home,
    End,
}

impl Transcript {
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Self::with_effort(ReasoningEffort::default())
    }

    pub(crate) fn with_effort(effort: ReasoningEffort) -> Self {
        Self {
            model: TranscriptModel::default(),
            cache: LayoutCache::default(),
            scroll: ScrollState::Follow,
            pending_scroll: ScrollCommand::None,
            last_top: None,
            viewport_height: 0,
            new_updates: 0,
            tool_spinner: None,
            running_tool_timers: HashMap::new(),
            retry_timer: None,
            expandables_focused: false,
            selected_expandable: None,
            expandable_hits: Vec::new(),
            link_hits: Vec::new(),
            selection_rows: Vec::new(),
            transcript_y: 0,
            transcript_x: 0,
            pending_expandable_anchor: None,
            empty_logo: EmptyLogo::new(Instant::now()),
            effort,
            pinned_prompt: None,
            updates_banner_area: None,
        }
    }

    pub(crate) fn fork_snapshot(&self) -> Self {
        let mut snapshot = Self::with_effort(self.effort);
        snapshot.model = self.model.fork_snapshot();
        snapshot
    }

    pub(crate) const fn is_active(&self) -> bool {
        self.model.is_active()
    }

    pub(crate) const fn set_effort(&mut self, effort: ReasoningEffort) {
        self.effort = effort;
    }

    pub(super) fn render_chrome(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        self.updates_banner_area = None;
        let area = self.pinned_prompt.map_or(area, |prompt| Rect {
            y: prompt.area.bottom(),
            height: area.bottom().saturating_sub(prompt.area.bottom()),
            ..area
        });
        if self.expandables_focused {
            let _ = render_top_right_hint(frame, area, &EXPANDABLE_FOCUS_HINTS, theme.accent());
            return;
        }

        if !matches!(self.scroll, ScrollState::Detached(_)) || self.new_updates == 0 {
            return;
        }

        let noun = if self.new_updates == 1 {
            "update"
        } else {
            "updates"
        };
        let label = format!("↓ {} {noun} · Ctrl+End to follow", self.new_updates);
        let compact_label = format!("↓ {} {noun} · Ctrl+End", self.new_updates);
        self.updates_banner_area =
            render_top_right_hint(frame, area, &[&label, &compact_label], theme.border());
    }

    pub(crate) fn animation_deadline(&self) -> Option<Instant> {
        let empty = self.is_empty().then(|| self.empty_logo.deadline());
        self.tool_spinner
            .map(Spinner::deadline)
            .into_iter()
            .chain(empty)
            .chain(self.retry_timer.and_then(|timer| timer.next_frame))
            .min()
    }

    fn update_record(
        &mut self,
        record: Arc<TranscriptRecord>,
    ) -> ComponentUpdate<TranscriptEffect> {
        let previous_activity = self.activity();
        let change = self.model.apply(&record);
        let activity = self.activity();
        let now = Instant::now();
        if record.kind() == "model.attempt.retrying" {
            if let Some(TransientStatus::Retrying(delay_ns)) = self.model.transient() {
                self.retry_timer = Some(RetryTimer::new(now, *delay_ns));
            }
        } else if !matches!(self.model.transient(), Some(TransientStatus::Retrying(_))) {
            self.retry_timer = None;
        }
        self.sync_running_tool_timers(now);
        let tool_active = self.model.has_running_tools();
        if tool_active && self.tool_spinner.is_none() {
            self.tool_spinner = Some(Spinner::new(now));
        } else if !tool_active {
            self.tool_spinner = None;
        }
        if change.changed && matches!(self.scroll, ScrollState::Detached(_)) {
            self.new_updates = self.new_updates.saturating_add(1);
        }
        let effects = (previous_activity != activity)
            .then_some(activity)
            .into_iter()
            .collect();
        let render = if !change.changed {
            RenderRequest::None
        } else if record.source() == "tact" {
            RenderRequest::Immediate
        } else {
            RenderRequest::Streaming
        };
        ComponentUpdate { effects, render }
    }

    fn update_message(
        &mut self,
        perspective: MessageSender,
        update: AgentMessageUpdate,
    ) -> ComponentUpdate<TranscriptEffect> {
        let change = self.model.apply_message(perspective, update);
        if let Some(id) = change.removed {
            self.forget_entry(id);
        }
        if !change.changed {
            return ComponentUpdate::none();
        }
        if matches!(self.scroll, ScrollState::Detached(_)) {
            self.new_updates = self.new_updates.saturating_add(1);
        }
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn forget_entry(&mut self, id: EntryId) {
        self.cache.forget(id);
        self.running_tool_timers.remove(&id);
        self.expandable_hits.retain(|hit| hit.entry != id);
        if self.selected_expandable == Some(id) {
            self.selected_expandable = None;
        }
        if self.last_top.is_some_and(|anchor| anchor.entry == id) {
            self.last_top = None;
        }
        if matches!(self.scroll, ScrollState::Detached(anchor) if anchor.entry == id) {
            self.scroll = ScrollState::Follow;
        }
        if matches!(
            self.pending_expandable_anchor,
            Some(PendingExpandableAnchor::Reveal(entry)) if entry == id
        ) || matches!(
            self.pending_expandable_anchor,
            Some(PendingExpandableAnchor::Preserve { entry, .. }) if entry == id
        ) {
            self.pending_expandable_anchor = None;
        }
    }

    fn agent_stream_closed(&mut self) -> ComponentUpdate<TranscriptEffect> {
        let previous_activity = self.activity();
        if !self.model.agent_stream_closed() {
            return ComponentUpdate::none();
        }
        let now = Instant::now();
        self.sync_running_tool_timers(now);
        self.tool_spinner = self.model.has_running_tools().then(|| Spinner::new(now));
        let activity = self.activity();
        ComponentUpdate {
            effects: (previous_activity != activity)
                .then_some(activity)
                .into_iter()
                .collect(),
            render: RenderRequest::Immediate,
        }
    }

    fn activity(&self) -> TranscriptEffect {
        TranscriptEffect {
            active: self.model.is_active(),
            status: self.model.transient().map(|status| match status {
                TransientStatus::Retrying(delay_ns) => {
                    let remaining_ns = self
                        .retry_timer
                        .map_or(*delay_ns, |timer| timer.remaining_ns);
                    format!("Retrying in {}…", format_duration(remaining_ns))
                }
                status => transient_label(status),
            }),
        }
    }

    fn update_animation(&mut self, now: Instant) -> ComponentUpdate<TranscriptEffect> {
        let previous_activity = self.activity();
        let retry_changed = self
            .retry_timer
            .as_mut()
            .is_some_and(|timer| timer.refresh(now));
        let timer_changed = self.refresh_running_tool_durations(now);
        let tool_changed = self
            .tool_spinner
            .as_mut()
            .is_some_and(|spinner| spinner.advance(now));
        let logo_changed = self.is_empty() && self.empty_logo.advance(now);
        let activity = self.activity();
        ComponentUpdate {
            effects: (previous_activity != activity)
                .then_some(activity)
                .into_iter()
                .collect(),
            render: if retry_changed || timer_changed || tool_changed || logo_changed {
                RenderRequest::Streaming
            } else {
                RenderRequest::None
            },
        }
    }

    fn sync_running_tool_timers(&mut self, now: Instant) {
        self.running_tool_timers
            .retain(|id, _| self.model.entry(*id).is_some_and(is_running_tool));
        let observed_at_unix_ms = unix_milliseconds();
        for id in self.model.running_tool_ids() {
            let Some(started_at_unix_ms) =
                self.model.entry(id).and_then(|entry| match &entry.kind {
                    EntryKind::Tool(tool) => Some(tool.started_at_unix_ms),
                    _ => None,
                })
            else {
                continue;
            };
            self.running_tool_timers.entry(id).or_insert_with(|| {
                RunningToolTimer::new(started_at_unix_ms, now, observed_at_unix_ms)
            });
        }
        self.refresh_running_tool_durations(now);
    }

    fn refresh_running_tool_durations(&mut self, now: Instant) -> bool {
        let mut changed = false;
        for (&id, &timer) in &self.running_tool_timers {
            let elapsed = timer.elapsed(now);
            let duration_ns = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
            changed |= self.cache.set_live_tool_duration(id, duration_ns);
        }
        self.cache
            .retain_live_tool_durations(|id| self.running_tool_timers.contains_key(&id));
        changed
    }

    fn is_empty(&self) -> bool {
        self.model.entries().iter().all(|entry| entry.hidden)
    }

    pub(super) fn scroll_command(&self, event: &Event) -> Option<ScrollCommand> {
        let command = match event {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                match (key.code, key.modifiers) {
                    (KeyCode::PageUp, _) => ScrollCommand::Rows(-self.page_size()),
                    (KeyCode::PageDown, _) => ScrollCommand::Rows(self.page_size()),
                    (KeyCode::Home, modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                        ScrollCommand::Home
                    }
                    (KeyCode::End, modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                        ScrollCommand::End
                    }
                    _ => return None,
                }
            }
            Event::Mouse(mouse) if !mouse.modifiers.contains(KeyModifiers::SHIFT) => {
                if let Some(prompt) = self.pinned_prompt
                    && prompt.area.contains(Position::new(mouse.column, mouse.row))
                {
                    match mouse.kind {
                        MouseEventKind::ScrollUp if prompt.offset > 0 => {
                            return Some(ScrollCommand::PinnedPromptRows(-1));
                        }
                        MouseEventKind::ScrollDown if prompt.offset < prompt.max_offset => {
                            return Some(ScrollCommand::PinnedPromptRows(1));
                        }
                        _ => {}
                    }
                }
                match mouse.kind {
                    MouseEventKind::ScrollUp => ScrollCommand::Rows(-3),
                    MouseEventKind::ScrollDown => ScrollCommand::Rows(3),
                    _ => return None,
                }
            }
            _ => return None,
        };
        Some(command)
    }

    pub(super) fn pinned_prompt_clicked(&self, event: &Event) -> bool {
        let Event::Mouse(mouse) = event else {
            return false;
        };
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return false;
        }
        self.pinned_prompt
            .is_some_and(|prompt| prompt.area.contains(Position::new(mouse.column, mouse.row)))
    }

    pub(super) fn updates_banner_clicked(&self, event: &Event) -> bool {
        let Event::Mouse(mouse) = event else {
            return false;
        };
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return false;
        }
        self.updates_banner_area
            .is_some_and(|area| area.contains(Position::new(mouse.column, mouse.row)))
    }

    pub(super) fn expandable_command(&self, event: &Event) -> Option<ExpandableCommand> {
        match event {
            Event::Mouse(mouse) if mouse.kind == MouseEventKind::Down(MouseButton::Left) => self
                .expandable_hits
                .iter()
                .any(|hit| hit.row == mouse.row)
                .then_some(ExpandableCommand::Click { row: mouse.row }),
            Event::Key(key)
                if self.expandables_focused
                    && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
            {
                match key.code {
                    KeyCode::Up => Some(ExpandableCommand::Previous),
                    KeyCode::Down => Some(ExpandableCommand::Next),
                    KeyCode::Enter => Some(ExpandableCommand::Toggle),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    pub(super) fn link_destination(&self, event: &Event) -> Option<Arc<str>> {
        let Event::Mouse(mouse) = event else {
            return None;
        };
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return None;
        }
        self.link_hits
            .iter()
            .find(|hit| hit.row == mouse.row && (hit.start..hit.end).contains(&mouse.column))
            .map(|hit| Arc::clone(&hit.destination))
    }

    pub(super) fn selection_span(&self, position: Position) -> Option<TextSpan> {
        self.selection_span_with_fallback(position, false)
    }

    pub(super) fn selection_span_nearest(&self, position: Position) -> Option<TextSpan> {
        self.selection_span_with_fallback(position, true)
    }

    fn selection_span_with_fallback(
        &self,
        position: Position,
        across_entries: bool,
    ) -> Option<TextSpan> {
        let exact = self
            .selection_rows
            .iter()
            .find(|(row, _)| *row == position.y)
            .map(|(_, anchor)| *anchor);
        let exact = match exact {
            Some(exact) => exact,
            None if across_entries => self
                .selection_rows
                .iter()
                .min_by_key(|(row, _)| row.abs_diff(position.y))
                .map(|(_, anchor)| *anchor)?,
            None => return None,
        };
        let anchor = if self.cache.selections(exact).is_empty() {
            if !across_entries {
                let entry = self.model.entry(exact.entry)?;
                if matches!(entry.kind, EntryKind::Tool(_)) {
                    return None;
                }
                self.cache.selection_source(entry)?;
            }
            self.selection_rows
                .iter()
                .filter(|(_, anchor)| {
                    (across_entries || anchor.entry == exact.entry)
                        && !self.cache.selections(*anchor).is_empty()
                })
                .min_by_key(|(row, _)| row.abs_diff(position.y))
                .map(|(_, anchor)| *anchor)?
        } else {
            exact
        };
        let spans = self.cache.selections(anchor);
        let column = position.x.saturating_sub(self.transcript_x);
        let source = spans
            .iter()
            .find(|span| span.columns.contains(&column))
            .or_else(|| {
                spans.iter().min_by_key(|span| {
                    if column < span.columns.start {
                        span.columns.start - column
                    } else {
                        column.saturating_sub(span.columns.end.saturating_sub(1))
                    }
                })
            })?;
        Some(TextSpan::new(
            anchor.entry.index(),
            source.source.start,
            source.source.end,
        ))
    }

    pub(super) fn selection_text(&self, range: TextRange) -> Option<String> {
        let (start, end) = range.bounds();
        let mut fragments = Vec::new();
        for entry in self.model.entries() {
            let block = entry.id.index();
            if block < start.block || block > end.block {
                continue;
            }
            let Some(source) = self.cache.selection_source(entry) else {
                continue;
            };
            let Some(selected) = range.source_range(block, source.len()) else {
                continue;
            };
            let selected = self.cache.expand_selection(entry.id, selected);
            if let Some(fragment) = source.get(selected)
                && !fragment.is_empty()
            {
                fragments.push(fragment);
            }
        }
        (!fragments.is_empty()).then(|| fragments.join("\n\n"))
    }

    pub(super) fn render_selection(&self, buffer: &mut Buffer, range: TextRange) {
        let selected = Style::reset().fg(Color::Black).bg(Color::Yellow);
        for (row, anchor) in &self.selection_rows {
            for span in self.cache.selections(*anchor) {
                if !range.includes(anchor.entry.index(), &span.source) {
                    continue;
                }
                for column in span.columns.clone() {
                    let column = self.transcript_x.saturating_add(column);
                    if let Some(cell) = buffer.cell_mut(Position::new(column, *row)) {
                        cell.set_style(selected);
                    }
                }
            }
        }
    }

    pub(super) const fn expandables_focused(&self) -> bool {
        self.expandables_focused
    }

    fn update_scroll(&mut self, command: ScrollCommand) -> ComponentUpdate<TranscriptEffect> {
        if let ScrollCommand::PinnedPromptRows(rows) = command {
            let Some(prompt) = &mut self.pinned_prompt else {
                return ComponentUpdate::none();
            };
            let offset = i64::try_from(prompt.offset).unwrap_or(i64::MAX);
            let max_offset = i64::try_from(prompt.max_offset).unwrap_or(i64::MAX);
            prompt.offset = usize::try_from((offset + i64::from(rows)).clamp(0, max_offset))
                .unwrap_or(prompt.max_offset);
            return ComponentUpdate::render(RenderRequest::Immediate);
        }
        self.pending_scroll = command;
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn jump_to_pinned_prompt(&mut self) -> ComponentUpdate<TranscriptEffect> {
        let Some(prompt) = self.pinned_prompt.take() else {
            return ComponentUpdate::none();
        };
        self.scroll = ScrollState::Detached(Anchor {
            entry: prompt.entry,
            line: 0,
        });
        self.pending_scroll = ScrollCommand::None;
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn follow_tail(&mut self) -> ComponentUpdate<TranscriptEffect> {
        let was_detached = matches!(self.scroll, ScrollState::Detached(_));
        self.scroll = ScrollState::Follow;
        self.pending_scroll = ScrollCommand::None;
        self.new_updates = 0;

        if was_detached {
            ComponentUpdate::render(RenderRequest::Immediate)
        } else {
            ComponentUpdate::none()
        }
    }

    fn blur_expandables(&mut self) -> ComponentUpdate<TranscriptEffect> {
        if !self.expandables_focused {
            return ComponentUpdate::none();
        }
        self.expandables_focused = false;
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    #[cfg(test)]
    pub(super) fn focus_expandables(&mut self) -> ComponentUpdate<TranscriptEffect> {
        self.expandables_focused = true;
        if self.selected_expandable.is_none() {
            self.selected_expandable = self.expandable_hits.last().map(|hit| hit.entry);
        }
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn update_expandable(
        &mut self,
        command: ExpandableCommand,
    ) -> ComponentUpdate<TranscriptEffect> {
        match command {
            ExpandableCommand::Previous => self.select_expandable(-1),
            ExpandableCommand::Next => self.select_expandable(1),
            ExpandableCommand::Toggle => self.toggle_selected_expandable(),
            ExpandableCommand::Click { row } => {
                let Some(entry) = self
                    .expandable_hits
                    .iter()
                    .find(|hit| hit.row == row)
                    .map(|hit| hit.entry)
                else {
                    return ComponentUpdate::none();
                };
                self.expandables_focused = true;
                self.selected_expandable = Some(entry);
                self.toggle_selected_expandable()
            }
        }
    }

    fn select_expandable(&mut self, direction: i32) -> ComponentUpdate<TranscriptEffect> {
        let entries = self.model.entries();
        let selected = self
            .selected_expandable
            .and_then(|selected| self.model.index_of(selected));
        let next = if direction < 0 {
            let end = selected.unwrap_or(entries.len());
            entries[..end]
                .iter()
                .rev()
                .find(|entry| !entry.hidden && is_expandable(entry))
        } else if let Some(selected) = selected {
            entries[selected.saturating_add(1)..]
                .iter()
                .find(|entry| !entry.hidden && is_expandable(entry))
        } else {
            entries
                .iter()
                .rev()
                .find(|entry| !entry.hidden && is_expandable(entry))
        };
        let Some(selected) = next.map(|entry| entry.id) else {
            return ComponentUpdate::none();
        };
        self.selected_expandable = Some(selected);
        if !self.expandable_hits.iter().any(|hit| hit.entry == selected) {
            self.pending_expandable_anchor = Some(PendingExpandableAnchor::Reveal(selected));
        }
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn toggle_selected_expandable(&mut self) -> ComponentUpdate<TranscriptEffect> {
        let Some(entry_id) = self.selected_expandable else {
            return ComponentUpdate::none();
        };
        let Some(entry_index) = self.model.index_of(entry_id) else {
            return ComponentUpdate::none();
        };
        let row = self
            .expandable_hits
            .iter()
            .find(|hit| hit.entry == entry_id)
            .map_or(0, |hit| hit.row.saturating_sub(self.transcript_y));
        self.cache.toggle(&self.model.entries()[entry_index]);
        self.pending_expandable_anchor = Some(PendingExpandableAnchor::Preserve {
            entry: entry_id,
            row,
        });
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn page_size(&self) -> i32 {
        i32::from(self.viewport_height.saturating_sub(2).max(1))
    }

    fn render_plan(&mut self, width: u16, height: u16, theme: &Theme) -> RenderPlan {
        if width == 0 || height == 0 {
            return RenderPlan::default();
        }
        self.apply_pending_expandable_anchor(width, theme);
        self.apply_pending_scroll(width, height, theme);
        let top = match self.scroll {
            ScrollState::Follow => self.tail_top(width, height, theme),
            ScrollState::Detached(anchor) => {
                let top = self
                    .resolve_anchor(anchor, width, theme)
                    .map(|top| self.fill_viewport_from(top, height, width, theme));
                if let Some(top) = top {
                    self.scroll = ScrollState::Detached(top);
                }
                top
            }
        };
        self.last_top = top;

        let anchors = top.map_or_else(Vec::new, |anchor| {
            self.collect_forward_anchors(anchor, usize::from(height), width, theme)
        });
        if matches!(self.scroll, ScrollState::Detached(_))
            && anchors.last().copied() == self.last_anchor(width, theme)
        {
            self.scroll = ScrollState::Follow;
            self.new_updates = 0;
        }
        let occupied = anchors.len();
        let top_padding = height.saturating_sub(u16::try_from(occupied).unwrap_or(u16::MAX));
        self.warm_overscan(top, height, width, theme);
        RenderPlan {
            top_padding,
            anchors,
        }
    }

    fn pinned_prompt_entry(&self, top: Option<Anchor>) -> Option<EntryId> {
        if !matches!(self.scroll, ScrollState::Detached(_)) {
            return None;
        }
        let top = top?;
        let top_index = self.model.index_of(top.entry)?;
        if matches!(self.model.entries()[top_index].kind, EntryKind::User { .. }) {
            return None;
        }
        self.model.entries()[..top_index]
            .iter()
            .rev()
            .find(|entry| !entry.hidden && matches!(entry.kind, EntryKind::User { .. }))
            .map(|entry| entry.id)
    }

    fn prepare_pinned_prompt(
        &mut self,
        entry_id: Option<EntryId>,
        area: Rect,
        theme: &Theme,
    ) -> u16 {
        let Some(entry_id) = entry_id else {
            self.pinned_prompt = None;
            return 0;
        };
        let Some(entry) = self.model.entry(entry_id).cloned() else {
            self.pinned_prompt = None;
            return 0;
        };
        let available_height = area.height.saturating_sub(1).min(PINNED_PROMPT_MAX_HEIGHT);
        let mut line_count = self.cache.layout(&entry, area.width, theme).len();
        if entry.trailing_spacer {
            line_count = line_count.saturating_sub(1);
        }
        let height = available_height.min(u16::try_from(line_count).unwrap_or(u16::MAX));
        if height == 0 {
            self.pinned_prompt = None;
            return 0;
        }

        let max_offset = line_count.saturating_sub(usize::from(height));
        let offset = self
            .pinned_prompt
            .filter(|prompt| prompt.entry == entry_id)
            .map_or(0, |prompt| prompt.offset.min(max_offset));
        self.pinned_prompt = Some(PinnedPrompt {
            entry: entry_id,
            area: Rect { height, ..area },
            offset,
            max_offset,
        });
        height
    }

    fn render_pinned_prompt(&mut self, frame: &mut Frame<'_>, theme: &Theme) {
        let Some(prompt) = self.pinned_prompt else {
            return;
        };
        for row in 0..prompt.area.height {
            let line = prompt.offset.saturating_add(usize::from(row));
            let anchor = Anchor {
                entry: prompt.entry,
                line,
            };
            if let Some(content) = self.cache.line(anchor) {
                frame.buffer_mut().set_line(
                    prompt.area.x,
                    prompt.area.y.saturating_add(row),
                    content,
                    prompt.area.width,
                );
            }
            self.selection_rows
                .push((prompt.area.y.saturating_add(row), anchor));
        }
        frame
            .buffer_mut()
            .set_style(prompt.area, Style::default().bg(theme.code_background()));

        let marker_style = Style::default()
            .fg(theme.muted())
            .add_modifier(Modifier::BOLD);
        let marker_x = prompt.area.right().saturating_sub(1);
        if prompt.offset > 0
            && let Some(cell) = frame
                .buffer_mut()
                .cell_mut(Position::new(marker_x, prompt.area.y))
        {
            cell.set_symbol("…").set_style(marker_style);
        }
        if prompt.offset < prompt.max_offset
            && let Some(cell) = frame.buffer_mut().cell_mut(Position::new(
                marker_x,
                prompt.area.bottom().saturating_sub(1),
            ))
        {
            cell.set_symbol("…").set_style(marker_style);
        }
    }

    fn apply_pending_expandable_anchor(&mut self, width: u16, theme: &Theme) {
        let Some(request) = self.pending_expandable_anchor.take() else {
            return;
        };
        let (entry, row) = match request {
            PendingExpandableAnchor::Reveal(entry) => (entry, 0),
            PendingExpandableAnchor::Preserve { entry, row } => (entry, row),
        };
        let anchor = Anchor { entry, line: 0 };
        let (top, _) = self.move_anchor(anchor, -i32::from(row), width, theme);
        self.scroll = ScrollState::Detached(top);
    }

    fn apply_pending_scroll(&mut self, width: u16, height: u16, theme: &Theme) {
        let command = std::mem::take(&mut self.pending_scroll);
        match command {
            ScrollCommand::None => {}
            ScrollCommand::PinnedPromptRows(_) => {}
            ScrollCommand::End => {
                self.scroll = ScrollState::Follow;
                self.new_updates = 0;
            }
            ScrollCommand::Home => {
                if let Some(anchor) = self.first_anchor(width, theme) {
                    self.scroll = ScrollState::Detached(anchor);
                }
            }
            ScrollCommand::Rows(rows) if rows < 0 => {
                let start = match self.scroll {
                    ScrollState::Follow => self
                        .last_top
                        .or_else(|| self.tail_top(width, height, theme)),
                    ScrollState::Detached(anchor) => Some(anchor),
                };
                if let Some(start) = start {
                    let anchor = self.move_anchor(start, rows, width, theme).0;
                    self.scroll = ScrollState::Detached(anchor);
                }
            }
            ScrollCommand::Rows(rows) => {
                let ScrollState::Detached(start) = self.scroll else {
                    return;
                };
                let (anchor, reached_end) = self.move_anchor(start, rows, width, theme);
                if reached_end {
                    self.scroll = ScrollState::Follow;
                    self.new_updates = 0;
                } else {
                    self.scroll = ScrollState::Detached(anchor);
                }
            }
        }
    }

    fn tail_top(&mut self, width: u16, height: u16, theme: &Theme) -> Option<Anchor> {
        let mut anchor = self.last_anchor(width, theme)?;
        for _ in 1..height {
            let Some(previous) = self.previous(anchor, width, theme) else {
                break;
            };
            anchor = previous;
        }
        Some(anchor)
    }

    fn fill_viewport_from(
        &mut self,
        anchor: Anchor,
        height: u16,
        width: u16,
        theme: &Theme,
    ) -> Anchor {
        let mut last = anchor;
        let mut available = 1_u16;
        while available < height {
            let Some(next) = self.next(last, width, theme) else {
                break;
            };
            last = next;
            available = available.saturating_add(1);
        }

        let mut top = anchor;
        for _ in available..height {
            let Some(previous) = self.previous(top, width, theme) else {
                break;
            };
            top = previous;
        }
        top
    }

    fn first_anchor(&mut self, width: u16, theme: &Theme) -> Option<Anchor> {
        for index in 0..self.model.entries().len() {
            let entry = &self.model.entries()[index];
            if entry.hidden || self.cache.layout(entry, width, theme).is_empty() {
                continue;
            }
            return Some(Anchor {
                entry: entry.id,
                line: 0,
            });
        }
        None
    }

    fn last_anchor(&mut self, width: u16, theme: &Theme) -> Option<Anchor> {
        for index in (0..self.model.entries().len()).rev() {
            let entry = &self.model.entries()[index];
            if entry.hidden {
                continue;
            }
            let len = self.cache.layout(entry, width, theme).len();
            if len == 0 {
                continue;
            }
            return Some(Anchor {
                entry: entry.id,
                line: len - 1,
            });
        }
        None
    }

    fn resolve_anchor(&mut self, anchor: Anchor, width: u16, theme: &Theme) -> Option<Anchor> {
        let entry = self.model.entry(anchor.entry)?;
        if entry.hidden {
            return self.next_visible_entry(anchor.entry, width, theme);
        }
        let len = self.cache.layout(entry, width, theme).len();
        (len > 0).then_some(Anchor {
            entry: anchor.entry,
            line: anchor.line.min(len - 1),
        })
    }

    fn move_anchor(
        &mut self,
        mut anchor: Anchor,
        rows: i32,
        width: u16,
        theme: &Theme,
    ) -> (Anchor, bool) {
        if rows < 0 {
            for _ in 0..rows.unsigned_abs() {
                let Some(previous) = self.previous(anchor, width, theme) else {
                    return (anchor, false);
                };
                anchor = previous;
            }
            return (anchor, false);
        }
        for _ in 0..u32::try_from(rows).unwrap_or_default() {
            let Some(next) = self.next(anchor, width, theme) else {
                return (anchor, true);
            };
            anchor = next;
        }
        (anchor, false)
    }

    fn previous(&mut self, anchor: Anchor, width: u16, theme: &Theme) -> Option<Anchor> {
        if anchor.line > 0 {
            return Some(Anchor {
                line: anchor.line - 1,
                ..anchor
            });
        }
        let index = self.model.index_of(anchor.entry)?;
        for previous in (0..index).rev() {
            let entry = &self.model.entries()[previous];
            if entry.hidden {
                continue;
            }
            let len = self.cache.layout(entry, width, theme).len();
            if len > 0 {
                return Some(Anchor {
                    entry: entry.id,
                    line: len - 1,
                });
            }
        }
        None
    }

    fn next(&mut self, anchor: Anchor, width: u16, theme: &Theme) -> Option<Anchor> {
        let entry = self.model.entry(anchor.entry)?;
        let len = self.cache.layout(entry, width, theme).len();
        if anchor.line + 1 < len {
            return Some(Anchor {
                line: anchor.line + 1,
                ..anchor
            });
        }
        self.next_visible_entry(anchor.entry, width, theme)
    }

    fn next_visible_entry(
        &mut self,
        entry_id: EntryId,
        width: u16,
        theme: &Theme,
    ) -> Option<Anchor> {
        let index = self.model.index_of(entry_id)?;
        for next in index + 1..self.model.entries().len() {
            let entry = &self.model.entries()[next];
            if entry.hidden || self.cache.layout(entry, width, theme).is_empty() {
                continue;
            }
            return Some(Anchor {
                entry: entry.id,
                line: 0,
            });
        }
        None
    }

    fn collect_forward_anchors(
        &mut self,
        mut anchor: Anchor,
        height: usize,
        width: u16,
        theme: &Theme,
    ) -> Vec<Anchor> {
        let mut anchors = Vec::with_capacity(height);
        while anchors.len() < height {
            let Some(entry) = self.model.entry(anchor.entry) else {
                break;
            };
            let layout = self.cache.layout(entry, width, theme);
            if layout.get(anchor.line).is_some() {
                anchors.push(anchor);
            }
            let Some(next) = self.next(anchor, width, theme) else {
                break;
            };
            anchor = next;
        }
        anchors
    }

    fn warm_overscan(&mut self, top: Option<Anchor>, height: u16, width: u16, theme: &Theme) {
        let Some(top) = top else {
            return;
        };
        let _ = self.move_anchor(top, -i32::from(height), width, theme);
        let _ = self.move_anchor(top, i32::from(height.saturating_mul(2)), width, theme);
    }
}

fn unix_milliseconds() -> u64 {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
}

fn transient_label(status: &TransientStatus) -> String {
    match status {
        TransientStatus::Thinking => "Thinking…".to_owned(),
        TransientStatus::Responding => "Responding…".to_owned(),
        TransientStatus::Warming => "Warming model…".to_owned(),
        TransientStatus::WaitingForBackgroundWork => "Waiting for background work…".to_owned(),
        TransientStatus::Tool(tool) => format!("Running {tool}…"),
        TransientStatus::Compacting => "Compacting context…".to_owned(),
        TransientStatus::Retrying(delay_ns) => {
            format!("Retrying in {}…", format_duration(*delay_ns))
        }
        TransientStatus::Connecting => "Connecting…".to_owned(),
        TransientStatus::Reconnecting => "Reconnecting…".to_owned(),
        TransientStatus::Error(error) => error.clone(),
    }
}

fn is_running_tool(entry: &TranscriptEntry) -> bool {
    matches!(
        &entry.kind,
        EntryKind::Tool(tool) if tool.state == crate::tui::transcript::ToolState::Running
    )
}

fn is_expandable(entry: &TranscriptEntry) -> bool {
    matches!(
        entry.kind,
        EntryKind::Tool(_) | EntryKind::DirectedMessage(_)
    )
}

impl LayoutCache {
    fn forget(&mut self, id: EntryId) {
        self.entries.remove(&id);
        self.live_tool_durations.remove(&id);
        self.expansion_overrides.remove(&id);
    }

    fn layout(&mut self, entry: &TranscriptEntry, width: u16, theme: &Theme) -> &[Line<'static>] {
        let expanded = self
            .expansion_overrides
            .get(&entry.id)
            .copied()
            .or(self.expand_all)
            .unwrap_or_else(|| Self::expanded_by_default(entry));
        let live_duration_ns = self.live_tool_durations.get(&entry.id).copied();
        let cached = match self.entries.entry(entry.id) {
            Entry::Occupied(mut occupied) => {
                let cached = occupied.get();
                if cached.revision != entry.revision
                    || cached.width != width
                    || cached.expanded != expanded
                {
                    occupied.insert(CachedEntry::new(
                        entry,
                        live_duration_ns,
                        width,
                        theme,
                        expanded,
                    ));
                } else if cached.live_duration_ns != live_duration_ns {
                    occupied
                        .get_mut()
                        .update_live_duration(entry, live_duration_ns, theme);
                }
                occupied.into_mut()
            }
            Entry::Vacant(vacant) => vacant.insert(CachedEntry::new(
                entry,
                live_duration_ns,
                width,
                theme,
                expanded,
            )),
        };
        &cached.lines
    }

    fn set_live_tool_duration(&mut self, id: EntryId, duration_ns: u64) -> bool {
        let display_changed = self.live_tool_durations.get(&id).is_none_or(|previous| {
            duration_display_tick(*previous) != duration_display_tick(duration_ns)
        });
        if display_changed {
            self.live_tool_durations.insert(id, duration_ns);
        }
        display_changed
    }

    fn retain_live_tool_durations(&mut self, mut retain: impl FnMut(EntryId) -> bool) {
        self.live_tool_durations.retain(|id, _| retain(*id));
    }

    fn toggle(&mut self, entry: &TranscriptEntry) {
        let expanded = self
            .expansion_overrides
            .get(&entry.id)
            .copied()
            .or(self.expand_all)
            .unwrap_or_else(|| Self::expanded_by_default(entry));
        self.expansion_overrides.insert(entry.id, !expanded);
        self.entries.remove(&entry.id);
    }

    fn toggle_all(&mut self) {
        self.expand_all = Some(!matches!(self.expand_all, Some(true)));
        self.expansion_overrides.clear();
        self.entries.clear();
    }

    fn expanded_by_default(entry: &TranscriptEntry) -> bool {
        matches!(&entry.kind, EntryKind::Tool(tool) if tool.name == "update_plan")
    }

    fn line(&self, anchor: Anchor) -> Option<&Line<'static>> {
        self.entries
            .get(&anchor.entry)
            .and_then(|cached| cached.lines.get(anchor.line))
    }

    fn links(&self, anchor: Anchor) -> &[markdown::LinkSpan] {
        self.entries
            .get(&anchor.entry)
            .and_then(|cached| cached.links.get(anchor.line))
            .map_or(&[], Vec::as_slice)
    }

    fn selections(&self, anchor: Anchor) -> &[markdown::SourceSpan] {
        self.entries
            .get(&anchor.entry)
            .and_then(|cached| cached.selections.get(anchor.line))
            .map_or(&[], Vec::as_slice)
    }

    fn selection_source<'a>(&'a self, entry: &'a TranscriptEntry) -> Option<&'a str> {
        self.entries
            .get(&entry.id)
            .and_then(|cached| cached.selection_source.as_deref())
            .or_else(|| entry_selection_source(entry))
    }

    fn expand_selection(&self, entry: EntryId, mut selected: Range<usize>) -> Range<usize> {
        let Some(cached) = self.entries.get(&entry) else {
            return selected;
        };
        loop {
            let previous = selected.clone();
            for envelope in &cached.envelopes {
                if selected.start <= envelope.content.start && selected.end >= envelope.content.end
                {
                    selected.start = selected.start.min(envelope.source.start);
                    selected.end = selected.end.max(envelope.source.end);
                }
            }
            if selected == previous {
                return selected;
            }
        }
    }
}

impl CachedEntry {
    fn new(
        entry: &TranscriptEntry,
        live_duration_ns: Option<u64>,
        width: u16,
        theme: &Theme,
        expanded: bool,
    ) -> Self {
        let layout = render_entry(entry, live_duration_ns, width, theme, expanded);
        let tool_summary_lines = match (&entry.kind, live_duration_ns) {
            (EntryKind::Tool(tool), Some(duration_ns)) => {
                render_live_tool_summary(entry, tool, duration_ns, width, theme, expanded).len()
            }
            _ => 0,
        };
        Self {
            revision: entry.revision,
            width,
            expanded,
            live_duration_ns,
            tool_summary_lines,
            lines: layout.lines,
            links: layout.links,
            selections: layout.selections,
            envelopes: layout.envelopes,
            selection_source: layout.selection_source,
        }
    }

    fn update_live_duration(
        &mut self,
        entry: &TranscriptEntry,
        live_duration_ns: Option<u64>,
        theme: &Theme,
    ) {
        let (EntryKind::Tool(tool), Some(duration_ns)) = (&entry.kind, live_duration_ns) else {
            return;
        };
        let summary =
            render_live_tool_summary(entry, tool, duration_ns, self.width, theme, self.expanded);
        let summary_len = summary.len();
        self.lines.splice(0..self.tool_summary_lines, summary);
        self.links.splice(
            0..self.tool_summary_lines,
            std::iter::repeat_with(Vec::new).take(summary_len),
        );
        self.selections.splice(
            0..self.tool_summary_lines,
            std::iter::repeat_with(Vec::new).take(summary_len),
        );
        self.live_duration_ns = live_duration_ns;
        self.tool_summary_lines = summary_len;
    }
}

impl Component for Transcript {
    type Event = TranscriptEvent;
    type Effect = TranscriptEffect;

    fn update(&mut self, event: Self::Event) -> ComponentUpdate<Self::Effect> {
        match event {
            TranscriptEvent::Record(record) => self.update_record(record),
            TranscriptEvent::DirectedMessage {
                perspective,
                update,
            } => self.update_message(perspective, update),
            TranscriptEvent::AgentStreamClosed => self.agent_stream_closed(),
            TranscriptEvent::Scroll(command) => self.update_scroll(command),
            TranscriptEvent::JumpToPinnedPrompt => self.jump_to_pinned_prompt(),
            TranscriptEvent::FollowTail => self.follow_tail(),
            TranscriptEvent::BlurExpandables => self.blur_expandables(),
            TranscriptEvent::Expandable(command) => self.update_expandable(command),
            TranscriptEvent::ToggleExpandAll => {
                self.cache.toggle_all();
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            TranscriptEvent::AnimationFrame(now) => self.update_animation(now),
        }
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        self.viewport_height = area.height;
        self.transcript_y = area.y;
        self.transcript_x = area.x;
        self.expandable_hits.clear();
        self.link_hits.clear();
        self.selection_rows.clear();
        Clear.render(area, frame.buffer_mut());
        if self.is_empty() {
            self.empty_logo.render(frame, area, theme, self.effort);
            return;
        }
        let mut plan = self.render_plan(area.width, area.height, theme);
        let prompt_entry = self.pinned_prompt_entry(plan.anchors.first().copied());
        let prompt_height = self.prepare_pinned_prompt(prompt_entry, area, theme);
        let transcript_area = Rect {
            y: area.y.saturating_add(prompt_height),
            height: area.height.saturating_sub(prompt_height),
            ..area
        };
        self.viewport_height = transcript_area.height;
        if prompt_height > 0 {
            plan = self.render_plan(transcript_area.width, transcript_area.height, theme);
            self.render_pinned_prompt(frame, theme);
        }
        let RenderPlan {
            top_padding,
            anchors,
        } = plan;
        let mut y = transcript_area.y.saturating_add(top_padding);
        for anchor in anchors {
            if let Some(line) = self.cache.line(anchor) {
                frame
                    .buffer_mut()
                    .set_line(transcript_area.x, y, line, transcript_area.width);
            }
            self.link_hits
                .extend(self.cache.links(anchor).iter().map(|link| {
                    LinkHitRegion {
                        destination: Arc::clone(&link.destination),
                        row: y,
                        start: area.x.saturating_add(link.start),
                        end: transcript_area
                            .x
                            .saturating_add(link.end)
                            .min(transcript_area.right()),
                    }
                }));
            self.selection_rows.push((y, anchor));
            if anchor.line == 0
                && let Some(entry) = self.model.entry(anchor.entry)
            {
                if !is_expandable(entry) {
                    y = y.saturating_add(1);
                    continue;
                }
                self.expandable_hits.push(ExpandableHitRegion {
                    entry: anchor.entry,
                    row: y,
                });
                if matches!(
                    &entry.kind,
                    EntryKind::Tool(tool)
                        if tool.state == crate::tui::transcript::ToolState::Running
                ) && let Some(spinner) = self.tool_spinner
                {
                    let spinner_x = transcript_area
                        .x
                        .saturating_add(4)
                        .saturating_add(nested_tool_indent(entry, transcript_area.width));
                    if spinner_x < transcript_area.right() {
                        frame.buffer_mut().set_string(
                            spinner_x,
                            y,
                            spinner.symbol(),
                            Style::default()
                                .fg(theme.accent())
                                .add_modifier(Modifier::BOLD),
                        );
                    }
                }
                if self.expandables_focused && self.selected_expandable == Some(anchor.entry) {
                    frame.buffer_mut().set_string(
                        transcript_area.x,
                        y,
                        "›",
                        Style::default()
                            .fg(theme.accent())
                            .add_modifier(Modifier::BOLD),
                    );
                }
            }
            y = y.saturating_add(1);
        }
    }
}

fn render_top_right_hint(
    frame: &mut Frame<'_>,
    area: Rect,
    labels: &[&str],
    color: Color,
) -> Option<Rect> {
    let label = labels
        .iter()
        .copied()
        .find(|label| line_width(label) <= usize::from(area.width))?;
    let width = u16::try_from(line_width(label)).unwrap_or(u16::MAX);
    let x = area.right().saturating_sub(width);
    frame.buffer_mut().set_line(
        x,
        area.y,
        &Line::from(Span::styled(
            label,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )),
        area.right().saturating_sub(x),
    );
    Some(Rect::new(x, area.y, width, 1))
}

fn render_entry(
    entry: &TranscriptEntry,
    live_duration_ns: Option<u64>,
    width: u16,
    theme: &Theme,
    expanded: bool,
) -> markdown::Layout {
    let mut layout = match &entry.kind {
        EntryKind::User { text, .. } => render_user(text, width, theme),
        EntryKind::Assistant { text, .. } => markdown::render(text, width, theme),
        EntryKind::Reasoning { text } => {
            let mut layout = markdown::render(text, width.saturating_sub(2), theme);
            for line in &mut layout.lines {
                for span in &mut line.spans {
                    span.style = span.style.patch(
                        Style::default()
                            .fg(theme.muted())
                            .add_modifier(Modifier::ITALIC),
                    );
                }
            }
            layout
        }
        EntryKind::Tool(tool) => {
            let indent = nested_tool_indent(entry, width);
            let tool_width = width.saturating_sub(indent);
            let mut layout =
                tool::render_layout(tool, live_duration_ns, tool_width, theme, expanded);
            indent_nested_tool(
                indent,
                &mut layout.lines,
                theme,
                expanded,
                entry.trailing_spacer,
            );
            for spans in &mut layout.selections {
                for span in spans {
                    span.columns.start = span.columns.start.saturating_add(indent);
                    span.columns.end = span.columns.end.saturating_add(indent);
                }
            }
            layout
        }
        EntryKind::DirectedMessage(thread) => {
            layout_without_links(message::render(thread, width, theme, expanded))
        }
        EntryKind::ForkedFrom { session_id } => {
            layout_without_links(vec![Line::from(Span::styled(
                format!("◇ Forked from @@{session_id}"),
                Style::default().fg(theme.muted()),
            ))])
        }
        EntryKind::EffortChanged { to } => layout_without_links(vec![Line::from(vec![
            Span::styled("◇ Effort changed to ", Style::default().fg(theme.muted())),
            Span::styled(
                to.as_str(),
                Style::default()
                    .fg(theme.effort(*to))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " · takes effect on the next turn",
                Style::default().fg(theme.muted()),
            ),
        ])]),
        EntryKind::FastModeChanged { enabled } => {
            let status = if *enabled { "enabled" } else { "disabled" };
            layout_without_links(vec![Line::from(vec![
                Span::styled(
                    "⚡ ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("Fast mode {status}"),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    " · takes effect on the next turn",
                    Style::default().fg(theme.muted()),
                ),
            ])])
        }
        EntryKind::Interrupted { count } => {
            let label = if *count == 1 {
                "◇ Interrupted response".to_owned()
            } else {
                format!("◇ Interrupted {count} responses")
            };
            layout_without_links(vec![Line::from(Span::styled(
                label,
                Style::default().fg(theme.border()),
            ))])
        }
        EntryKind::ContextCompacted { duration_ns } => {
            layout_without_links(vec![Line::from(Span::styled(
                format!("◇ Context compacted · {}", format_duration(*duration_ns)),
                Style::default().fg(theme.muted()),
            ))])
        }
        EntryKind::TurnCompleted { duration_ns } => {
            layout_without_links(vec![Line::from(Span::styled(
                format!("◇ Turn completed · {}", format_turn_duration(*duration_ns)),
                Style::default().fg(theme.muted()),
            ))])
        }
        EntryKind::ContextCompactionFailed { message } => {
            layout_without_links(vec![Line::from(Span::styled(
                format!("◇ Context compaction failed · continuing · {message}"),
                Style::default().fg(theme.thinking_high()),
            ))])
        }
        EntryKind::Error { message } => layout_without_links(markdown::wrap_plain(
            &format!("× {message}"),
            width,
            Style::default().fg(theme.thinking_xhigh()),
        )),
    };
    if entry.trailing_spacer {
        layout.lines.push(Line::default());
        layout.links.push(Vec::new());
        layout.selections.push(Vec::new());
    }
    layout
}

fn render_live_tool_summary(
    entry: &TranscriptEntry,
    tool: &crate::tui::transcript::ToolEntry,
    duration_ns: u64,
    width: u16,
    theme: &Theme,
    expanded: bool,
) -> Vec<Line<'static>> {
    let indent = nested_tool_indent(entry, width);
    let tool_width = width.saturating_sub(indent);
    let mut lines = tool::render_live_summary(tool, duration_ns, tool_width, theme, expanded);
    indent_nested_tool(indent, &mut lines, theme, expanded, entry.trailing_spacer);
    lines
}

const fn nested_tool_indent(entry: &TranscriptEntry, width: u16) -> u16 {
    if entry.parent.is_some() {
        let available = width.saturating_sub(1);
        if available < NESTED_TOOL_INDENT {
            available
        } else {
            NESTED_TOOL_INDENT
        }
    } else {
        0
    }
}

fn indent_nested_tool(
    indent: u16,
    lines: &mut [Line<'static>],
    theme: &Theme,
    expanded: bool,
    terminal: bool,
) {
    let (terminal_marker, continuing_marker, continuation) = match indent {
        0 => return,
        1 => ("└", "├", "│"),
        2 => ("└─", "├─", "│ "),
        3 => (" └─", " ├─", " │ "),
        _ => ("  └─", "  ├─", "  │ "),
    };
    if lines.is_empty() {
        return;
    }
    let line_count = lines.len();
    for (index, line) in lines.iter_mut().enumerate() {
        let marker = if index == 0 {
            if expanded || !terminal || line_count > 1 {
                continuing_marker
            } else {
                terminal_marker
            }
        } else if !expanded && terminal && index + 1 == line_count {
            terminal_marker
        } else {
            continuation
        };
        line.spans
            .insert(0, Span::styled(marker, Style::default().fg(theme.border())));
    }
}

fn entry_selection_source(entry: &TranscriptEntry) -> Option<&str> {
    match &entry.kind {
        EntryKind::User { text }
        | EntryKind::Assistant { text, .. }
        | EntryKind::Reasoning { text } => Some(text),
        _ => None,
    }
}

fn layout_without_links(lines: Vec<Line<'static>>) -> markdown::Layout {
    let links = vec![Vec::new(); lines.len()];
    let selections = vec![Vec::new(); lines.len()];
    markdown::Layout {
        lines,
        links,
        selections,
        envelopes: Vec::new(),
        selection_source: None,
    }
}

fn render_user(text: &str, width: u16, theme: &Theme) -> markdown::Layout {
    let color = theme.thinking_medium();
    let content_width = width.saturating_sub(2).max(1);
    let mut lines = Vec::new();
    let mut selections = Vec::new();
    let mut source_offset = 0;
    for logical in text.split('\n') {
        let wrapped = markdown::wrap_plain_preserving_whitespace(
            logical,
            content_width,
            Style::default().fg(color),
        );
        let wrapped_selections = markdown::plain_selection_spans(logical, &wrapped);
        for (line, mut line_selections) in wrapped.into_iter().zip(wrapped_selections) {
            for selection in &mut line_selections {
                selection.columns.start = selection.columns.start.saturating_add(2);
                selection.columns.end = selection.columns.end.saturating_add(2);
                selection.source.start = selection.source.start.saturating_add(source_offset);
                selection.source.end = selection.source.end.saturating_add(source_offset);
            }
            lines.push(Line::from(
                std::iter::once(Span::styled("┃ ", Style::default().fg(color)))
                    .chain(line.spans)
                    .collect::<Vec<_>>(),
            ));
            selections.push(line_selections);
        }
        source_offset = source_offset
            .saturating_add(logical.len())
            .saturating_add(1);
    }
    markdown::Layout {
        links: vec![Vec::new(); lines.len()],
        lines,
        selections,
        envelopes: Vec::new(),
        selection_source: None,
    }
}

fn line_width(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
}

#[cfg(test)]
mod tests {
    use super::{
        Anchor, Component, ExpandableCommand, RenderRequest, ScrollCommand, ScrollState,
        Transcript, TranscriptEvent, unix_milliseconds,
    };
    use crate::{
        app::config::{ReasoningEffort, ReasoningMode},
        tui::{
            theme::Theme,
            transcript::{EntryKind, LocalEvent, SessionStarted, TranscriptRecord, TurnId},
        },
    };
    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use nanocodex::{
        Model,
        agent::events::{AgentEvent, AgentEventKind},
    };
    use ratatui::{Terminal, backend::TestBackend, layout::Position, style::Color};
    use serde_json::{json, value::to_raw_value};
    use std::{sync::Arc, time::Duration};
    use tact_subagents::{AgentMessageUpdate, MessageSender};

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

    fn scroll(transcript: &mut Transcript, event: Event) {
        let command = transcript
            .scroll_command(&event)
            .expect("test event should be a transcript scroll command");
        transcript.update(TranscriptEvent::Scroll(command));
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
    fn fork_start_renders_its_parent_session_boundary() {
        let mut transcript = Transcript::new();
        transcript.update(TranscriptEvent::Record(Arc::new(
            TranscriptRecord::from_local(
                1,
                1,
                LocalEvent::SessionStarted(SessionStarted {
                    session_id: "fork".to_owned(),
                    parent_session_id: Some("parent".to_owned()),
                    parent_sequence: Some(0),
                    model: Model::Luna.to_string(),
                    effort: ReasoningEffort::Medium,
                    reasoning_mode: ReasoningMode::Standard,
                    fast_mode: false,
                    workspace: "/work".into(),
                    application_version: "test".to_owned(),
                }),
            )
            .unwrap(),
        )));

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
        transcript.scroll = ScrollState::Detached(Anchor {
            entry: answer,
            line: 2,
        });

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
        transcript.scroll = ScrollState::Detached(Anchor {
            entry: answer,
            line: 2,
        });

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

        assert!(matches!(transcript.scroll, ScrollState::Follow));
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
        transcript.scroll = ScrollState::Detached(Anchor {
            entry: answer,
            line: 2,
        });
        drop(render(&mut transcript, 30, 6));
        let transcript_top = transcript.last_top;

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

        assert_eq!(transcript.last_top, transcript_top);
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
        transcript.scroll = ScrollState::Detached(Anchor {
            entry: answer,
            line: 2,
        });
        drop(render(&mut transcript, 30, 6));
        assert_eq!(transcript.pinned_prompt.unwrap().area.height, 3);

        let page_up = transcript.scroll_command(&Event::Key(KeyEvent::new(
            KeyCode::PageUp,
            KeyModifiers::NONE,
        )));
        let page_down = transcript.scroll_command(&Event::Key(KeyEvent::new(
            KeyCode::PageDown,
            KeyModifiers::NONE,
        )));

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
        transcript.scroll = ScrollState::Detached(Anchor {
            entry: answer,
            line: 2,
        });
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
            transcript.last_top,
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
            transcript.scroll = ScrollState::Detached(Anchor {
                entry: answer,
                line: 2,
            });
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
        transcript.scroll = ScrollState::Detached(Anchor {
            entry: second_prompt,
            line: 0,
        });

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
                LocalEvent::FastModeChanged {
                    from: false,
                    to: true,
                },
            )
            .unwrap(),
        )));

        let backend = render(&mut transcript, 72, 6);
        let cells = backend.buffer().content();
        let rendered = cells.iter().map(|cell| cell.symbol()).collect::<String>();
        let bolt = cells
            .iter()
            .find(|cell| cell.symbol() == "⚡")
            .expect("fast-mode notification should include a bolt");
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
        assert!(rendered.contains("Fast mode enabled · takes effect on the next turn"));
        assert_eq!(bolt.fg, Color::Yellow);
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
    fn retry_status_counts_down_during_backoff() {
        let mut transcript = Transcript::new();
        transcript.update(TranscriptEvent::Record(user(1, "hello")));
        transcript.update(TranscriptEvent::Record(agent_with_payload(
            2,
            AgentEventKind::ModelAttemptRetrying,
            json!({"delay_ns": 2_000_000_000_u64, "error": "temporary"}),
        )));

        assert_eq!(
            transcript.activity().status.as_deref(),
            Some("Retrying in 2.0s…")
        );
        let deadline = transcript
            .animation_deadline()
            .expect("retry backoff should schedule countdown frames");
        transcript.update(TranscriptEvent::AnimationFrame(deadline));

        assert_ne!(
            transcript.activity().status.as_deref(),
            Some("Retrying in 2.0s…")
        );
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
        let row = transcript.expandable_hits[0].row;
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
        let hit = transcript
            .link_hits
            .iter()
            .find(|hit| hit.destination.as_ref() == "/work/src/main.rs:12")
            .expect("rendered link should have a hit region");
        let event = Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: hit.start,
            row: hit.row,
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
        transcript.scroll = ScrollState::Detached(Anchor {
            entry: transcript.model.entries()[8].id,
            line: 0,
        });
        drop(render(&mut transcript, 60, 10));
        transcript.focus_expandables();
        drop(render(&mut transcript, 60, 10));
        let before = transcript.expandable_hits[0].row;

        transcript.update(TranscriptEvent::Expandable(ExpandableCommand::Toggle));
        drop(render(&mut transcript, 60, 10));

        assert_eq!(transcript.expandable_hits[0].row, before);
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
        assert!(matches!(transcript.scroll, ScrollState::Follow));
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

        assert!(matches!(transcript.scroll, ScrollState::Detached(_)));
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
            unix_milliseconds().saturating_sub(10_000),
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
        let recorded_at = unix_milliseconds();
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
        transcript.cache.expansion_overrides.insert(id, true);
        drop(render(&mut transcript, 50, 10));
        let cached = transcript.cache.entries.get(&id).unwrap();
        let details = cached.lines[cached.tool_summary_lines..].to_vec();
        let timer = transcript.running_tool_timers[&id];

        let frame_at = timer.observed_at + Duration::from_millis(1_234);
        transcript.update(TranscriptEvent::AnimationFrame(frame_at));
        drop(render(&mut transcript, 50, 10));

        let cached = transcript.cache.entries.get(&id).unwrap();
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
}
