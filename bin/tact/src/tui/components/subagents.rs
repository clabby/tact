//! Camera-centered subagent hierarchy and read-only transcript inspector.
//!
//! [`SubagentTree`] owns the roster of delegated agents and one transcript per agent. Agent
//! lifecycle updates, transcript records, and directed messages arrive through
//! [`SubagentTree::apply`]; terminal input arrives through the tree and transcript update methods,
//! which answer with [`SubagentEffect`]s for the owner to dismiss, inspect, go back, open a link,
//! or persist a new concurrency limit.
//!
//! The tree shows active agents by default and every agent after the filter is toggled. Agents
//! whose parent is hidden are promoted to roots. Exactly one visible agent is focused while any
//! agent is visible, and the camera eases toward the focused node; finishing the animation
//! settles it on the latest focus. A directed message is projected once into the transcript of
//! each agent participant, from that participant's perspective.

use super::{
    clock::unix_time_ms,
    fit::ellipsize,
    floating::{Floating, KeyBinding},
    node::Component,
    subagent_tree_layout::{
        LayoutNode, NODE_HEIGHT, NODE_WIDTH, NodePosition, TreeLayout, VERTICAL_GAP, WorldPoint,
    },
    transcript::{Transcript, TranscriptEvent},
};
use crate::{
    app::{config::DEFAULT_MAX_SUBAGENTS, model, theme::Theme},
    core::transcript::TranscriptRecord,
    tui::format::sanitize_terminal_text_inline,
};
use crossterm::event::{Event, KeyCode, KeyEventKind};
use nanocodex::{ReasoningMode, agent::events::AgentEvent};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph, Wrap},
};
use std::{
    collections::HashMap,
    num::NonZeroU16,
    sync::Arc,
    time::{Duration, Instant},
};
use tact_subagents::{
    AgentId, AgentStatus, AgentUpdate, MessageSender, SubagentNode, SubagentRoster,
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const TREE_KEYS: [(&str, &str); 7] = [
    ("←/→", "row"),
    ("↑", "parent"),
    ("↓", "child"),
    ("enter", "inspect"),
    ("-/+", "limit"),
    ("f", "filter"),
    ("esc", "close"),
];
const TRANSCRIPT_KEYS: [(&str, &str); 4] = [
    ("pgup/pgdn", "scroll"),
    ("ctrl+home/end", ""),
    ("ctrl+o", "expand all"),
    ("esc", "back"),
];
const FOCUSED_ENTRY_KEYS: [(&str, &str); 3] = [
    ("↑↓", "item"),
    ("enter", "toggle"),
    ("esc", "blur, then back"),
];
const CAMERA_FRAME_INTERVAL: Duration = Duration::from_millis(16);
const CAMERA_MIN_DURATION: Duration = Duration::from_millis(120);
const CAMERA_MAX_DURATION: Duration = Duration::from_millis(240);
const INSPECTOR_HEIGHT: u16 = 6;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AgentFilter {
    Active,
    All,
}

impl AgentFilter {
    const fn includes(self, status: &AgentStatus) -> bool {
        match self {
            Self::Active => status.is_active(),
            Self::All => true,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::All => "all",
        }
    }

    const fn helper(self) -> &'static str {
        match self {
            Self::Active => "filter: active",
            Self::All => "filter: all",
        }
    }

    const fn toggled(self) -> Self {
        match self {
            Self::Active => Self::All,
            Self::All => Self::Active,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SubagentOverlay {
    Tree,
    Transcript(AgentId),
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum SubagentEffect {
    Dismiss,
    Inspect(AgentId),
    Back,
    OpenLink(String),
    SetMaxSubagents(usize),
}

struct CameraAnimation {
    from: WorldPoint,
    to: WorldPoint,
    started_at: Instant,
    duration: Duration,
    next_frame: Instant,
}

#[derive(Default)]
struct Camera {
    center: Option<WorldPoint>,
    animation: Option<CameraAnimation>,
}

pub(super) struct SubagentTree {
    roster: SubagentRoster,
    transcripts: HashMap<AgentId, Transcript>,
    focused: Option<AgentId>,
    remembered_children: HashMap<AgentId, AgentId>,
    camera: Camera,
    filter: AgentFilter,
    effort: crate::app::config::ReasoningEffort,
    workspace: std::path::PathBuf,
}

impl SubagentTree {
    pub(super) fn new(effort: crate::app::config::ReasoningEffort) -> Self {
        Self {
            roster: SubagentRoster::new(DEFAULT_MAX_SUBAGENTS),
            transcripts: HashMap::new(),
            focused: None,
            remembered_children: HashMap::new(),
            camera: Camera::default(),
            filter: AgentFilter::Active,
            effort,
            workspace: std::env::current_dir().unwrap_or_default(),
        }
    }

    pub(super) fn set_workspace(&mut self, workspace: &std::path::Path) {
        self.workspace = workspace.to_path_buf();
        for transcript in self.transcripts.values_mut() {
            transcript.set_workspace(workspace);
        }
    }

    pub(super) fn refresh_terminal_images(&mut self) {
        for transcript in self.transcripts.values_mut() {
            transcript.refresh_terminal_images();
        }
    }

    /// The tree as every front-end presents it.
    pub(super) const fn roster(&self) -> &SubagentRoster {
        &self.roster
    }

    pub(super) fn apply(&mut self, update: AgentUpdate) -> bool {
        let roster_changed = self.roster.apply(&update);
        match update {
            AgentUpdate::Added(descriptor) => {
                let id = descriptor.id;
                self.transcripts.entry(id).or_insert_with(|| {
                    let mut transcript = Transcript::with_effort(self.effort);
                    transcript.set_workspace(&self.workspace);
                    transcript
                });
                self.focused.get_or_insert(id);
                roster_changed
            }
            AgentUpdate::Event { id, event } => self.apply_record(id, subagent_record(event)),
            AgentUpdate::Status { .. } => roster_changed,
            AgentUpdate::Message(update) => {
                let mut projected = false;
                let mut previous = None;
                for participant in update.thread.participants {
                    let MessageSender::Agent { agent_id } = participant else {
                        continue;
                    };
                    if previous == Some(agent_id) {
                        continue;
                    }
                    previous = Some(agent_id);
                    let Some(transcript) = self.transcripts.get_mut(&agent_id) else {
                        continue;
                    };
                    transcript.update(TranscriptEvent::DirectedMessage {
                        perspective: participant,
                        update: update.clone(),
                    });
                    projected = true;
                }
                projected
            }
        }
    }

    /// Appends a record to an agent's transcript and reports whether the agent is known.
    pub(super) fn apply_record(&mut self, id: AgentId, record: Arc<TranscriptRecord>) -> bool {
        let Some(transcript) = self.transcripts.get_mut(&id) else {
            return false;
        };
        transcript.update(TranscriptEvent::Record(record));
        true
    }

    pub(super) fn active_count(&self) -> usize {
        self.roster.active_count()
    }

    pub(super) fn set_effort(&mut self, effort: crate::app::config::ReasoningEffort) {
        self.effort = effort;
        for transcript in self.transcripts.values_mut() {
            transcript.set_effort(effort);
        }
    }

    pub(super) fn set_max_subagents(&mut self, limit: usize) {
        self.roster.max_subagents = limit;
    }

    pub(super) const fn max_subagents(&self) -> usize {
        self.roster.max_subagents
    }

    pub(super) fn contains(&self, id: AgentId) -> bool {
        self.roster.agent(id).is_some()
    }

    pub(super) fn is_direct_child(&self, id: AgentId) -> bool {
        self.roster
            .agent(id)
            .is_some_and(|node| node.parent.is_none())
    }

    pub(super) fn animation_deadline(&self) -> Option<Instant> {
        self.transcripts
            .values()
            .filter_map(|transcript| transcript.animation_deadline())
            .chain(
                self.camera
                    .animation
                    .as_ref()
                    .map(|animation| animation.next_frame),
            )
            .min()
    }

    pub(super) fn advance(&mut self, now: Instant) -> bool {
        let camera_changed = self.advance_camera(now);
        self.transcripts
            .values_mut()
            .fold(camera_changed, |changed, transcript| {
                let node_changed = transcript
                    .update(TranscriptEvent::AnimationFrame(now))
                    .render
                    != super::node::RenderRequest::None;
                changed || node_changed
            })
    }

    pub(super) fn finish_camera_animation(&mut self) {
        let Some(animation) = self.camera.animation.take() else {
            return;
        };
        self.camera.center = Some(animation.to);
    }

    pub(super) fn open_tree(&mut self) {
        self.filter = AgentFilter::Active;
        let layout = self.layout();
        self.focus_oldest(&layout);
    }

    pub(super) fn update_tree(&mut self, event: Event) -> Option<SubagentEffect> {
        self.update_tree_at(event, Instant::now())
    }

    fn update_tree_at(&mut self, event: Event, now: Instant) -> Option<SubagentEffect> {
        let Event::Key(key) = event else {
            return None;
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return None;
        }

        match key.code {
            KeyCode::Esc => Some(SubagentEffect::Dismiss),
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_focus(Direction::Parent, now);
                None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_focus(Direction::Child, now);
                None
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.move_focus(Direction::PreviousOnLevel, now);
                None
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.move_focus(Direction::NextOnLevel, now);
                None
            }
            KeyCode::Home => {
                self.move_focus(Direction::Root, now);
                None
            }
            KeyCode::Char('f') if key.modifiers.is_empty() => {
                self.filter = self.filter.toggled();
                let layout = self.layout();
                self.focus_oldest(&layout);
                None
            }
            KeyCode::Char('-') if key.modifiers.is_empty() => {
                self.roster.max_subagents = self.roster.max_subagents.saturating_sub(1);
                Some(SubagentEffect::SetMaxSubagents(self.roster.max_subagents))
            }
            KeyCode::Char('+') | KeyCode::Char('=') if key.modifiers.is_empty() => {
                self.roster.max_subagents = self.roster.max_subagents.saturating_add(1);
                Some(SubagentEffect::SetMaxSubagents(self.roster.max_subagents))
            }
            KeyCode::Enter => self.focused.map(SubagentEffect::Inspect),
            _ => None,
        }
    }

    pub(super) fn update_transcript(
        &mut self,
        id: AgentId,
        event: Event,
        mouse_scroll_lines: NonZeroU16,
    ) -> Option<SubagentEffect> {
        if matches!(
            &event,
            Event::Key(key)
                if key.code == KeyCode::Esc
                    && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        ) {
            let Some(transcript) = self.transcripts.get_mut(&id) else {
                return Some(SubagentEffect::Back);
            };
            if transcript.expandables_focused() {
                transcript.update(TranscriptEvent::BlurExpandables);
                return None;
            }
            return Some(SubagentEffect::Back);
        }
        let Some(transcript) = self.transcripts.get_mut(&id) else {
            return Some(SubagentEffect::Back);
        };
        if let Some(destination) = transcript.link_destination(&event) {
            return Some(SubagentEffect::OpenLink(destination.to_string()));
        }
        if let Some(command) = transcript.scroll_command(&event, mouse_scroll_lines) {
            transcript.update(TranscriptEvent::Scroll(command));
        } else if let Some(command) = transcript.expandable_command(&event) {
            transcript.update(TranscriptEvent::Expandable(command));
        }
        None
    }

    pub(super) fn toggle_expand_all(&mut self, id: AgentId) -> bool {
        let Some(transcript) = self.transcripts.get_mut(&id) else {
            return false;
        };
        transcript.update(TranscriptEvent::ToggleExpandAll);
        true
    }

    fn tree_keys(&self) -> [KeyBinding; 7] {
        let mut keys = TREE_KEYS;
        keys[5].1 = self.filter.helper();
        keys
    }

    fn transcript_keys(&self, id: AgentId) -> &'static [KeyBinding] {
        match self
            .transcripts
            .get(&id)
            .is_some_and(Transcript::expandables_focused)
        {
            true => &FOCUSED_ENTRY_KEYS,
            false => &TRANSCRIPT_KEYS,
        }
    }

    pub(super) fn render_tree(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let keys = self.tree_keys();
        let layout = Floating::new("Sub-agent tree", area.width, area.height, &keys)
            .render(frame, area, theme);
        if layout.body.is_empty() {
            return;
        }

        let tree_layout = self.layout();
        self.ensure_focus(&tree_layout);
        let Some(focused) = self.focused else {
            let message = if self.roster.agents.is_empty() {
                format!(
                    "Concurrency: {} / {} active. No subagents have been delegated yet.",
                    self.active_count(),
                    self.roster.max_subagents
                )
            } else {
                format!(
                    "Concurrency: {} / {} active. No subagents are currently running. Press f to show all.",
                    self.active_count(),
                    self.roster.max_subagents
                )
            };
            frame.render_widget(
                Paragraph::new(message)
                    .style(Style::default().fg(theme.muted()))
                    .wrap(Wrap { trim: true }),
                inset(layout.body, 2, 1),
            );
            return;
        };

        let (canvas, inspector) = split_inspector(layout.body);
        if canvas.is_empty() {
            return;
        }
        let focus_center = tree_layout
            .center(focused)
            .expect("focused agent should have a layout position");
        self.sync_camera_target(focus_center, Instant::now());
        let camera_center = self.camera.center.unwrap_or(focus_center);

        render_edges(frame, canvas, theme, &tree_layout, camera_center);
        for (id, position) in tree_layout.positioned_nodes() {
            let Some(node) = self.roster.agent(id) else {
                continue;
            };
            render_node(
                frame,
                canvas,
                theme,
                camera_center,
                NodeRender {
                    node,
                    position,
                    focused: id == focused,
                    child_count: tree_layout.children(id).len(),
                },
            );
        }
        self.render_inspector(frame, inspector, theme, focused, &tree_layout);
    }

    pub(super) fn render_transcript(
        &mut self,
        id: AgentId,
        frame: &mut Frame<'_>,
        area: Rect,
        theme: &Theme,
    ) {
        let keys = self.transcript_keys(id);
        let (Some(node), Some(transcript)) = (self.roster.agent(id), self.transcripts.get_mut(&id))
        else {
            return;
        };
        let title = transcript_title(node);
        let layout = Floating::new(&title, area.width, area.height, keys)
            .colors(theme.border(), theme.model(node.model))
            .render(frame, area, theme);
        transcript.render(frame, layout.body, theme);
    }

    fn layout(&self) -> TreeLayout {
        let visible = self.visible_ids();
        let nodes = self
            .roster
            .agents
            .iter()
            .filter(|node| visible.contains(&node.id))
            .map(|node| LayoutNode {
                id: node.id,
                parent: node.parent,
            })
            .collect::<Vec<_>>();
        TreeLayout::new(&nodes)
    }

    fn visible_ids(&self) -> Vec<AgentId> {
        self.roster
            .agents
            .iter()
            .filter(|node| self.filter.includes(&node.status))
            .map(|node| node.id)
            .collect()
    }

    fn ensure_focus(&mut self, layout: &TreeLayout) {
        if self
            .focused
            .is_some_and(|focused| layout.position(focused).is_some())
        {
            return;
        }
        self.focused = layout.roots().first().copied();
        self.camera.center = self.focused.and_then(|id| layout.center(id));
        self.camera.animation = None;
    }

    fn focus_oldest(&mut self, layout: &TreeLayout) {
        self.focused = self
            .roster
            .agents
            .iter()
            .filter(|node| self.filter.includes(&node.status))
            .map(|node| node.id)
            .filter(|id| layout.position(*id).is_some())
            .min();
        self.camera.center = self.focused.and_then(|id| layout.center(id));
        self.camera.animation = None;
    }

    fn move_focus(&mut self, direction: Direction, now: Instant) {
        let layout = self.layout();
        self.ensure_focus(&layout);
        let Some(current) = self.focused else {
            return;
        };

        let target = match direction {
            Direction::Parent => layout.parent(current),
            Direction::Child => {
                let children = layout.children(current);
                self.remembered_children
                    .get(&current)
                    .copied()
                    .filter(|child| children.contains(child))
                    .or_else(|| {
                        let parent_x = layout.position(current)?.center_x;
                        children.iter().copied().min_by_key(|child| {
                            layout
                                .position(*child)
                                .map_or(i32::MAX, |position| (position.center_x - parent_x).abs())
                        })
                    })
            }
            Direction::PreviousOnLevel => {
                previous_or_next_on_level(&layout, current, HorizontalDirection::Previous)
            }
            Direction::NextOnLevel => {
                previous_or_next_on_level(&layout, current, HorizontalDirection::Next)
            }
            Direction::Root => layout.roots().first().copied(),
        };
        let Some(target) = target else {
            return;
        };

        if let Some(parent) = layout.parent(target) {
            self.remembered_children.insert(parent, target);
        }
        if direction == Direction::Parent {
            self.remembered_children.insert(target, current);
        }
        self.focused = Some(target);
        self.recenter_on_focus(&layout, now);
    }

    fn recenter_on_focus(&mut self, layout: &TreeLayout, now: Instant) {
        self.advance_camera(now);
        let Some(target) = self.focused.and_then(|id| layout.center(id)) else {
            return;
        };
        self.start_camera_animation(target, now);
    }

    fn sync_camera_target(&mut self, target: WorldPoint, now: Instant) {
        if self
            .camera
            .animation
            .as_ref()
            .is_some_and(|animation| distance(animation.to, target) < f64::EPSILON)
        {
            return;
        }
        if self
            .camera
            .center
            .is_some_and(|center| distance(center, target) < f64::EPSILON)
        {
            return;
        }
        self.advance_camera(now);
        self.start_camera_animation(target, now);
    }

    fn start_camera_animation(&mut self, target: WorldPoint, now: Instant) {
        let Some(from) = self.camera.center else {
            self.camera.center = Some(target);
            return;
        };
        if distance(from, target) < f64::EPSILON {
            self.camera.animation = None;
            return;
        }

        let duration_ms = (CAMERA_MIN_DURATION.as_millis() as f64 + distance(from, target))
            .min(CAMERA_MAX_DURATION.as_millis() as f64);
        let duration = Duration::from_millis(duration_ms.round() as u64);
        self.camera.animation = Some(CameraAnimation {
            from,
            to: target,
            started_at: now,
            duration,
            next_frame: now + CAMERA_FRAME_INTERVAL,
        });
    }

    fn advance_camera(&mut self, now: Instant) -> bool {
        let Some(animation) = &mut self.camera.animation else {
            return false;
        };
        if now < animation.next_frame {
            return false;
        }
        let elapsed = now.saturating_duration_since(animation.started_at);
        let progress = (elapsed.as_secs_f64() / animation.duration.as_secs_f64()).min(1.0);
        let eased = 1.0 - (1.0 - progress).powi(3);
        self.camera.center = Some(WorldPoint {
            x: animation.from.x + (animation.to.x - animation.from.x) * eased,
            y: animation.from.y + (animation.to.y - animation.from.y) * eased,
        });

        if progress >= 1.0 {
            self.camera.center = Some(animation.to);
            self.camera.animation = None;
        } else {
            animation.next_frame = now + CAMERA_FRAME_INTERVAL;
        }
        true
    }

    fn render_inspector(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        theme: &Theme,
        focused: AgentId,
        layout: &TreeLayout,
    ) {
        if area.is_empty() {
            return;
        }
        let Some(node) = self.roster.agent(focused) else {
            return;
        };
        let (symbol, color, status) = state_style(&node.status);
        let title = format!(" {symbol} #{} · {} · {status} ", focused, node.role);
        let block = Block::new()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(color))
            .title(title)
            .title_style(Style::default().fg(color).add_modifier(Modifier::BOLD));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.is_empty() {
            return;
        }

        let parent = layout
            .parent(focused)
            .map_or_else(|| "root".to_owned(), |id| format!("parent #{id}"));
        let children = layout.children(focused).len();
        let task = ellipsize(
            sanitize_terminal_text_inline(&node.task),
            usize::from(inner.width.saturating_sub(6)),
        )
        .into_owned();
        let lines = vec![
            Line::from(vec![
                Span::styled("Task  ", Style::default().fg(theme.muted())),
                Span::styled(task, Style::default().fg(theme.text())),
            ]),
            Line::from(vec![
                Span::styled("Tree  ", Style::default().fg(theme.muted())),
                Span::raw(format!("{parent} · {children} children")),
                Span::styled(
                    format!(
                        "    Concurrency  {} / {} active",
                        self.active_count(),
                        self.roster.max_subagents
                    ),
                    Style::default().fg(theme.muted()),
                ),
            ]),
            Line::from(vec![
                Span::styled("View  ", Style::default().fg(theme.muted())),
                Span::raw(format!(
                    "{} agents · {} filter",
                    self.roster.agents.len(),
                    self.filter.label()
                )),
                Span::styled("    Model  ", Style::default().fg(theme.muted())),
                Span::styled(
                    model_label(node),
                    Style::default()
                        .fg(theme.model(node.model))
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                Span::styled("Session  ", Style::default().fg(theme.muted())),
                Span::raw(
                    ellipsize(
                        sanitize_terminal_text_inline(&node.session_id),
                        usize::from(inner.width.saturating_sub(9)),
                    )
                    .into_owned(),
                ),
            ]),
        ];
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

fn model_label(node: &SubagentNode) -> String {
    let pro = match node.reasoning_mode {
        ReasoningMode::Pro => " pro",
        ReasoningMode::Standard => "",
    };
    format!("{} ({}{pro})", model::name(node.model), node.thinking)
}

fn transcript_title(node: &SubagentNode) -> String {
    format!("{} · {} · #{}", node.role, model_label(node), node.id)
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Direction {
    Parent,
    Child,
    PreviousOnLevel,
    NextOnLevel,
    Root,
}

#[derive(Clone, Copy)]
enum HorizontalDirection {
    Previous,
    Next,
}

fn previous_or_next_on_level(
    layout: &TreeLayout,
    current: AgentId,
    direction: HorizontalDirection,
) -> Option<AgentId> {
    let current_position = layout.position(current)?;
    let mut level = layout
        .positioned_nodes()
        .filter(|(_, position)| position.top == current_position.top)
        .collect::<Vec<_>>();
    level.sort_unstable_by_key(|(id, position)| (position.center_x, *id));
    let index = level.iter().position(|&(id, _)| id == current)?;
    match direction {
        HorizontalDirection::Previous => index.checked_sub(1).map(|index| level[index].0),
        HorizontalDirection::Next => level.get(index + 1).map(|(id, _)| *id),
    }
}

fn split_inspector(area: Rect) -> (Rect, Rect) {
    if area.height <= 4 {
        return (area, Rect::default());
    }
    let inspector_height = INSPECTOR_HEIGHT.min(area.height.saturating_sub(3));
    let canvas = Rect {
        height: area.height - inspector_height,
        ..area
    };
    let inspector = Rect {
        y: canvas.bottom(),
        height: inspector_height,
        ..area
    };
    (canvas, inspector)
}

struct NodeRender<'a> {
    node: &'a SubagentNode,
    position: NodePosition,
    focused: bool,
    child_count: usize,
}

fn render_node(
    frame: &mut Frame<'_>,
    canvas: Rect,
    theme: &Theme,
    camera: WorldPoint,
    render: NodeRender<'_>,
) {
    let NodeRender {
        node,
        position,
        focused,
        child_count,
    } = render;
    let left = position.center_x - NODE_WIDTH / 2;
    let border_style = if focused {
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.border())
    };
    let text_style = Style::default().fg(theme.text());
    let (symbol, status_color, status) = state_style(&node.status);
    let detail_style = Style::default().fg(status_color);
    let role_width = u16::try_from(NODE_WIDTH.saturating_sub(4)).unwrap_or_default();
    let role = ellipsize(
        sanitize_terminal_text_inline(&node.role),
        usize::from(role_width),
    );
    let title = centered_text(&format!("{symbol} #{} {role}", node.id), NODE_WIDTH - 2);
    let detail = centered_text(
        &format!("{status} · {child_count} children"),
        NODE_WIDTH - 2,
    );
    let top = format!(
        "╭{}╮",
        "─".repeat(usize::try_from(NODE_WIDTH - 2).unwrap_or_default())
    );
    let bottom = format!(
        "╰{}╯",
        "─".repeat(usize::try_from(NODE_WIDTH - 2).unwrap_or_default())
    );
    draw_world_string(
        frame,
        canvas,
        left,
        position.top,
        camera,
        &top,
        border_style,
    );
    for (row, (text, style)) in [(title, text_style), (detail, detail_style)]
        .into_iter()
        .enumerate()
    {
        let y = position.top + i32::try_from(row).unwrap_or_default() + 1;
        draw_world_string(frame, canvas, left, y, camera, "│", border_style);
        draw_world_string(frame, canvas, left + 1, y, camera, &text, style);
        draw_world_string(
            frame,
            canvas,
            left + NODE_WIDTH - 1,
            y,
            camera,
            "│",
            border_style,
        );
    }
    draw_world_string(
        frame,
        canvas,
        left,
        position.top + NODE_HEIGHT - 1,
        camera,
        &bottom,
        border_style,
    );
}

fn render_edges(
    frame: &mut Frame<'_>,
    canvas: Rect,
    theme: &Theme,
    layout: &TreeLayout,
    camera: WorldPoint,
) {
    let mut cells = HashMap::<(i32, i32), u8>::new();
    let mut arrows = Vec::new();
    for (parent, position) in layout.positioned_nodes() {
        let children = layout.children(parent);
        if children.is_empty() {
            continue;
        }
        let child_centers = children
            .iter()
            .filter_map(|&id| layout.position(id).map(|position| position.center_x))
            .collect::<Vec<_>>();
        if child_centers.is_empty() {
            continue;
        }

        let start_y = position.top + NODE_HEIGHT;
        let junction_y = start_y + VERTICAL_GAP / 2;
        if child_centers.len() == 1 {
            let child_top = layout.position(children[0]).unwrap().top;
            add_vertical(&mut cells, position.center_x, start_y, child_top - 2);
            arrows.push((child_centers[0], child_top - 1));
            continue;
        }
        add_vertical(&mut cells, position.center_x, start_y, junction_y);
        let first = child_centers[0].min(position.center_x);
        let last = child_centers[child_centers.len() - 1].max(position.center_x);
        add_horizontal(&mut cells, first, last, junction_y);
        for (index, child_x) in child_centers.into_iter().enumerate() {
            let child_top = layout.position(children[index]).unwrap().top;
            add_vertical(&mut cells, child_x, junction_y, child_top - 2);
            arrows.push((child_x, child_top - 1));
        }
    }

    let style = Style::default().fg(theme.border());
    for ((x, y), connections) in cells {
        draw_world_string(frame, canvas, x, y, camera, edge_symbol(connections), style);
    }
    for (x, y) in arrows {
        draw_world_string(frame, canvas, x, y, camera, "↓", style);
    }
}

const UP: u8 = 1;
const RIGHT: u8 = 2;
const DOWN: u8 = 4;
const LEFT: u8 = 8;

fn add_vertical(cells: &mut HashMap<(i32, i32), u8>, x: i32, start: i32, end: i32) {
    if start > end {
        return;
    }
    for y in start..=end {
        let mut connections = 0;
        if y > start {
            connections |= UP;
        }
        if y < end {
            connections |= DOWN;
        }
        if connections == 0 {
            connections = UP | DOWN;
        }
        *cells.entry((x, y)).or_default() |= connections;
    }
}

fn add_horizontal(cells: &mut HashMap<(i32, i32), u8>, start: i32, end: i32, y: i32) {
    if start > end {
        return;
    }
    for x in start..=end {
        let mut connections = 0;
        if x > start {
            connections |= LEFT;
        }
        if x < end {
            connections |= RIGHT;
        }
        if connections == 0 {
            connections = LEFT | RIGHT;
        }
        *cells.entry((x, y)).or_default() |= connections;
    }
}

const fn edge_symbol(connections: u8) -> &'static str {
    match connections {
        5 => "│",
        10 => "─",
        6 => "╭",
        12 => "╮",
        3 => "╰",
        9 => "╯",
        7 => "├",
        13 => "┤",
        14 => "┬",
        11 => "┴",
        15 => "┼",
        _ if connections & (LEFT | RIGHT) != 0 => "─",
        _ => "│",
    }
}

fn draw_world_string(
    frame: &mut Frame<'_>,
    canvas: Rect,
    world_x: i32,
    world_y: i32,
    camera: WorldPoint,
    text: &str,
    style: Style,
) {
    let screen_x = i32::from(canvas.x)
        + i32::from(canvas.width) / 2
        + (f64::from(world_x) - camera.x).round() as i32;
    let screen_y = i32::from(canvas.y)
        + i32::from(canvas.height) / 2
        + (f64::from(world_y) - camera.y).round() as i32;
    if screen_y < i32::from(canvas.y) || screen_y >= i32::from(canvas.bottom()) {
        return;
    }

    let text = sanitize_terminal_text_inline(text);
    let mut x = screen_x;
    for grapheme in text.graphemes(true) {
        let width = i32::try_from(UnicodeWidthStr::width(grapheme)).unwrap_or(i32::MAX);
        if x >= i32::from(canvas.x) && x.saturating_add(width) <= i32::from(canvas.right()) {
            let position = (
                u16::try_from(x).unwrap_or_default(),
                u16::try_from(screen_y).unwrap_or_default(),
            );
            frame.buffer_mut()[position]
                .set_symbol(grapheme)
                .set_style(style);
        }
        x = x.saturating_add(width);
    }
}

fn centered_text(text: &str, width: i32) -> String {
    let width = u16::try_from(width).unwrap_or_default();
    let text = ellipsize(sanitize_terminal_text_inline(text), usize::from(width));
    let text_width = u16::try_from(UnicodeWidthStr::width(text.as_ref())).unwrap_or(u16::MAX);
    let padding = width.saturating_sub(text_width);
    let left = padding / 2;
    let right = padding - left;
    format!(
        "{}{text}{}",
        " ".repeat(usize::from(left)),
        " ".repeat(usize::from(right))
    )
}

const fn state_style(status: &AgentStatus) -> (&'static str, Color, &'static str) {
    match status {
        AgentStatus::Pending => ("○", Color::Yellow, "pending"),
        AgentStatus::Running => ("◐", Color::Yellow, "running"),
        AgentStatus::Completed { .. } => ("●", Color::Green, "completed"),
        AgentStatus::Interrupted => ("■", Color::Blue, "interrupted"),
        AgentStatus::Failed { .. } => ("×", Color::Red, "failed"),
        AgentStatus::Closing => ("◑", Color::Yellow, "closing"),
        AgentStatus::Closed => ("■", Color::DarkGray, "closed"),
    }
}

fn inset(area: Rect, horizontal: u16, vertical: u16) -> Rect {
    Rect::new(
        area.x.saturating_add(horizontal),
        area.y.saturating_add(vertical),
        area.width.saturating_sub(horizontal.saturating_mul(2)),
        area.height.saturating_sub(vertical.saturating_mul(2)),
    )
}

fn distance(from: WorldPoint, to: WorldPoint) -> f64 {
    (to.x - from.x).hypot(to.y - from.y)
}

/// The transcript record of one subagent event, stamped with its arrival time.
pub(super) fn subagent_record(event: AgentEvent) -> Arc<TranscriptRecord> {
    Arc::new(TranscriptRecord::from_agent(
        event.seq,
        unix_time_ms(),
        event,
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        AgentFilter, DOWN, FOCUSED_ENTRY_KEYS, LEFT, RIGHT, SubagentEffect, SubagentTree,
        TRANSCRIPT_KEYS, UP, edge_symbol, transcript_title,
    };
    use crate::app::{config::ReasoningEffort, theme::Theme};
    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use nanocodex::{
        HarnessModel as Model, Model as CodexModel, ReasoningMode, Thinking,
        agent::events::{AgentEvent, AgentEventKind},
    };
    use ratatui::{Terminal, backend::TestBackend, style::Color};
    use serde_json::{json, value::to_raw_value};
    use std::{
        num::NonZeroU16,
        sync::Arc,
        time::{Duration, Instant},
    };
    use tact_subagents::{AgentDescriptor, AgentId, AgentMessageUpdate, AgentStatus, AgentUpdate};

    const SCROLL_LINES: NonZeroU16 = NonZeroU16::new(3).unwrap();

    /// The default agent: a running root researcher on Luna with low thinking.
    fn descriptor() -> AgentDescriptor {
        AgentDescriptor {
            session_id: "child-session".to_owned(),
            model: Model::Codex(CodexModel::Luna),
            thinking: Thinking::Low,
            task: "Trace the event lifecycle".to_owned(),
            ..agent(1, None, "researcher")
        }
    }

    fn second_descriptor() -> AgentDescriptor {
        AgentDescriptor {
            session_id: "second-session".to_owned(),
            thinking: Thinking::High,
            task: "Verify the event ordering".to_owned(),
            ..agent(2, None, "reviewer")
        }
    }

    fn agent(id: u64, parent: Option<u64>, role: &str) -> AgentDescriptor {
        AgentDescriptor {
            id: AgentId::new(id),
            session_id: format!("agent-{id}"),
            model: Model::Codex(CodexModel::Sol),
            thinking: Thinking::Medium,
            reasoning_mode: ReasoningMode::Standard,
            role: role.to_owned(),
            task: format!("Task for {role}"),
            parent: parent.map(AgentId::new),
        }
    }

    fn tree_of(agents: impl IntoIterator<Item = AgentDescriptor>) -> SubagentTree {
        let mut tree = SubagentTree::new(ReasoningEffort::Medium);
        for agent in agents {
            assert!(tree.apply(AgentUpdate::Added(agent)));
        }
        tree
    }

    fn completed(id: u64) -> AgentUpdate {
        AgentUpdate::Status {
            id: AgentId::new(id),
            status: AgentStatus::Completed {
                output: json!({ "report": "done" }),
            },
        }
    }

    fn event(kind: AgentEventKind, payload: serde_json::Value) -> AgentUpdate {
        AgentUpdate::Event {
            id: AgentId::new(1),
            event: AgentEvent {
                protocol_version: 1,
                request_id: Arc::from("child-session"),
                seq: 1,
                kind,
                payload: to_raw_value(&payload).unwrap().into(),
            },
        }
    }

    fn message_update(reply: bool) -> AgentUpdate {
        let question = json!({
            "id": 1,
            "thread_id": 1,
            "from": {"kind": "agent", "agent_id": 1},
            "to": 2,
            "priority": "deferred",
            "purpose": "question",
            "body": "Can you verify the event ordering?"
        });
        let answer = json!({
            "id": 2,
            "thread_id": 1,
            "from": {"kind": "agent", "agent_id": 2},
            "to": 1,
            "priority": "deferred",
            "purpose": "reply",
            "in_reply_to": 1,
            "body": "Verified: delivery precedes projection."
        });
        let (message_id, messages) = match reply {
            true => (2, json!([question, answer])),
            false => (1, json!([question])),
        };
        AgentUpdate::Message(
            serde_json::from_value::<AgentMessageUpdate>(json!({
                "message_id": message_id,
                "thread": {
                    "id": 1,
                    "participants": [
                        {"kind": "agent", "agent_id": 1},
                        {"kind": "agent", "agent_id": 2}
                    ],
                    "messages": messages
                },
                "delivery": {"state": "delivered", "disposition": "started"}
            }))
            .unwrap(),
        )
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn render_tree(tree: &mut SubagentTree, width: u16, height: u16) -> TestBackend {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| tree.render_tree(frame, frame.area(), &Theme::default()))
            .unwrap();
        terminal.backend().clone()
    }

    fn render_transcript(tree: &mut SubagentTree, id: u64) -> TestBackend {
        let mut terminal = Terminal::new(TestBackend::new(72, 24)).unwrap();
        terminal
            .draw(|frame| {
                tree.render_transcript(AgentId::new(id), frame, frame.area(), &Theme::default());
            })
            .unwrap();
        terminal.backend().clone()
    }

    fn rows(backend: &TestBackend) -> Vec<String> {
        let buffer = backend.buffer();
        buffer
            .content
            .chunks(usize::from(buffer.area.width))
            .map(|row| row.iter().map(|cell| cell.symbol()).collect())
            .collect()
    }

    fn cells_with_symbol(backend: &TestBackend, symbol: &str) -> Vec<(u16, u16)> {
        let area = backend.buffer().area;
        (0..area.height)
            .flat_map(|y| (0..area.width).map(move |x| (x, y)))
            .filter(|&position| backend.buffer()[position].symbol() == symbol)
            .collect()
    }

    /// Edge arrows on the tree canvas, which use the border color unlike footer key glyphs.
    fn edge_arrows(backend: &TestBackend) -> Vec<(u16, u16)> {
        cells_with_symbol(backend, "↓")
            .into_iter()
            .filter(|&position| backend.buffer()[position].fg == Theme::default().border())
            .collect()
    }

    /// Clicks the only collapsed or expanded entry marker in an agent's transcript.
    fn click_entry(tree: &mut SubagentTree, id: u64, marker: &str) {
        let [(_, row)] = cells_with_symbol(&render_transcript(tree, id), marker)[..] else {
            panic!("transcript should render exactly one {marker} marker");
        };
        tree.update_transcript(
            AgentId::new(id),
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 20,
                row,
                modifiers: KeyModifiers::NONE,
            }),
            SCROLL_LINES,
        );
    }

    fn focus_tool(tree: &mut SubagentTree) {
        tree.apply(event(
            AgentEventKind::ToolCall,
            json!({
                "call_id": "tool-1",
                "tool": "exec_command",
                "arguments": {"cmd": "cargo test", "workdir": "/work"},
            }),
        ));
        click_entry(tree, 1, "▶");
        assert!(tree.transcripts[&AgentId::new(1)].expandables_focused());
    }

    #[test]
    fn changing_effort_preserves_active_subagents_and_their_own_thinking() {
        let mut tree = tree_of([descriptor()]);

        tree.set_effort(ReasoningEffort::High);

        assert_eq!(tree.effort, ReasoningEffort::High);
        assert_eq!(tree.active_count(), 1);
        assert!(tree.transcripts.contains_key(&AgentId::new(1)));
        let node = tree.roster.agent(AgentId::new(1)).unwrap();
        assert_eq!(node.thinking, Thinking::Low);
        assert_eq!(transcript_title(node), "researcher · Luna (low) · #1");

        tree.apply(AgentUpdate::Added(AgentDescriptor {
            reasoning_mode: ReasoningMode::Pro,
            ..agent(2, None, "reviewer")
        }));
        let node = tree.roster.agent(AgentId::new(2)).unwrap();
        assert_eq!(transcript_title(node), "reviewer · Sol (medium pro) · #2");
    }

    #[test]
    fn tree_nodes_do_not_write_control_characters_to_terminal_cells() {
        let mut tree = tree_of([AgentDescriptor {
            role: "re\u{1b}\tsearcher".to_owned(),
            ..descriptor()
        }]);

        let backend = render_tree(&mut tree, 100, 40);
        assert!(
            backend
                .buffer()
                .content
                .iter()
                .all(|cell| !cell.symbol().chars().any(char::is_control))
        );
    }

    #[test]
    fn directed_messages_are_upserted_into_both_agent_transcripts() {
        let mut tree = tree_of([descriptor(), second_descriptor()]);

        assert!(tree.apply(message_update(false)));
        assert!(tree.apply(message_update(true)));

        for (id, summary) in [
            (
                1,
                "│  ▶ ← Message  #2 → you · reply · delivered · started · 2 messages ·  │",
            ),
            (
                2,
                "│  ▶ → Message  you → #1 · reply · delivered · started · 2 messages ·  │",
            ),
        ] {
            let backend = render_transcript(&mut tree, id);
            let [(_, row)] = cells_with_symbol(&backend, "▶")[..] else {
                panic!("agent #{id} should show one collapsed thread");
            };
            assert_eq!(rows(&backend)[usize::from(row)], summary);
        }
    }

    #[test]
    fn directed_message_threads_share_inline_focus_and_expansion() {
        let mut tree = tree_of([descriptor(), second_descriptor()]);
        tree.apply(message_update(true));

        click_entry(&mut tree, 1, "▶");

        assert!(tree.transcripts[&AgentId::new(1)].expandables_focused());
        assert_eq!(tree.transcript_keys(AgentId::new(1)), FOCUSED_ENTRY_KEYS);
        let expanded = render_transcript(&mut tree, 1);
        assert!(cells_with_symbol(&expanded, "▶").is_empty());
        assert_eq!(cells_with_symbol(&expanded, "▼").len(), 1);
        assert_eq!(
            rows(&expanded)[14..21],
            [
                "│› ▼ ← Message  #2 → you · reply · delivered · started · 2 messages ·  │",
                "│      Verified: delivery precedes projection.                         │",
                "│    │ you → #2 · question · deferred · pending                        │",
                "│    │ Can you verify the event ordering?                              │",
                "│    │ #2 → you · reply · deferred · delivered · started               │",
                "│    │ Verified: delivery precedes projection.                         │",
                "│    └ thread #1 · 2 messages                                          │",
            ]
        );
    }

    #[test]
    fn concurrency_limit_is_visible_and_editable() {
        let mut tree = SubagentTree::new(ReasoningEffort::Medium);
        tree.set_max_subagents(4);

        assert_eq!(
            rows(&render_tree(&mut tree, 64, 8)),
            [
                "╭─────────────────────── Sub-agent tree ───────────────────────╮",
                "│                                                              │",
                "│  Concurrency: 0 / 4 active. No subagents have been           │",
                "│  delegated yet.                                              │",
                "│                                                              │",
                "│   ←/→ row · ↑ parent · ↓ child · enter inspect · -/+ limit   │",
                "│                 f filter: active · esc close                 │",
                "╰──────────────────────────────────────────────────────────────╯",
            ]
        );
        assert_eq!(
            tree.update_tree(key(KeyCode::Char('-'))),
            Some(SubagentEffect::SetMaxSubagents(3))
        );
        assert_eq!(
            tree.update_tree(key(KeyCode::Char('+'))),
            Some(SubagentEffect::SetMaxSubagents(4))
        );
        assert_eq!(tree.max_subagents(), 4);
    }

    #[test]
    fn lifecycle_updates_active_count_and_preserves_reusable_agent() {
        let mut tree = tree_of([descriptor()]);
        assert_eq!(tree.active_count(), 1);

        tree.apply(event(AgentEventKind::RunCompleted, json!({})));
        tree.apply(completed(1));
        assert_eq!(tree.active_count(), 0);
        assert!(matches!(
            tree.roster.agents[0].status,
            AgentStatus::Completed { .. }
        ));

        tree.apply(AgentUpdate::Added(descriptor()));
        tree.apply(AgentUpdate::Status {
            id: AgentId::new(1),
            status: AgentStatus::Running,
        });
        assert_eq!(tree.active_count(), 1);
        assert_eq!(tree.roster.agents.len(), 1);
    }

    #[test]
    fn active_filter_hides_completed_agents_until_show_all_is_selected() {
        let mut tree = tree_of([descriptor()]);
        tree.apply(event(AgentEventKind::RunCompleted, json!({})));
        tree.apply(completed(1));

        assert_eq!(tree.filter, AgentFilter::Active);
        assert_eq!(tree.tree_keys()[5], ("f", "filter: active"));
        assert!(tree.visible_ids().is_empty());

        tree.update_tree(key(KeyCode::Char('f')));

        assert_eq!(tree.filter, AgentFilter::All);
        assert_eq!(tree.tree_keys()[5], ("f", "filter: all"));
        assert_eq!(tree.visible_ids(), [AgentId::new(1)]);
        assert_eq!(
            tree.update_tree(key(KeyCode::Enter)),
            Some(SubagentEffect::Inspect(AgentId::new(1)))
        );
    }

    #[test]
    fn active_filter_promotes_children_of_completed_agents_to_roots() {
        let mut tree = tree_of([agent(1, None, "parent"), agent(2, Some(1), "child")]);
        tree.apply(completed(1));

        assert_eq!(tree.visible_ids(), [AgentId::new(2)]);
        assert_eq!(tree.layout().parent(AgentId::new(2)), None);
    }

    #[test]
    fn opening_and_switching_filters_center_the_oldest_matching_agent() {
        let mut tree = tree_of([
            agent(1, None, "oldest"),
            agent(2, None, "older active"),
            agent(3, None, "newer active"),
        ]);
        tree.apply(completed(1));

        tree.update_tree(key(KeyCode::Char('f')));
        assert_eq!(tree.filter, AgentFilter::All);
        assert_eq!(tree.focused, Some(AgentId::new(1)));

        tree.open_tree();
        assert_eq!(tree.filter, AgentFilter::Active);
        assert_eq!(tree.focused, Some(AgentId::new(2)));
        assert_eq!(tree.camera.center, tree.layout().center(AgentId::new(2)));
        assert!(tree.camera.animation.is_none());

        tree.update_tree(key(KeyCode::Char('f')));
        assert_eq!(tree.filter, AgentFilter::All);
        assert_eq!(tree.focused, Some(AgentId::new(1)));
        assert_eq!(tree.camera.center, tree.layout().center(AgentId::new(1)));

        tree.update_tree(key(KeyCode::Right));
        tree.update_tree(key(KeyCode::Right));
        assert_eq!(tree.focused, Some(AgentId::new(3)));

        tree.update_tree(key(KeyCode::Char('f')));
        assert_eq!(tree.filter, AgentFilter::Active);
        assert_eq!(tree.focused, Some(AgentId::new(2)));
        assert_eq!(tree.camera.center, tree.layout().center(AgentId::new(2)));
        assert!(tree.camera.animation.is_none());
    }

    #[test]
    fn focused_tree_layout_golden() {
        let mut tree = tree_of([descriptor()]);

        let backend = render_tree(&mut tree, 64, 18);

        assert_eq!(
            rows(&backend),
            [
                "╭─────────────────────── Sub-agent tree ───────────────────────╮",
                "│                                                              │",
                "│                                                              │",
                "│                   ╭──────────────────────╮                   │",
                "│                   │   ◐ #1 researcher    │                   │",
                "│                   │ running · 0 children │                   │",
                "│                   ╰──────────────────────╯                   │",
                "│                                                              │",
                "│                                                              │",
                "│╭ ◐ #1 · researcher · running ───────────────────────────────╮│",
                "││Task  Trace the event lifecycle                             ││",
                "││Tree  root · 0 children    Concurrency  1 / 32 active       ││",
                "││View  1 agents · active filter    Model  Luna (low)         ││",
                "││Session  child-session                                      ││",
                "│╰────────────────────────────────────────────────────────────╯│",
                "│   ←/→ row · ↑ parent · ↓ child · enter inspect · -/+ limit   │",
                "│                 f filter: active · esc close                 │",
                "╰──────────────────────────────────────────────────────────────╯",
            ]
        );
        let buffer = backend.buffer();
        let luna = buffer
            .content
            .windows(4)
            .find(|cells| {
                cells
                    .iter()
                    .map(|cell| cell.symbol())
                    .eq(["L", "u", "n", "a"])
            })
            .unwrap();
        assert_eq!(
            luna[0].fg,
            Theme::default().model(Model::Codex(CodexModel::Luna))
        );
    }

    #[test]
    fn tree_nests_children_beneath_their_active_parent() {
        let mut tree = tree_of([agent(1, None, "parent"), agent(2, Some(1), "child")]);
        assert_eq!(tree.layout().parent(AgentId::new(2)), Some(AgentId::new(1)));

        let backend = render_tree(&mut tree, 90, 40);

        let [(x, y)] = edge_arrows(&backend)[..] else {
            panic!("one edge should point at the child");
        };
        let child_top_border = &backend.buffer()[(x, y + 1)];
        assert_eq!(child_top_border.symbol(), "─");
        assert_eq!(child_top_border.fg, Theme::default().border());
    }

    #[test]
    fn arrows_navigate_the_hierarchy_and_remember_the_last_child() {
        let mut tree = tree_of([
            agent(1, None, "root"),
            agent(2, Some(1), "left"),
            agent(3, Some(1), "right"),
        ]);
        let start = Instant::now();

        for (code, focused) in [
            (KeyCode::Down, 2),
            (KeyCode::Right, 3),
            (KeyCode::Up, 1),
            (KeyCode::Down, 3),
        ] {
            tree.update_tree_at(key(code), start);
            assert_eq!(tree.focused, Some(AgentId::new(focused)));
        }
    }

    #[test]
    fn horizontal_navigation_crosses_cousins_and_separate_trees() {
        let mut tree = tree_of([
            agent(1, None, "first root"),
            agent(2, Some(1), "left branch"),
            agent(3, Some(1), "right branch"),
            agent(4, Some(2), "left cousin"),
            agent(5, Some(3), "right cousin"),
            agent(10, None, "second root"),
            agent(11, Some(10), "second child"),
            agent(12, Some(11), "second leaf"),
        ]);
        let now = Instant::now();

        tree.focused = Some(AgentId::new(4));
        for (code, focused) in [
            (KeyCode::Right, 5),
            (KeyCode::Right, 12),
            (KeyCode::Left, 5),
        ] {
            tree.update_tree_at(key(code), now);
            assert_eq!(tree.focused, Some(AgentId::new(focused)));
        }

        tree.focused = Some(AgentId::new(1));
        tree.update_tree_at(key(KeyCode::Right), now);
        assert_eq!(tree.focused, Some(AgentId::new(10)));
    }

    #[test]
    fn camera_animation_is_interruptible_and_settles_on_the_latest_focus() {
        let mut tree = tree_of([
            agent(1, None, "root"),
            agent(2, Some(1), "left"),
            agent(3, Some(1), "right"),
        ]);
        render_tree(&mut tree, 100, 40);
        let start = Instant::now();

        tree.update_tree_at(key(KeyCode::Down), start);
        let first_deadline = tree.animation_deadline().unwrap();
        assert!(!tree.advance(first_deadline - Duration::from_millis(1)));

        let first_duration = tree.camera.animation.as_ref().unwrap().duration;
        let interruption = start + first_duration / 2;
        assert!(tree.advance(interruption));
        let interrupted_center = tree.camera.center.unwrap();

        tree.update_tree_at(key(KeyCode::Right), interruption);
        let retargeted = tree.camera.animation.as_ref().unwrap();
        assert_eq!(retargeted.from, interrupted_center);
        assert_eq!(tree.focused, Some(AgentId::new(3)));

        assert!(tree.advance(interruption + Duration::from_secs(1)));
        assert_eq!(tree.camera.center, tree.layout().center(AgentId::new(3)));
        assert!(tree.camera.animation.is_none());
    }

    #[test]
    fn focused_green_border_straddles_the_usable_canvas_center() {
        let mut tree = tree_of([descriptor()]);
        let backend = render_tree(&mut tree, 100, 40);
        let mut focused_cells = Vec::new();
        for y in 0..40 {
            for x in 0..100 {
                if backend.buffer()[(x, y)].fg == Color::Green {
                    focused_cells.push((x, y));
                }
            }
        }

        let min_x = focused_cells.iter().map(|(x, _)| *x).min().unwrap();
        let max_x = focused_cells.iter().map(|(x, _)| *x).max().unwrap();
        let min_y = focused_cells.iter().map(|(_, y)| *y).min().unwrap();
        let max_y = focused_cells.iter().map(|(_, y)| *y).max().unwrap();
        assert!(min_x <= 49 && max_x >= 49);
        assert!(min_y <= 16 && max_y >= 16);
        assert!(
            focused_cells
                .iter()
                .all(|&(x, y)| backend.buffer()[(x, y)].bg != Theme::default().accent())
        );
    }

    #[test]
    fn three_children_render_as_an_even_fan_out() {
        let mut tree = tree_of([
            agent(1, None, "root"),
            agent(2, Some(1), "left"),
            agent(3, Some(1), "middle"),
            agent(4, Some(1), "right"),
        ]);
        assert_eq!(
            tree.layout().children(AgentId::new(1)),
            [AgentId::new(2), AgentId::new(3), AgentId::new(4)]
        );

        let backend = render_tree(&mut tree, 140, 50);

        let arrows = edge_arrows(&backend);
        assert_eq!(arrows.len(), 3);
        assert!(arrows.iter().all(|&(_, y)| y == arrows[0].1));
        let [(junction_x, _)] = cells_with_symbol(&backend, "┼")[..] else {
            panic!("the middle child should share the parent's junction");
        };
        assert_eq!(junction_x, arrows[1].0);
        assert_eq!(arrows[1].0 - arrows[0].0, arrows[2].0 - arrows[1].0);
    }

    #[test]
    fn connector_turns_use_rounded_unicode_corners() {
        assert_eq!(edge_symbol(RIGHT | DOWN), "╭");
        assert_eq!(edge_symbol(LEFT | DOWN), "╮");
        assert_eq!(edge_symbol(RIGHT | UP), "╰");
        assert_eq!(edge_symbol(LEFT | UP), "╯");
    }

    #[test]
    fn narrow_tree_clips_without_panicking() {
        let mut tree = tree_of([descriptor()]);

        let backend = render_tree(&mut tree, 20, 8);

        assert_eq!(backend.buffer().area.width, 20);
    }

    #[test]
    fn hiding_the_tree_finishes_camera_motion() {
        let mut tree = tree_of([agent(1, None, "root"), agent(2, Some(1), "child")]);
        render_tree(&mut tree, 100, 40);
        tree.update_tree_at(key(KeyCode::Down), Instant::now());
        assert!(tree.camera.animation.is_some());

        tree.finish_camera_animation();

        assert!(tree.camera.animation.is_none());
        assert_eq!(tree.camera.center, tree.layout().center(AgentId::new(2)));
    }

    #[test]
    fn transcript_inspector_uses_the_full_screen() {
        let mut tree = tree_of([descriptor()]);
        tree.apply(event(
            AgentEventKind::AssistantMessage,
            json!({"model_call_index": 1, "item_id": "a", "phase": "final_answer", "text": "Report"}),
        ));

        let backend = render_transcript(&mut tree, 1);

        let buffer = backend.buffer();
        assert_eq!(buffer[(0, 0)].symbol(), "╭");
        assert_eq!(buffer[(71, 23)].symbol(), "╯");
    }

    #[test]
    fn transcript_footer_reflects_expandable_focus() {
        let mut tree = tree_of([descriptor()]);
        assert_eq!(tree.transcript_keys(AgentId::new(1)), TRANSCRIPT_KEYS);

        focus_tool(&mut tree);

        assert_eq!(tree.transcript_keys(AgentId::new(1)), FOCUSED_ENTRY_KEYS);
    }

    #[test]
    fn escape_blurs_focused_item_before_returning_to_tree() {
        let mut tree = tree_of([descriptor()]);
        focus_tool(&mut tree);

        assert_eq!(
            tree.update_transcript(AgentId::new(1), key(KeyCode::Esc), SCROLL_LINES),
            None
        );
        assert!(!tree.transcripts[&AgentId::new(1)].expandables_focused());
        assert_eq!(
            tree.update_transcript(AgentId::new(1), key(KeyCode::Esc), SCROLL_LINES),
            Some(SubagentEffect::Back)
        );
        assert_eq!(tree.transcript_keys(AgentId::new(1)), TRANSCRIPT_KEYS);
    }
}
