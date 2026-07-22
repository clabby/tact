//! Scrollable rendering of the persisted agent session.

mod diff;
mod empty;
mod highlight;
mod markdown;
mod tool;

use super::node::{Component, ComponentUpdate, RenderRequest};
use crate::{
    config::ReasoningEffort,
    tui::{
        format::format_duration,
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
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Widget},
};
use std::{
    collections::{HashMap, hash_map::Entry},
    sync::Arc,
    time::Instant,
};

pub(crate) enum TranscriptEvent {
    Record(Arc<TranscriptRecord>),
    AgentStreamClosed,
    Scroll(ScrollCommand),
    FollowTail,
    BlurTools,
    Tool(ToolCommand),
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
    tools_focused: bool,
    selected_tool: Option<EntryId>,
    tool_hits: Vec<ToolHitRegion>,
    link_hits: Vec<LinkHitRegion>,
    transcript_y: u16,
    pending_tool_anchor: Option<PendingToolAnchor>,
    empty_logo: EmptyLogo,
    effort: ReasoningEffort,
}

struct CachedEntry {
    revision: u64,
    width: u16,
    expanded: bool,
    lines: Vec<Line<'static>>,
    links: Vec<Vec<markdown::LinkSpan>>,
}

#[derive(Default)]
struct LayoutCache {
    entries: HashMap<EntryId, CachedEntry>,
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

#[derive(Clone, Copy, Eq, PartialEq)]
struct Anchor {
    entry: EntryId,
    line: usize,
}

#[derive(Clone, Copy)]
struct ToolHitRegion {
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
enum PendingToolAnchor {
    Reveal(EntryId),
    Preserve { entry: EntryId, row: u16 },
}

#[derive(Clone, Copy)]
pub(super) enum ToolCommand {
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
            tools_focused: false,
            selected_tool: None,
            tool_hits: Vec::new(),
            link_hits: Vec::new(),
            transcript_y: 0,
            pending_tool_anchor: None,
            empty_logo: EmptyLogo::new(Instant::now()),
            effort,
        }
    }

    pub(crate) fn fork_snapshot(&self) -> Self {
        let mut snapshot = Self::with_effort(self.effort);
        snapshot.model = self.model.fork_snapshot();
        snapshot
    }

    pub(crate) const fn set_effort(&mut self, effort: ReasoningEffort) {
        self.effort = effort;
    }

    pub(crate) fn animation_deadline(&self) -> Option<Instant> {
        let empty = self.is_empty().then(|| self.empty_logo.deadline());
        self.tool_spinner
            .map(Spinner::deadline)
            .into_iter()
            .chain(empty)
            .min()
    }

    fn update_record(
        &mut self,
        record: Arc<TranscriptRecord>,
    ) -> ComponentUpdate<TranscriptEffect> {
        let previous_activity = self.activity();
        let change = self.model.apply(&record);
        let activity = self.activity();
        let tool_active = self.model.has_running_tools();
        if tool_active && self.tool_spinner.is_none() {
            self.tool_spinner = Some(Spinner::new(Instant::now()));
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

    fn agent_stream_closed(&mut self) -> ComponentUpdate<TranscriptEffect> {
        let previous_activity = self.activity();
        if !self.model.agent_stream_closed() {
            return ComponentUpdate::none();
        }
        self.tool_spinner = self
            .model
            .has_running_tools()
            .then(|| Spinner::new(Instant::now()));
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
        let tool_changed = self
            .tool_spinner
            .as_mut()
            .is_some_and(|spinner| spinner.advance(now));
        let logo_changed = self.is_empty() && self.empty_logo.advance(now);
        ComponentUpdate::render(if tool_changed || logo_changed {
            RenderRequest::Streaming
        } else {
            RenderRequest::None
        })
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

    pub(super) fn tool_command(&self, event: &Event) -> Option<ToolCommand> {
        match event {
            Event::Mouse(mouse) if mouse.kind == MouseEventKind::Down(MouseButton::Left) => self
                .tool_hits
                .iter()
                .any(|hit| hit.row == mouse.row)
                .then_some(ToolCommand::Click { row: mouse.row }),
            Event::Key(key)
                if self.tools_focused
                    && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
            {
                match key.code {
                    KeyCode::Up => Some(ToolCommand::Previous),
                    KeyCode::Down => Some(ToolCommand::Next),
                    KeyCode::Enter => Some(ToolCommand::Toggle),
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

    pub(super) const fn tools_focused(&self) -> bool {
        self.tools_focused
    }

    fn update_scroll(&mut self, command: ScrollCommand) -> ComponentUpdate<TranscriptEffect> {
        self.pending_scroll = command;
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

    fn blur_tools(&mut self) -> ComponentUpdate<TranscriptEffect> {
        if !self.tools_focused {
            return ComponentUpdate::none();
        }
        self.tools_focused = false;
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    #[cfg(test)]
    pub(super) fn focus_tools(&mut self) -> ComponentUpdate<TranscriptEffect> {
        self.tools_focused = true;
        if self.selected_tool.is_none() {
            self.selected_tool = self.tool_hits.last().map(|hit| hit.entry);
        }
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn update_tool(&mut self, command: ToolCommand) -> ComponentUpdate<TranscriptEffect> {
        match command {
            ToolCommand::Previous => self.select_tool(-1),
            ToolCommand::Next => self.select_tool(1),
            ToolCommand::Toggle => self.toggle_selected_tool(),
            ToolCommand::Click { row } => {
                let Some(entry) = self
                    .tool_hits
                    .iter()
                    .find(|hit| hit.row == row)
                    .map(|hit| hit.entry)
                else {
                    return ComponentUpdate::none();
                };
                self.tools_focused = true;
                self.selected_tool = Some(entry);
                self.toggle_selected_tool()
            }
        }
    }

    fn select_tool(&mut self, direction: i32) -> ComponentUpdate<TranscriptEffect> {
        let entries = self.model.entries();
        let selected = self
            .selected_tool
            .and_then(|selected| self.model.index_of(selected));
        let next = if direction < 0 {
            let end = selected.unwrap_or(entries.len());
            entries[..end]
                .iter()
                .rev()
                .find(|entry| !entry.hidden && matches!(entry.kind, EntryKind::Tool(_)))
        } else if let Some(selected) = selected {
            entries[selected.saturating_add(1)..]
                .iter()
                .find(|entry| !entry.hidden && matches!(entry.kind, EntryKind::Tool(_)))
        } else {
            entries
                .iter()
                .rev()
                .find(|entry| !entry.hidden && matches!(entry.kind, EntryKind::Tool(_)))
        };
        let Some(selected) = next.map(|entry| entry.id) else {
            return ComponentUpdate::none();
        };
        self.selected_tool = Some(selected);
        if !self.tool_hits.iter().any(|hit| hit.entry == selected) {
            self.pending_tool_anchor = Some(PendingToolAnchor::Reveal(selected));
        }
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn toggle_selected_tool(&mut self) -> ComponentUpdate<TranscriptEffect> {
        let Some(entry_id) = self.selected_tool else {
            return ComponentUpdate::none();
        };
        let Some(entry_index) = self.model.index_of(entry_id) else {
            return ComponentUpdate::none();
        };
        let row = self
            .tool_hits
            .iter()
            .find(|hit| hit.entry == entry_id)
            .map_or(0, |hit| hit.row.saturating_sub(self.transcript_y));
        self.cache.toggle(&self.model.entries()[entry_index]);
        self.pending_tool_anchor = Some(PendingToolAnchor::Preserve {
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
        self.apply_pending_tool_anchor(width, theme);
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

    fn apply_pending_tool_anchor(&mut self, width: u16, theme: &Theme) {
        let Some(request) = self.pending_tool_anchor.take() else {
            return;
        };
        let (entry, row) = match request {
            PendingToolAnchor::Reveal(entry) => (entry, 0),
            PendingToolAnchor::Preserve { entry, row } => (entry, row),
        };
        let anchor = Anchor { entry, line: 0 };
        let (top, _) = self.move_anchor(anchor, -i32::from(row), width, theme);
        self.scroll = ScrollState::Detached(top);
    }

    fn apply_pending_scroll(&mut self, width: u16, height: u16, theme: &Theme) {
        let command = std::mem::take(&mut self.pending_scroll);
        match command {
            ScrollCommand::None => {}
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

fn transient_label(status: &TransientStatus) -> String {
    match status {
        TransientStatus::Thinking => "Thinking…".to_owned(),
        TransientStatus::Responding => "Responding…".to_owned(),
        TransientStatus::Warming => "Warming model…".to_owned(),
        TransientStatus::WaitingForBackgroundWork => "Waiting for background work…".to_owned(),
        TransientStatus::Tool(tool) => format!("Running {tool}…"),
        TransientStatus::Compacting => "Compacting context…".to_owned(),
        TransientStatus::Retrying(delay) => format!("Retrying in {delay}…"),
        TransientStatus::Connecting => "Connecting…".to_owned(),
        TransientStatus::Reconnecting => "Reconnecting…".to_owned(),
        TransientStatus::Error(error) => error.clone(),
    }
}

impl LayoutCache {
    fn layout(&mut self, entry: &TranscriptEntry, width: u16, theme: &Theme) -> &[Line<'static>] {
        let expanded = self
            .expansion_overrides
            .get(&entry.id)
            .copied()
            .or(self.expand_all)
            .unwrap_or_else(|| Self::expanded_by_default(entry));
        let cached = match self.entries.entry(entry.id) {
            Entry::Occupied(mut occupied) => {
                let cached = occupied.get();
                if cached.revision != entry.revision
                    || cached.width != width
                    || cached.expanded != expanded
                {
                    occupied.insert(CachedEntry::new(entry, width, theme, expanded));
                }
                occupied.into_mut()
            }
            Entry::Vacant(vacant) => vacant.insert(CachedEntry::new(entry, width, theme, expanded)),
        };
        &cached.lines
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
}

impl CachedEntry {
    fn new(entry: &TranscriptEntry, width: u16, theme: &Theme, expanded: bool) -> Self {
        let layout = render_entry(entry, width, theme, expanded);
        Self {
            revision: entry.revision,
            width,
            expanded,
            lines: layout.lines,
            links: layout.links,
        }
    }
}

impl Component for Transcript {
    type Event = TranscriptEvent;
    type Effect = TranscriptEffect;

    fn update(&mut self, event: Self::Event) -> ComponentUpdate<Self::Effect> {
        match event {
            TranscriptEvent::Record(record) => self.update_record(record),
            TranscriptEvent::AgentStreamClosed => self.agent_stream_closed(),
            TranscriptEvent::Scroll(command) => self.update_scroll(command),
            TranscriptEvent::FollowTail => self.follow_tail(),
            TranscriptEvent::BlurTools => self.blur_tools(),
            TranscriptEvent::Tool(command) => self.update_tool(command),
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
        self.tool_hits.clear();
        self.link_hits.clear();
        Clear.render(area, frame.buffer_mut());
        if self.is_empty() {
            self.empty_logo.render(frame, area, theme, self.effort);
            return;
        }
        let RenderPlan {
            top_padding,
            anchors,
        } = self.render_plan(area.width, area.height, theme);
        let mut y = area.y.saturating_add(top_padding);
        for anchor in anchors {
            if let Some(line) = self.cache.line(anchor) {
                frame.buffer_mut().set_line(area.x, y, line, area.width);
            }
            self.link_hits
                .extend(self.cache.links(anchor).iter().map(|link| LinkHitRegion {
                    destination: Arc::clone(&link.destination),
                    row: y,
                    start: area.x.saturating_add(link.start),
                    end: area.x.saturating_add(link.end).min(area.right()),
                }));
            if anchor.line == 0
                && let Some(entry) = self.model.entry(anchor.entry)
                && let EntryKind::Tool(tool) = &entry.kind
            {
                self.tool_hits.push(ToolHitRegion {
                    entry: anchor.entry,
                    row: y,
                });
                if tool.state == crate::tui::transcript::ToolState::Running
                    && let Some(spinner) = self.tool_spinner
                {
                    frame.buffer_mut().set_string(
                        area.x.saturating_add(4),
                        y,
                        spinner.symbol(),
                        Style::default()
                            .fg(theme.accent())
                            .add_modifier(Modifier::BOLD),
                    );
                }
                if self.tools_focused && self.selected_tool == Some(anchor.entry) {
                    frame.buffer_mut().set_string(
                        area.x,
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
        if self.tools_focused {
            render_top_right_hint(
                frame,
                area,
                &["↑↓ tool · Enter toggle · Esc back", "↑↓ tool · Enter · Esc"],
                theme.accent(),
            );
        } else if matches!(self.scroll, ScrollState::Detached(_)) && self.new_updates > 0 {
            let noun = if self.new_updates == 1 {
                "update"
            } else {
                "updates"
            };
            let label = format!("↓ {} {noun} · Ctrl+End to follow", self.new_updates);
            let compact_label = format!("↓ {} {noun} · Ctrl+End", self.new_updates);
            render_top_right_hint(frame, area, &[&label, &compact_label], theme.border());
        }
    }
}

fn render_top_right_hint(frame: &mut Frame<'_>, area: Rect, labels: &[&str], color: Color) {
    let Some(label) = labels
        .iter()
        .copied()
        .find(|label| line_width(label) <= usize::from(area.width))
    else {
        return;
    };
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
}

fn render_entry(
    entry: &TranscriptEntry,
    width: u16,
    theme: &Theme,
    expanded: bool,
) -> markdown::Layout {
    let mut layout = match &entry.kind {
        EntryKind::User { text, .. } => layout_without_links(render_user(text, width, theme)),
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
        EntryKind::Tool(tool) if expanded => {
            layout_without_links(tool::render_expanded(tool, width, theme))
        }
        EntryKind::Tool(tool) => layout_without_links(tool::render(tool, width, theme)),
        EntryKind::Interrupted { count } => {
            let label = if *count == 0 {
                "◇ Nothing to interrupt".to_owned()
            } else if *count == 1 {
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
    layout.lines.push(Line::default());
    layout.links.push(Vec::new());
    layout
}

fn layout_without_links(lines: Vec<Line<'static>>) -> markdown::Layout {
    let links = vec![Vec::new(); lines.len()];
    markdown::Layout { lines, links }
}

fn render_user(text: &str, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    let color = theme.thinking_medium();
    let content_width = width.saturating_sub(2).max(1);
    let mut lines = Vec::new();
    for logical in text.split('\n') {
        for line in markdown::wrap_plain(logical, content_width, Style::default().fg(color)) {
            lines.push(Line::from(
                std::iter::once(Span::styled("┃ ", Style::default().fg(color)))
                    .chain(line.spans)
                    .collect::<Vec<_>>(),
            ));
        }
    }
    lines
}

fn line_width(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
}

#[cfg(test)]
mod tests {
    use super::{
        Anchor, Component, RenderRequest, ScrollCommand, ScrollState, ToolCommand, Transcript,
        TranscriptEvent,
    };
    use crate::tui::{
        theme::Theme,
        transcript::{LocalEvent, TranscriptRecord, TurnId},
    };
    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use nanocodex::{AgentEvent, AgentEventKind};
    use ratatui::{Terminal, backend::TestBackend};
    use serde_json::{json, value::to_raw_value};
    use std::sync::Arc;

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
        Arc::new(TranscriptRecord::from_agent(
            sequence,
            sequence,
            AgentEvent {
                protocol_version: 1,
                request_id: Arc::from("test"),
                seq: sequence,
                kind,
                payload: to_raw_value(&payload).unwrap(),
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
                "result": {"output": output, "exit_code": 0},
                "metadata": null,
            }),
        )));
    }

    fn render(transcript: &mut Transcript, width: u16, height: u16) -> TestBackend {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| transcript.render(frame, frame.area(), &Theme::default()))
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

        transcript.focus_tools();
        let focused = render(&mut transcript, 60, 8);
        assert!(
            focused
                .buffer()
                .content()
                .iter()
                .any(|cell| cell.symbol() == "›")
        );

        transcript.update(TranscriptEvent::Tool(ToolCommand::Toggle));
        let expanded = render(&mut transcript, 60, 8);
        let expanded = expanded
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(expanded.contains("▼"));
        assert!(expanded.contains("all tests passed"));

        transcript.update(TranscriptEvent::BlurTools);
        assert!(!transcript.tools_focused());
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
        let row = transcript.tool_hits[0].row;
        let event = Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 10,
            row,
            modifiers: KeyModifiers::NONE,
        });
        let command = transcript.tool_command(&event).unwrap();

        transcript.update(TranscriptEvent::Tool(command));
        let backend = render(&mut transcript, 60, 8);
        let rendered = backend
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(transcript.tools_focused());
        assert!(rendered.contains("▼"));
        assert!(rendered.contains("done"));
        assert!(rendered.contains("↑↓ tool · Enter toggle · Esc back"));
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
        transcript.focus_tools();
        drop(render(&mut transcript, 60, 10));
        let before = transcript.tool_hits[0].row;

        transcript.update(TranscriptEvent::Tool(ToolCommand::Toggle));
        drop(render(&mut transcript, 60, 10));

        assert_eq!(transcript.tool_hits[0].row, before);
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
