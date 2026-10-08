//! Scrollable rendering of the persisted agent session.
//!
//! [Transcript] projects transcript records and directed-message updates into a
//! [TranscriptModel] and draws it bottom-aligned in its area. It owns:
//!
//! - scrolling and tail following ([viewport]), including the count of updates
//!   that arrived while the reader was scrolled away;
//! - the per-entry layout cache ([cache]) and entry rendering ([render]);
//! - expandable tool and message entries, their keyboard focus, and the
//!   expand-all toggle;
//! - the prompt pinned above a detached viewport ([pinned]);
//! - the regions that mouse clicks and text selection resolve against ([hits]).
//!
//! Updates emit a [TranscriptEffect] only when the turn activity or its
//! transient status label changes. Records that originate from Tact itself
//! request an immediate frame; streamed agent output requests a streaming frame.

mod cache;
mod diff;
mod empty;
mod highlight;
mod hits;
pub(crate) mod image;
mod markdown;
mod message;
mod pinned;
mod render;
mod tool;
mod viewport;

pub(super) use viewport::ScrollCommand;

use super::{
    clock::unix_time_ms,
    node::{Component, ComponentUpdate, RenderRequest},
    selection::{TextRange, TextSpan},
};
use crate::{
    app::{config::ReasoningEffort, theme::Theme},
    core::transcript::{
        EntryId, EntryKind, ToolState, TranscriptEntry, TranscriptModel, TranscriptRecord,
        TransientStatus,
    },
    tui::{format::format_duration, spinner::Spinner},
};
use cache::LayoutCache;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use empty::EmptyLogo;
use hits::{HitMap, SelectionReach};
use pinned::PinnedPrompt;
use ratatui::{
    Frame,
    buffer::Buffer,
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Widget},
};
use ratatui_image::sliced::{SignedPosition, SlicedImage, SlicedProtocol};
use render::nested_tool_indent;
use std::{
    collections::HashMap,
    num::NonZeroU16,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use tact_subagents::{AgentMessageUpdate, MessageSender};
use viewport::{Anchor, Layouts, RenderPlan, Reveal, Viewport};

const EXPANDABLE_FOCUS_HINTS: [&str; 2] =
    ["↑↓ item · Enter toggle · Esc back", "↑↓ item · Enter · Esc"];

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

/// Turn activity reported to the composer, which owns the activity indicator.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranscriptEffect {
    pub(crate) active: bool,
    pub(crate) status: Option<String>,
}

pub(crate) struct Transcript {
    model: TranscriptModel,
    cache: LayoutCache,
    viewport: Viewport,
    hits: HitMap,
    tool_spinner: Option<Spinner>,
    running_tool_timers: HashMap<EntryId, RunningToolTimer>,
    expandables_focused: bool,
    /// The expandable that keyboard focus acts on; retained while unfocused so
    /// focus returns to the same entry.
    selected_expandable: Option<EntryId>,
    empty_logo: EmptyLogo,
    effort: ReasoningEffort,
    pinned_prompt: Option<PinnedPrompt>,
    updates_banner_area: Option<Rect>,
}

/// Measures a running tool on the monotonic clock, seeded with the wall-clock
/// time that elapsed before the tool was first observed.
#[derive(Clone, Copy)]
struct RunningToolTimer {
    observed_at: Instant,
    elapsed_at_observation: Duration,
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

#[derive(Clone, Copy)]
pub(super) enum ExpandableCommand {
    Previous,
    Next,
    Toggle,
    Click { row: u16 },
}

/// The direction of keyboard navigation between expandable entries.
#[derive(Clone, Copy)]
enum Direction {
    Previous,
    Next,
}

impl Transcript {
    pub(super) fn assistant_response(&self, index: usize) -> Option<&str> {
        self.model
            .entries()
            .iter()
            .rev()
            .filter_map(|entry| match &entry.kind {
                EntryKind::Assistant {
                    text,
                    complete: true,
                } if !entry.hidden && !text.trim().is_empty() => Some(text.as_str()),
                _ => None,
            })
            .nth(index.checked_sub(1)?)
    }

    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Self::with_effort(ReasoningEffort::default())
    }

    pub(crate) fn with_effort(effort: ReasoningEffort) -> Self {
        Self {
            model: TranscriptModel::default(),
            cache: LayoutCache::default(),
            viewport: Viewport::default(),
            hits: HitMap::default(),
            tool_spinner: None,
            running_tool_timers: HashMap::new(),
            expandables_focused: false,
            selected_expandable: None,
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
            .cache
            .set_workspace(self.cache.workspace().to_path_buf());
        snapshot
    }

    pub(crate) fn set_workspace(&mut self, workspace: &Path) {
        self.cache.set_workspace(workspace.to_path_buf());
    }

    pub(crate) fn refresh_terminal_images(&mut self) {
        self.cache.refresh_terminal_images();
    }

    pub(crate) const fn set_effort(&mut self, effort: ReasoningEffort) {
        self.effort = effort;
    }

    /// Draws the hints overlaid on the top-right corner of the transcript: the
    /// expandable navigation keys while focused, otherwise the count of updates
    /// below a detached viewport.
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

        let new_updates = self.viewport.new_updates();
        if !self.viewport.is_detached() || new_updates == 0 {
            return;
        }

        let noun = if new_updates == 1 {
            "update"
        } else {
            "updates"
        };
        let label = format!("↓ {new_updates} {noun} · Ctrl+End to follow");
        let compact_label = format!("↓ {new_updates} {noun} · Ctrl+End");
        self.updates_banner_area =
            render_top_right_hint(frame, area, &[&label, &compact_label], theme.border());
    }

    pub(crate) fn animation_deadline(&self) -> Option<Instant> {
        let empty = self.is_empty().then(|| self.empty_logo.deadline());
        self.tool_spinner
            .map(Spinner::deadline)
            .into_iter()
            .chain(empty)
            .chain(self.cache.images().animation_deadline())
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
        self.sync_running_tool_timers(now);
        let tool_active = self.model.has_running_tools();
        if tool_active && self.tool_spinner.is_none() {
            self.tool_spinner = Some(Spinner::new(now));
        } else if !tool_active {
            self.tool_spinner = None;
        }
        if change.changed {
            self.viewport.record_update();
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
        self.viewport.record_update();
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn forget_entry(&mut self, id: EntryId) {
        self.cache.forget(id);
        self.running_tool_timers.remove(&id);
        self.hits.forget(id);
        if self.selected_expandable == Some(id) {
            self.selected_expandable = None;
        }
        self.viewport.forget(id);
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
            status: self.model.transient().map(transient_label),
        }
    }

    fn update_animation(&mut self, now: Instant) -> ComponentUpdate<TranscriptEffect> {
        let timer_changed = self.refresh_running_tool_durations(now);
        let tool_changed = self
            .tool_spinner
            .as_mut()
            .is_some_and(|spinner| spinner.advance(now));
        let logo_changed = self.is_empty() && self.empty_logo.advance(now);
        let images_changed = self.cache.poll_images(now);
        ComponentUpdate {
            effects: Vec::new(),
            render: if timer_changed || tool_changed || logo_changed || images_changed {
                RenderRequest::Streaming
            } else {
                RenderRequest::None
            },
        }
    }

    fn sync_running_tool_timers(&mut self, now: Instant) {
        self.running_tool_timers
            .retain(|id, _| self.model.entry(*id).is_some_and(is_running_tool));
        let observed_at_unix_ms = unix_time_ms();
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

    pub(super) fn scroll_command(
        &self,
        event: &Event,
        mouse_scroll_lines: NonZeroU16,
    ) -> Option<ScrollCommand> {
        let command = match event {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                match (key.code, key.modifiers) {
                    (KeyCode::PageUp, _) => ScrollCommand::Rows(-self.viewport.page_size()),
                    (KeyCode::PageDown, _) => ScrollCommand::Rows(self.viewport.page_size()),
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
                    && prompt.contains(Position::new(mouse.column, mouse.row))
                {
                    match mouse.kind {
                        MouseEventKind::ScrollUp if prompt.can_scroll_up() => {
                            return Some(ScrollCommand::PinnedPromptRows(-1));
                        }
                        MouseEventKind::ScrollDown if prompt.can_scroll_down() => {
                            return Some(ScrollCommand::PinnedPromptRows(1));
                        }
                        _ => {}
                    }
                }
                let rows = i32::from(mouse_scroll_lines.get());
                match mouse.kind {
                    MouseEventKind::ScrollUp => ScrollCommand::Rows(-rows),
                    MouseEventKind::ScrollDown => ScrollCommand::Rows(rows),
                    _ => return None,
                }
            }
            _ => return None,
        };
        Some(command)
    }

    pub(super) fn pinned_prompt_clicked(&self, event: &Event) -> bool {
        left_click(event).is_some_and(|position| {
            self.pinned_prompt
                .is_some_and(|prompt| prompt.contains(position))
        })
    }

    pub(super) fn updates_banner_clicked(&self, event: &Event) -> bool {
        left_click(event).is_some_and(|position| {
            self.updates_banner_area
                .is_some_and(|area| area.contains(position))
        })
    }

    pub(super) fn expandable_command(&self, event: &Event) -> Option<ExpandableCommand> {
        if let Some(position) = left_click(event) {
            return self
                .hits
                .expandable_at(position.y)
                .map(|_| ExpandableCommand::Click { row: position.y });
        }
        let Event::Key(key) = event else {
            return None;
        };
        if !self.expandables_focused
            || !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        {
            return None;
        }
        match key.code {
            KeyCode::Up => Some(ExpandableCommand::Previous),
            KeyCode::Down => Some(ExpandableCommand::Next),
            KeyCode::Enter => Some(ExpandableCommand::Toggle),
            _ => None,
        }
    }

    pub(super) fn link_destination(&self, event: &Event) -> Option<Arc<str>> {
        self.hits.link_at(left_click(event)?)
    }

    pub(super) fn selection_span(&self, position: Position) -> Option<TextSpan> {
        self.hits
            .selection_span(position, SelectionReach::Row, &self.model, &self.cache)
    }

    #[cfg(test)]
    pub(super) fn selection_span_nearest(&self, position: Position) -> Option<TextSpan> {
        self.hits.selection_span(
            position,
            SelectionReach::Nearest { origin: None },
            &self.model,
            &self.cache,
        )
    }

    pub(super) fn selection_span_nearest_from(
        &self,
        position: Position,
        origin: TextSpan,
    ) -> Option<TextSpan> {
        self.hits.selection_span(
            position,
            SelectionReach::Nearest {
                origin: Some(origin),
            },
            &self.model,
            &self.cache,
        )
    }

    /// The selected source text of every entry in `range`, separated by blank
    /// lines.
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
        self.hits.render_selection(buffer, range, &self.cache);
    }

    pub(super) const fn expandables_focused(&self) -> bool {
        self.expandables_focused
    }

    fn update_scroll(&mut self, command: ScrollCommand) -> ComponentUpdate<TranscriptEffect> {
        if let ScrollCommand::PinnedPromptRows(rows) = command {
            let Some(prompt) = &mut self.pinned_prompt else {
                return ComponentUpdate::none();
            };
            prompt.scroll_by(rows);
            return ComponentUpdate::render(RenderRequest::Immediate);
        }
        self.viewport.request_scroll(command);
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn jump_to_pinned_prompt(&mut self) -> ComponentUpdate<TranscriptEffect> {
        let Some(prompt) = self.pinned_prompt.take() else {
            return ComponentUpdate::none();
        };
        self.viewport.detach_at(Anchor {
            entry: prompt.entry,
            line: 0,
        });
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn follow_tail(&mut self) -> ComponentUpdate<TranscriptEffect> {
        if self.viewport.follow() {
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
            self.selected_expandable = self.hits.last_expandable();
        }
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn update_expandable(
        &mut self,
        command: ExpandableCommand,
    ) -> ComponentUpdate<TranscriptEffect> {
        match command {
            ExpandableCommand::Previous => self.select_expandable(Direction::Previous),
            ExpandableCommand::Next => self.select_expandable(Direction::Next),
            ExpandableCommand::Toggle => self.toggle_selected_expandable(),
            ExpandableCommand::Click { row } => {
                let Some(entry) = self.hits.expandable_at(row) else {
                    return ComponentUpdate::none();
                };
                self.expandables_focused = true;
                self.selected_expandable = Some(entry);
                self.toggle_selected_expandable()
            }
        }
    }

    /// Moves keyboard selection to the neighbouring visible expandable. Without
    /// a selection, either direction starts from the newest expandable.
    fn select_expandable(&mut self, direction: Direction) -> ComponentUpdate<TranscriptEffect> {
        let entries = self.model.entries();
        let selected = self
            .selected_expandable
            .and_then(|selected| self.model.index_of(selected));
        let candidate = |entry: &&TranscriptEntry| !entry.hidden && is_expandable(entry);
        let next = match (direction, selected) {
            (Direction::Previous, selected) => entries[..selected.unwrap_or(entries.len())]
                .iter()
                .rev()
                .find(candidate),
            (Direction::Next, Some(selected)) => {
                entries[selected.saturating_add(1)..].iter().find(candidate)
            }
            (Direction::Next, None) => entries.iter().rev().find(candidate),
        };
        let Some(selected) = next.map(|entry| entry.id) else {
            return ComponentUpdate::none();
        };
        self.selected_expandable = Some(selected);
        if self.hits.expandable_row(selected).is_none() {
            self.viewport.request_reveal(Reveal::Entry(selected));
        }
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    /// Toggles the selected expandable while keeping its summary on the same
    /// viewport row.
    fn toggle_selected_expandable(&mut self) -> ComponentUpdate<TranscriptEffect> {
        let Some(entry_id) = self.selected_expandable else {
            return ComponentUpdate::none();
        };
        let Some(entry) = self.model.entry(entry_id) else {
            return ComponentUpdate::none();
        };
        let row = self.hits.expandable_row(entry_id).unwrap_or(0);
        self.cache.toggle(entry);
        self.viewport.request_reveal(Reveal::Preserve {
            entry: entry_id,
            row,
        });
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn render_plan(&mut self, width: u16, height: u16, theme: &Theme) -> RenderPlan {
        let mut layouts = Layouts {
            model: &self.model,
            cache: &mut self.cache,
            width,
            theme,
        };
        self.viewport.plan(&mut layouts, height)
    }

    /// The prompt of the turn at the top of a detached viewport, unless that
    /// top line already belongs to a prompt.
    fn pinned_prompt_entry(&self, top: Option<Anchor>) -> Option<EntryId> {
        if !self.viewport.is_detached() {
            return None;
        }
        let top_index = self.model.index_of(top?.entry)?;
        let entries = self.model.entries();
        if matches!(entries[top_index].kind, EntryKind::User { .. }) {
            return None;
        }
        entries[..top_index]
            .iter()
            .rev()
            .find(|entry| !entry.hidden && matches!(entry.kind, EntryKind::User { .. }))
            .map(|entry| entry.id)
    }

    /// Places the pinned prompt for this frame. Returns the rows it occupies.
    fn prepare_pinned_prompt(
        &mut self,
        entry_id: Option<EntryId>,
        area: Rect,
        theme: &Theme,
    ) -> u16 {
        let entry = entry_id.and_then(|id| self.model.entry(id));
        self.pinned_prompt = entry.and_then(|entry| {
            let mut line_count = self.cache.layout(entry, area.width, theme).len();
            if entry.trailing_spacer {
                line_count = line_count.saturating_sub(1);
            }
            PinnedPrompt::place(self.pinned_prompt, entry.id, line_count, area)
        });
        self.pinned_prompt.map_or(0, |prompt| prompt.area.height)
    }
}

fn left_click(event: &Event) -> Option<Position> {
    match event {
        Event::Mouse(mouse) if mouse.kind == MouseEventKind::Down(MouseButton::Left) => {
            Some(Position::new(mouse.column, mouse.row))
        }
        _ => None,
    }
}

fn transient_label(status: &TransientStatus) -> String {
    match status {
        TransientStatus::Thinking => "Thinking…".to_owned(),
        TransientStatus::Responding => "Responding…".to_owned(),
        TransientStatus::Warming => "Warming model…".to_owned(),
        TransientStatus::WaitingForBackgroundWork => "Waiting for background work…".to_owned(),
        TransientStatus::Tool(tool) => format!("Running {tool}…"),
        TransientStatus::Compacting => "Compacting context…".to_owned(),
        TransientStatus::Retrying {
            delay_ns,
            next_attempt,
            max_attempts,
        } => format!(
            "Retrying in {} (attempt {next_attempt}/{max_attempts})…",
            format_duration(*delay_ns)
        ),
        TransientStatus::Connecting => "Connecting…".to_owned(),
        TransientStatus::Reconnecting => "Reconnecting…".to_owned(),
        TransientStatus::Error(error) => error.clone(),
    }
}

fn is_running_tool(entry: &TranscriptEntry) -> bool {
    matches!(&entry.kind, EntryKind::Tool(tool) if tool.state == ToolState::Running)
}

fn is_expandable(entry: &TranscriptEntry) -> bool {
    matches!(
        entry.kind,
        EntryKind::Tool(_) | EntryKind::DirectedMessage(_)
    )
}

/// An image placement visible in the current frame.
struct VisibleImage {
    entry: EntryId,
    /// The layout line the image starts on.
    line: usize,
    protocol: Arc<SlicedProtocol>,
    position: SignedPosition,
}

impl VisibleImage {
    fn covers(&self, anchor: Anchor) -> bool {
        self.entry == anchor.entry
            && anchor.line >= self.line
            && anchor.line
                < self
                    .line
                    .saturating_add(usize::from(self.protocol.size().height))
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
        self.viewport.set_height(area.height);
        self.hits.begin_frame(area);
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
        self.viewport.set_height(transcript_area.height);
        if let Some(prompt) = self.pinned_prompt {
            plan = self.render_plan(transcript_area.width, transcript_area.height, theme);
            prompt.render(frame, &self.cache, &mut self.hits, theme);
        }
        let RenderPlan {
            top_padding,
            anchors,
        } = plan;
        let marker_style = Style::default()
            .fg(theme.accent())
            .add_modifier(Modifier::BOLD);
        let mut y = transcript_area.y.saturating_add(top_padding);
        let mut visible_images: Vec<VisibleImage> = Vec::new();
        for anchor in anchors {
            let row = y;
            y = y.saturating_add(1);
            if let Some(line) = self.cache.line(anchor) {
                frame
                    .buffer_mut()
                    .set_line(transcript_area.x, row, line, transcript_area.width);
            }
            self.hits.push_links(row, self.cache.links(anchor));
            self.hits.push_row(row, anchor);
            let covered = visible_images
                .last()
                .is_some_and(|image| image.covers(anchor));
            if !covered && let Some((line, protocol)) = self.cache.image(anchor) {
                let offset = i32::try_from(anchor.line.saturating_sub(line)).unwrap_or(i32::MAX);
                let position = i32::from(row)
                    .saturating_sub(i32::from(transcript_area.y))
                    .saturating_sub(offset)
                    .clamp(i32::from(i16::MIN), i32::from(i16::MAX));
                visible_images.push(VisibleImage {
                    entry: anchor.entry,
                    line,
                    protocol,
                    position: SignedPosition::from((0, position as i16)),
                });
            }
            if anchor.line != 0 {
                continue;
            }
            let Some(entry) = self.model.entry(anchor.entry) else {
                continue;
            };
            if !is_expandable(entry) {
                continue;
            }
            self.hits.push_expandable(anchor.entry, row);
            if is_running_tool(entry)
                && let Some(spinner) = self.tool_spinner
            {
                let spinner_x = transcript_area
                    .x
                    .saturating_add(4)
                    .saturating_add(nested_tool_indent(entry, transcript_area.width));
                if spinner_x < transcript_area.right() {
                    frame
                        .buffer_mut()
                        .set_string(spinner_x, row, spinner.symbol(), marker_style);
                }
            }
            if self.expandables_focused && self.selected_expandable == Some(anchor.entry) {
                frame
                    .buffer_mut()
                    .set_string(transcript_area.x, row, "›", marker_style);
            }
        }
        for image in visible_images {
            frame.render_widget(
                SlicedImage::new(&image.protocol, image.position),
                transcript_area,
            );
        }
    }
}

/// Draws the first of `labels` that fits on the top row of `area`, right
/// aligned. Returns the region it occupies.
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

fn line_width(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
}

#[cfg(test)]
mod tests;
