//! Camera-centered subagent hierarchy and read-only transcript inspector.

use super::{
    floating::Floating,
    node::Node,
    subagent_tree_layout::{
        LayoutNode, NODE_HEIGHT, NODE_WIDTH, NodePosition, TreeLayout, VERTICAL_GAP, WorldPoint,
    },
    transcript::{Transcript, TranscriptEvent},
};
use crate::{
    app::config::DEFAULT_MAX_SUBAGENTS,
    tui::{format::sanitize_terminal_text_inline, theme::Theme, transcript::TranscriptRecord},
};
use crossterm::event::{Event, KeyCode, KeyEventKind};
use nanocodex::Model;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph, Wrap},
};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tact_subagents::{AgentDescriptor, AgentId, AgentStatus, AgentUpdate, MessageSender};
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

struct AgentNode {
    descriptor: AgentDescriptor,
    status: AgentStatus,
    transcript: Node<Transcript>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AgentFilter {
    Running,
    All,
}

impl AgentFilter {
    const fn includes(self, status: &AgentStatus) -> bool {
        match self {
            Self::Running => status.is_active(),
            Self::All => true,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::All => "all",
        }
    }

    const fn toggled(self) -> Self {
        match self {
            Self::Running => Self::All,
            Self::All => Self::Running,
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
    nodes: Vec<AgentNode>,
    focused: Option<AgentId>,
    remembered_children: HashMap<AgentId, AgentId>,
    camera: Camera,
    filter: AgentFilter,
    effort: crate::app::config::ReasoningEffort,
    max_subagents: usize,
    workspace: std::path::PathBuf,
}

impl SubagentTree {
    pub(super) fn new(effort: crate::app::config::ReasoningEffort) -> Self {
        Self {
            nodes: Vec::new(),
            focused: None,
            remembered_children: HashMap::new(),
            camera: Camera::default(),
            filter: AgentFilter::All,
            effort,
            max_subagents: DEFAULT_MAX_SUBAGENTS,
            workspace: std::env::current_dir().unwrap_or_default(),
        }
    }

    pub(super) fn set_workspace(&mut self, workspace: &std::path::Path) {
        self.workspace = workspace.to_path_buf();
        for node in &mut self.nodes {
            node.transcript.component_mut().set_workspace(workspace);
        }
    }

    pub(super) fn apply(&mut self, update: AgentUpdate) -> bool {
        match update {
            AgentUpdate::Added(descriptor) => {
                if let Some(node) = self.node_mut(descriptor.id) {
                    node.descriptor = descriptor;
                } else {
                    let id = descriptor.id;
                    let mut transcript = Transcript::with_effort(self.effort);
                    transcript.set_workspace(&self.workspace);
                    self.nodes.push(AgentNode {
                        descriptor,
                        status: AgentStatus::Running,
                        transcript: Node::new(transcript),
                    });
                    self.focused.get_or_insert(id);
                }
                true
            }
            AgentUpdate::Event { id, event } => {
                let Some(node) = self.node_mut(id) else {
                    return false;
                };
                let record = TranscriptRecord::from_agent(event.seq, unix_time_ms(), event);
                node.transcript
                    .update(TranscriptEvent::Record(Arc::new(record)));
                true
            }
            AgentUpdate::Status { id, status } => {
                let Some(node) = self.node_mut(id) else {
                    return false;
                };
                if node.status == status {
                    return false;
                }
                node.status = status;
                true
            }
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
                    let Some(node) = self.node_mut(agent_id) else {
                        continue;
                    };
                    node.transcript.update(TranscriptEvent::DirectedMessage {
                        perspective: participant,
                        update: update.clone(),
                    });
                    projected = true;
                }
                projected
            }
        }
    }

    pub(super) fn active_count(&self) -> usize {
        self.nodes
            .iter()
            .filter(|node| node.status.is_active())
            .count()
    }

    pub(super) fn set_effort(&mut self, effort: crate::app::config::ReasoningEffort) {
        self.effort = effort;
        for node in &mut self.nodes {
            node.transcript.component_mut().set_effort(effort);
        }
    }

    pub(super) fn set_max_subagents(&mut self, limit: usize) {
        self.max_subagents = limit;
    }

    pub(super) const fn max_subagents(&self) -> usize {
        self.max_subagents
    }

    pub(super) fn contains(&self, id: AgentId) -> bool {
        self.nodes.iter().any(|node| node.descriptor.id == id)
    }

    pub(super) fn is_direct_child(&self, id: AgentId) -> bool {
        self.node(id)
            .is_some_and(|node| node.descriptor.parent.is_none())
    }

    pub(super) fn animation_deadline(&self) -> Option<Instant> {
        self.nodes
            .iter()
            .filter_map(|node| node.transcript.component().animation_deadline())
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
        self.nodes.iter_mut().fold(camera_changed, |changed, node| {
            let node_changed = node
                .transcript
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
                self.ensure_focus(&layout);
                self.recenter_on_focus(&layout, now);
                None
            }
            KeyCode::Char('-') if key.modifiers.is_empty() => {
                self.max_subagents = self.max_subagents.saturating_sub(1);
                Some(SubagentEffect::SetMaxSubagents(self.max_subagents))
            }
            KeyCode::Char('+') | KeyCode::Char('=') if key.modifiers.is_empty() => {
                self.max_subagents = self.max_subagents.saturating_add(1);
                Some(SubagentEffect::SetMaxSubagents(self.max_subagents))
            }
            KeyCode::Enter => self.focused.map(SubagentEffect::Inspect),
            _ => None,
        }
    }

    pub(super) fn update_transcript(
        &mut self,
        id: AgentId,
        event: Event,
    ) -> Option<SubagentEffect> {
        if matches!(
            &event,
            Event::Key(key)
                if key.code == KeyCode::Esc
                    && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        ) {
            let Some(node) = self.node_mut(id) else {
                return Some(SubagentEffect::Back);
            };
            if node.transcript.component().expandables_focused() {
                node.transcript.update(TranscriptEvent::BlurExpandables);
                return None;
            }
            return Some(SubagentEffect::Back);
        }
        let Some(node) = self.node_mut(id) else {
            return Some(SubagentEffect::Back);
        };
        if let Some(destination) = node.transcript.component().link_destination(&event) {
            return Some(SubagentEffect::OpenLink(destination.to_string()));
        }
        if let Some(command) = node.transcript.component().scroll_command(&event) {
            node.transcript.update(TranscriptEvent::Scroll(command));
        } else if let Some(command) = node.transcript.component().expandable_command(&event) {
            node.transcript.update(TranscriptEvent::Expandable(command));
        }
        None
    }

    pub(super) fn toggle_expand_all(&mut self, id: AgentId) -> bool {
        let Some(node) = self.node_mut(id) else {
            return false;
        };
        node.transcript.update(TranscriptEvent::ToggleExpandAll);
        true
    }

    pub(super) fn render_tree(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let layout = Floating::new("Sub-agent tree", area.width, area.height, &TREE_KEYS)
            .render(frame, area, theme);
        if layout.body.is_empty() {
            return;
        }

        let tree_layout = self.layout();
        self.ensure_focus(&tree_layout);
        let Some(focused) = self.focused else {
            let message = if self.nodes.is_empty() {
                format!(
                    "Concurrency: {} / {} active. No subagents have been delegated yet.",
                    self.active_count(),
                    self.max_subagents
                )
            } else {
                format!(
                    "Concurrency: {} / {} active. No subagents are currently running. Press f to show all.",
                    self.active_count(),
                    self.max_subagents
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
            let Some(node) = self.node(id) else {
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
        let Some(node) = self.node_mut(id) else {
            return;
        };
        let title = format!(
            "{} · {} · #{}",
            node.descriptor.role,
            model_name(node.descriptor.model),
            node.descriptor.id
        );
        let keys: &[(&str, &str)] = if node.transcript.component().expandables_focused() {
            &FOCUSED_ENTRY_KEYS
        } else {
            &TRANSCRIPT_KEYS
        };
        let layout = Floating::new(&title, area.width, area.height, keys)
            .colors(theme.border(), theme.model(node.descriptor.model))
            .render(frame, area, theme);
        node.transcript.render(frame, layout.body, theme);
    }

    fn layout(&self) -> TreeLayout {
        let visible = self.visible_ids();
        let nodes = self
            .nodes
            .iter()
            .filter(|node| visible.contains(&node.descriptor.id))
            .map(|node| LayoutNode {
                id: node.descriptor.id,
                parent: node.descriptor.parent,
            })
            .collect::<Vec<_>>();
        TreeLayout::new(&nodes)
    }

    fn visible_ids(&self) -> Vec<AgentId> {
        if self.filter == AgentFilter::All {
            return self.nodes.iter().map(|node| node.descriptor.id).collect();
        }

        let mut visible = self
            .nodes
            .iter()
            .filter(|node| self.filter.includes(&node.status))
            .map(|node| node.descriptor.id)
            .collect::<Vec<_>>();
        let mut cursor = 0;
        while cursor < visible.len() {
            let Some(parent) = self
                .node(visible[cursor])
                .and_then(|node| node.descriptor.parent)
            else {
                cursor += 1;
                continue;
            };
            if !visible.contains(&parent) {
                visible.push(parent);
            }
            cursor += 1;
        }
        self.nodes
            .iter()
            .map(|node| node.descriptor.id)
            .filter(|id| visible.contains(id))
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
        let Some(node) = self.node(focused) else {
            return;
        };
        let (symbol, color, status) = state_style(&node.status);
        let title = format!(
            " {symbol} #{} · {} · {status} ",
            focused, node.descriptor.role
        );
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
        let task = truncate_with_ellipsis(&node.descriptor.task, inner.width.saturating_sub(6));
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
                        self.max_subagents
                    ),
                    Style::default().fg(theme.muted()),
                ),
            ]),
            Line::from(vec![
                Span::styled("View  ", Style::default().fg(theme.muted())),
                Span::raw(format!(
                    "{} agents · {} filter",
                    self.nodes.len(),
                    self.filter.label()
                )),
                Span::styled("    Model  ", Style::default().fg(theme.muted())),
                Span::styled(
                    model_name(node.descriptor.model),
                    Style::default()
                        .fg(theme.model(node.descriptor.model))
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                Span::styled("Session  ", Style::default().fg(theme.muted())),
                Span::raw(truncate_with_ellipsis(
                    &node.descriptor.session_id,
                    inner.width.saturating_sub(9),
                )),
            ]),
        ];
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn node(&self, id: AgentId) -> Option<&AgentNode> {
        self.nodes.iter().find(|node| node.descriptor.id == id)
    }

    fn node_mut(&mut self, id: AgentId) -> Option<&mut AgentNode> {
        self.nodes.iter_mut().find(|node| node.descriptor.id == id)
    }
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
    node: &'a AgentNode,
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
    let role = truncate_with_ellipsis(&node.descriptor.role, role_width);
    let title = centered_text(
        &format!("{symbol} #{} {role}", node.descriptor.id),
        NODE_WIDTH - 2,
    );
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
    let text = truncate_with_ellipsis(text, width);
    let text_width = u16::try_from(UnicodeWidthStr::width(text.as_str())).unwrap_or(u16::MAX);
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

fn truncate_with_ellipsis(text: &str, width: u16) -> String {
    let text = sanitize_terminal_text_inline(text);
    let text = text.as_ref();
    if UnicodeWidthStr::width(text) <= usize::from(width) {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let target = width.saturating_sub(1);
    let mut rendered = String::new();
    let mut used = 0_u16;
    for grapheme in text.graphemes(true) {
        let grapheme_width = u16::try_from(UnicodeWidthStr::width(grapheme)).unwrap_or(u16::MAX);
        if used.saturating_add(grapheme_width) > target {
            break;
        }
        rendered.push_str(grapheme);
        used = used.saturating_add(grapheme_width);
    }
    rendered.push('…');
    rendered
}

fn distance(from: WorldPoint, to: WorldPoint) -> f64 {
    (to.x - from.x).hypot(to.y - from.y)
}

fn unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

fn model_name(model: Model) -> &'static str {
    match model {
        Model::Luna => "Luna",
        Model::Terra => "Terra",
        Model::Sol => "Sol",
        _ => "Sol",
    }
}

#[cfg(test)]
mod tests {
    use super::{SubagentEffect, SubagentTree};
    use crate::{app::config::ReasoningEffort, tui::theme::Theme};
    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use nanocodex::{
        Model,
        agent::events::{AgentEvent, AgentEventKind},
    };
    use ratatui::{Terminal, backend::TestBackend, style::Color};
    use serde_json::{json, value::to_raw_value};
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };
    use tact_subagents::{AgentDescriptor, AgentId, AgentMessageUpdate, AgentStatus, AgentUpdate};

    fn descriptor() -> AgentDescriptor {
        AgentDescriptor {
            id: AgentId::new(1),
            session_id: "child-session".to_owned(),
            model: Model::Luna,
            role: "researcher".to_owned(),
            task: "Trace the event lifecycle".to_owned(),
            parent: None,
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

    fn render_transcript(tree: &mut SubagentTree) -> TestBackend {
        render_agent_transcript(tree, AgentId::new(1))
    }

    fn render_agent_transcript(tree: &mut SubagentTree, id: AgentId) -> TestBackend {
        let mut terminal = Terminal::new(TestBackend::new(100, 40)).unwrap();
        terminal
            .draw(|frame| tree.render_transcript(id, frame, frame.area(), &Theme::default()))
            .unwrap();
        terminal.backend().clone()
    }

    fn message_update(reply: bool) -> AgentUpdate {
        let messages = if reply {
            json!([
                {
                    "id": 1,
                    "thread_id": 1,
                    "from": {"kind": "agent", "agent_id": 1},
                    "to": 2,
                    "priority": "deferred",
                    "purpose": "question",
                    "body": "Can you verify the event ordering?"
                },
                {
                    "id": 2,
                    "thread_id": 1,
                    "from": {"kind": "agent", "agent_id": 2},
                    "to": 1,
                    "priority": "deferred",
                    "purpose": "reply",
                    "in_reply_to": 1,
                    "body": "Verified: delivery precedes projection."
                }
            ])
        } else {
            json!([
                {
                    "id": 1,
                    "thread_id": 1,
                    "from": {"kind": "agent", "agent_id": 1},
                    "to": 2,
                    "priority": "deferred",
                    "purpose": "question",
                    "body": "Can you verify the event ordering?"
                }
            ])
        };
        let message_id = if reply { 2 } else { 1 };
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

    fn render_tree(tree: &mut SubagentTree) -> TestBackend {
        let mut terminal = Terminal::new(TestBackend::new(100, 40)).unwrap();
        terminal
            .draw(|frame| tree.render_tree(frame, frame.area(), &Theme::default()))
            .unwrap();
        terminal.backend().clone()
    }

    fn rendered_text(backend: &TestBackend) -> String {
        backend
            .buffer()
            .content
            .chunks(100)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
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
        let backend = render_transcript(tree);
        let row = backend
            .buffer()
            .content
            .chunks(100)
            .position(|row| row.iter().any(|cell| cell.symbol() == "▶"))
            .expect("tool summary should render");
        tree.update_transcript(
            AgentId::new(1),
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 20,
                row: u16::try_from(row).unwrap(),
                modifiers: KeyModifiers::NONE,
            }),
        );
        assert!(tree.nodes[0].transcript.component().expandables_focused());
    }

    fn second_descriptor() -> AgentDescriptor {
        AgentDescriptor {
            id: AgentId::new(2),
            session_id: "second-session".to_owned(),
            model: Model::Sol,
            role: "reviewer".to_owned(),
            task: "Verify the event ordering".to_owned(),
            parent: None,
        }
    }

    fn tree_descriptor(id: u64, parent: Option<u64>, role: &str) -> AgentDescriptor {
        AgentDescriptor {
            id: AgentId::new(id),
            session_id: format!("agent-{id}"),
            model: Model::Sol,
            role: role.to_owned(),
            task: format!("Task for {role}"),
            parent: parent.map(AgentId::new),
        }
    }

    #[test]
    fn changing_effort_preserves_active_subagents() {
        let mut tree = SubagentTree::new(ReasoningEffort::Medium);
        assert!(tree.apply(AgentUpdate::Added(descriptor())));

        tree.set_effort(ReasoningEffort::High);

        assert_eq!(tree.effort, ReasoningEffort::High);
        assert_eq!(tree.active_count(), 1);
        assert!(tree.contains(AgentId::new(1)));
    }

    #[test]
    fn tree_nodes_do_not_write_control_characters_to_terminal_cells() {
        let mut tree = SubagentTree::new(ReasoningEffort::Medium);
        let mut agent = descriptor();
        agent.role = "re\u{1b}\tsearcher".to_owned();
        tree.apply(AgentUpdate::Added(agent));

        let backend = render_tree(&mut tree);
        assert!(
            backend
                .buffer()
                .content
                .iter()
                .all(|cell| { !cell.symbol().chars().any(char::is_control) })
        );
    }

    #[test]
    fn directed_messages_are_upserted_into_both_agent_transcripts() {
        let mut tree = SubagentTree::new(ReasoningEffort::Medium);
        tree.apply(AgentUpdate::Added(descriptor()));
        tree.apply(AgentUpdate::Added(second_descriptor()));

        assert!(tree.apply(message_update(false)));
        assert!(tree.apply(message_update(true)));

        let sender = rendered_text(&render_agent_transcript(&mut tree, AgentId::new(1)));
        let recipient = rendered_text(&render_agent_transcript(&mut tree, AgentId::new(2)));
        assert!(sender.contains("← Message  #2 → you"));
        assert!(recipient.contains("→ Message  you → #1"));
        assert!(sender.contains("2 messages"));
        assert!(recipient.contains("2 messages"));
        assert_eq!(sender.matches('▶').count(), 1);
        assert_eq!(recipient.matches('▶').count(), 1);
    }

    #[test]
    fn directed_message_threads_share_inline_focus_and_expansion() {
        let mut tree = SubagentTree::new(ReasoningEffort::Medium);
        tree.apply(AgentUpdate::Added(descriptor()));
        tree.apply(AgentUpdate::Added(second_descriptor()));
        tree.apply(message_update(true));
        let collapsed = render_agent_transcript(&mut tree, AgentId::new(1));
        let row = collapsed
            .buffer()
            .content
            .chunks(100)
            .position(|row| row.iter().any(|cell| cell.symbol() == "▶"))
            .expect("message summary should render");

        tree.update_transcript(
            AgentId::new(1),
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 20,
                row: u16::try_from(row).unwrap(),
                modifiers: KeyModifiers::NONE,
            }),
        );

        let expanded = rendered_text(&render_agent_transcript(&mut tree, AgentId::new(1)));
        assert!(tree.nodes[0].transcript.component().expandables_focused());
        assert!(expanded.contains("▼"), "{expanded}");
        assert!(expanded.contains("Can you verify the event ordering?"));
        assert!(expanded.contains("thread #1 · 2 messages"));
        assert!(expanded.contains("↑↓ item"));
    }

    #[test]
    fn concurrency_limit_is_visible_and_editable() {
        let mut tree = SubagentTree::new(ReasoningEffort::Medium);
        tree.set_max_subagents(4);

        assert!(rendered_text(&render_tree(&mut tree)).contains("Concurrency: 0 / 4 active"));
        assert_eq!(
            tree.update_tree(Event::Key(KeyEvent::new(
                KeyCode::Char('-'),
                KeyModifiers::NONE,
            ))),
            Some(SubagentEffect::SetMaxSubagents(3))
        );
        assert_eq!(
            tree.update_tree(Event::Key(KeyEvent::new(
                KeyCode::Char('+'),
                KeyModifiers::NONE,
            ))),
            Some(SubagentEffect::SetMaxSubagents(4))
        );
    }

    #[test]
    fn lifecycle_updates_active_count_and_preserves_reusable_agent() {
        let mut tree = SubagentTree::new(ReasoningEffort::Medium);
        assert!(tree.apply(AgentUpdate::Added(descriptor())));
        assert_eq!(tree.active_count(), 1);

        tree.apply(event(AgentEventKind::RunCompleted, json!({})));
        tree.apply(AgentUpdate::Status {
            id: AgentId::new(1),
            status: AgentStatus::Completed {
                output: json!({ "report": "done" }),
            },
        });
        assert_eq!(tree.active_count(), 0);
        assert!(matches!(
            tree.nodes[0].status,
            AgentStatus::Completed { .. }
        ));

        tree.apply(AgentUpdate::Added(descriptor()));
        tree.apply(AgentUpdate::Status {
            id: AgentId::new(1),
            status: AgentStatus::Running,
        });
        assert_eq!(tree.active_count(), 1);
        assert_eq!(tree.nodes.len(), 1);
    }

    #[test]
    fn running_filter_hides_completed_agents_until_show_all_is_selected() {
        let mut tree = SubagentTree::new(ReasoningEffort::Medium);
        tree.apply(AgentUpdate::Added(descriptor()));
        tree.apply(event(AgentEventKind::RunCompleted, json!({})));
        tree.apply(AgentUpdate::Status {
            id: AgentId::new(1),
            status: AgentStatus::Completed {
                output: json!({ "report": "done" }),
            },
        });

        assert_eq!(tree.visible_ids(), [AgentId::new(1)]);

        tree.update_tree(Event::Key(KeyEvent::new(
            KeyCode::Char('f'),
            KeyModifiers::NONE,
        )));
        assert!(tree.visible_ids().is_empty());

        tree.update_tree(Event::Key(KeyEvent::new(
            KeyCode::Char('f'),
            KeyModifiers::NONE,
        )));
        let visible = tree.visible_ids();

        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0], AgentId::new(1));
        assert!(matches!(
            tree.update_tree(Event::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            ))),
            Some(SubagentEffect::Inspect(id)) if id == AgentId::new(1)
        ));
    }

    #[test]
    fn running_filter_retains_a_completed_ancestor_for_context() {
        let mut tree = SubagentTree::new(ReasoningEffort::Medium);
        let parent = descriptor();
        let mut child = descriptor();
        child.id = AgentId::new(2);
        child.parent = Some(parent.id);
        tree.apply(AgentUpdate::Added(parent));
        tree.apply(AgentUpdate::Added(child));
        tree.apply(AgentUpdate::Status {
            id: AgentId::new(1),
            status: AgentStatus::Completed {
                output: json!({ "report": "done" }),
            },
        });
        tree.update_tree(Event::Key(KeyEvent::new(
            KeyCode::Char('f'),
            KeyModifiers::NONE,
        )));

        let visible = tree.visible_ids();

        assert_eq!(visible, [AgentId::new(1), AgentId::new(2)]);
        assert_eq!(tree.layout().parent(AgentId::new(2)), Some(AgentId::new(1)));
    }

    #[test]
    fn tree_renders_a_rounded_focused_node_and_anchored_task_details() {
        let mut tree = SubagentTree::new(ReasoningEffort::Medium);
        tree.apply(AgentUpdate::Added(descriptor()));
        let mut terminal = Terminal::new(TestBackend::new(90, 40)).unwrap();

        terminal
            .draw(|frame| tree.render_tree(frame, frame.area(), &Theme::default()))
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .chunks(90)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("╭──────────────────────╮"));
        assert!(rendered.contains("researcher"));
        assert!(rendered.contains("running · 0 children"));
        assert!(rendered.contains("Trace the event lifecycle"));
        assert!(rendered.contains("Model  Luna"));
        let buffer = terminal.backend().buffer();
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
        assert_eq!(luna[0].fg, Color::White);
        assert_eq!(buffer[(0, 0)].symbol(), "╭");
        assert_eq!(buffer[(89, 39)].symbol(), "╯");
    }

    #[test]
    fn tree_nests_children_beneath_their_active_parent() {
        let mut tree = SubagentTree::new(ReasoningEffort::Medium);
        let mut parent = descriptor();
        parent.role = "parent".to_owned();
        let mut child = descriptor();
        child.id = AgentId::new(2);
        child.role = "child".to_owned();
        child.parent = Some(parent.id);
        tree.apply(AgentUpdate::Added(parent));
        tree.apply(AgentUpdate::Added(child));

        let layout = tree.layout();
        assert_eq!(layout.parent(AgentId::new(2)), Some(AgentId::new(1)));

        let mut terminal = Terminal::new(TestBackend::new(90, 40)).unwrap();
        terminal
            .draw(|frame| tree.render_tree(frame, frame.area(), &Theme::default()))
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .chunks(90)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("#1 parent"));
        assert!(rendered.contains("#2 child"));
        assert!(rendered.contains('↓'));
        let buffer = terminal.backend().buffer();
        let arrow = buffer
            .content
            .iter()
            .find(|cell| cell.symbol() == "↓")
            .unwrap();
        assert_eq!(arrow.fg, Theme::default().border());

        let child_row = buffer
            .content
            .chunks(90)
            .find(|row| {
                row.iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>()
                    .contains("#2 child")
            })
            .unwrap();
        let child_text = child_row
            .iter()
            .position(|cell| cell.symbol() == "#")
            .unwrap();
        let child_border = child_row[..child_text]
            .iter()
            .rposition(|cell| cell.symbol() == "│")
            .unwrap();
        assert_eq!(child_row[child_border].fg, Theme::default().border());
    }

    #[test]
    fn arrows_navigate_the_hierarchy_and_remember_the_last_child() {
        let mut tree = SubagentTree::new(ReasoningEffort::Medium);
        tree.apply(AgentUpdate::Added(tree_descriptor(1, None, "root")));
        tree.apply(AgentUpdate::Added(tree_descriptor(2, Some(1), "left")));
        tree.apply(AgentUpdate::Added(tree_descriptor(3, Some(1), "right")));
        let start = Instant::now();

        tree.update_tree_at(
            Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            start,
        );
        assert_eq!(tree.focused, Some(AgentId::new(2)));

        tree.update_tree_at(
            Event::Key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
            start,
        );
        assert_eq!(tree.focused, Some(AgentId::new(3)));

        tree.update_tree_at(
            Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            start,
        );
        assert_eq!(tree.focused, Some(AgentId::new(1)));

        tree.update_tree_at(
            Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            start,
        );
        assert_eq!(tree.focused, Some(AgentId::new(3)));
    }

    #[test]
    fn horizontal_navigation_crosses_cousins_and_separate_trees() {
        let mut tree = SubagentTree::new(ReasoningEffort::Medium);
        tree.apply(AgentUpdate::Added(tree_descriptor(1, None, "first root")));
        tree.apply(AgentUpdate::Added(tree_descriptor(
            2,
            Some(1),
            "left branch",
        )));
        tree.apply(AgentUpdate::Added(tree_descriptor(
            3,
            Some(1),
            "right branch",
        )));
        tree.apply(AgentUpdate::Added(tree_descriptor(
            4,
            Some(2),
            "left cousin",
        )));
        tree.apply(AgentUpdate::Added(tree_descriptor(
            5,
            Some(3),
            "right cousin",
        )));
        tree.apply(AgentUpdate::Added(tree_descriptor(10, None, "second root")));
        tree.apply(AgentUpdate::Added(tree_descriptor(
            11,
            Some(10),
            "second child",
        )));
        tree.apply(AgentUpdate::Added(tree_descriptor(
            12,
            Some(11),
            "second leaf",
        )));
        let now = Instant::now();

        tree.focused = Some(AgentId::new(4));
        tree.update_tree_at(
            Event::Key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
            now,
        );
        assert_eq!(tree.focused, Some(AgentId::new(5)));
        tree.update_tree_at(
            Event::Key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
            now,
        );
        assert_eq!(tree.focused, Some(AgentId::new(12)));
        tree.update_tree_at(
            Event::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
            now,
        );
        assert_eq!(tree.focused, Some(AgentId::new(5)));

        tree.focused = Some(AgentId::new(1));
        tree.update_tree_at(
            Event::Key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
            now,
        );
        assert_eq!(tree.focused, Some(AgentId::new(10)));
    }

    #[test]
    fn camera_animation_is_interruptible_and_settles_on_the_latest_focus() {
        let mut tree = SubagentTree::new(ReasoningEffort::Medium);
        tree.apply(AgentUpdate::Added(tree_descriptor(1, None, "root")));
        tree.apply(AgentUpdate::Added(tree_descriptor(2, Some(1), "left")));
        tree.apply(AgentUpdate::Added(tree_descriptor(3, Some(1), "right")));
        render_tree(&mut tree);
        let start = Instant::now();

        tree.update_tree_at(
            Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            start,
        );
        let first_deadline = tree.animation_deadline().unwrap();
        assert!(!tree.advance(first_deadline - Duration::from_millis(1)));

        let first_duration = tree.camera.animation.as_ref().unwrap().duration;
        let interruption = start + first_duration / 2;
        assert!(tree.advance(interruption));
        let interrupted_center = tree.camera.center.unwrap();

        tree.update_tree_at(
            Event::Key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
            interruption,
        );
        let retargeted = tree.camera.animation.as_ref().unwrap();
        assert_eq!(retargeted.from, interrupted_center);
        assert_eq!(tree.focused, Some(AgentId::new(3)));

        assert!(tree.advance(interruption + Duration::from_secs(1)));
        assert_eq!(tree.camera.center, tree.layout().center(AgentId::new(3)));
        assert!(tree.camera.animation.is_none());
    }

    #[test]
    fn focused_green_border_straddles_the_usable_canvas_center() {
        let mut tree = SubagentTree::new(ReasoningEffort::Medium);
        tree.apply(AgentUpdate::Added(descriptor()));
        let backend = render_tree(&mut tree);
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
        let mut tree = SubagentTree::new(ReasoningEffort::Medium);
        tree.apply(AgentUpdate::Added(tree_descriptor(1, None, "root")));
        tree.apply(AgentUpdate::Added(tree_descriptor(2, Some(1), "left")));
        tree.apply(AgentUpdate::Added(tree_descriptor(3, Some(1), "middle")));
        tree.apply(AgentUpdate::Added(tree_descriptor(4, Some(1), "right")));
        let mut terminal = Terminal::new(TestBackend::new(140, 50)).unwrap();
        terminal
            .draw(|frame| tree.render_tree(frame, frame.area(), &Theme::default()))
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .chunks(140)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");

        let arrows = (0..35)
            .flat_map(|y| (0..140).map(move |x| (x, y)))
            .filter(|&(x, y)| terminal.backend().buffer()[(x, y)].symbol() == "↓")
            .count();
        assert_eq!(arrows, 3);
        assert!(rendered.contains('┼'));
        assert!(rendered.contains("#2 left"));
        assert!(rendered.contains("#3 middle"));
        assert!(rendered.contains("#4 right"));
    }

    #[test]
    fn connector_turns_use_rounded_unicode_corners() {
        assert_eq!(super::edge_symbol(super::RIGHT | super::DOWN), "╭");
        assert_eq!(super::edge_symbol(super::LEFT | super::DOWN), "╮");
        assert_eq!(super::edge_symbol(super::RIGHT | super::UP), "╰");
        assert_eq!(super::edge_symbol(super::LEFT | super::UP), "╯");
    }

    #[test]
    fn narrow_tree_clips_without_panicking() {
        let mut tree = SubagentTree::new(ReasoningEffort::Medium);
        tree.apply(AgentUpdate::Added(descriptor()));
        let mut terminal = Terminal::new(TestBackend::new(20, 8)).unwrap();

        terminal
            .draw(|frame| tree.render_tree(frame, frame.area(), &Theme::default()))
            .unwrap();

        assert_eq!(terminal.backend().buffer().area.width, 20);
    }

    #[test]
    fn hiding_the_tree_finishes_camera_motion() {
        let mut tree = SubagentTree::new(ReasoningEffort::Medium);
        tree.apply(AgentUpdate::Added(tree_descriptor(1, None, "root")));
        tree.apply(AgentUpdate::Added(tree_descriptor(2, Some(1), "child")));
        render_tree(&mut tree);
        tree.update_tree_at(
            Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            Instant::now(),
        );
        assert!(tree.camera.animation.is_some());

        tree.finish_camera_animation();

        assert!(tree.camera.animation.is_none());
        assert_eq!(tree.camera.center, tree.layout().center(AgentId::new(2)));
    }

    #[test]
    fn transcript_inspector_uses_the_full_screen() {
        let mut tree = SubagentTree::new(ReasoningEffort::Medium);
        tree.apply(AgentUpdate::Added(descriptor()));
        tree.apply(event(
            AgentEventKind::AssistantMessage,
            json!({"model_call_index": 1, "item_id": "a", "phase": "final_answer", "text": "Report"}),
        ));
        let mut terminal = Terminal::new(TestBackend::new(100, 40)).unwrap();

        terminal
            .draw(|frame| {
                tree.render_transcript(AgentId::new(1), frame, frame.area(), &Theme::default());
            })
            .unwrap();
        let buffer = terminal.backend().buffer();

        assert_eq!(buffer[(0, 0)].symbol(), "╭");
        assert_eq!(buffer[(99, 39)].symbol(), "╯");
    }

    #[test]
    fn transcript_footer_reflects_expandable_focus_without_permanent_mouse_help() {
        let mut tree = SubagentTree::new(ReasoningEffort::Medium);
        tree.apply(AgentUpdate::Added(descriptor()));

        let unfocused = rendered_text(&render_transcript(&mut tree));
        assert!(unfocused.contains("pgup/pgdn scroll"));
        assert!(unfocused.contains("esc back"));
        assert!(!unfocused.contains("click"));

        focus_tool(&mut tree);
        let focused = rendered_text(&render_transcript(&mut tree));
        assert!(focused.contains("↑↓ item"));
        assert!(focused.contains("enter toggle"));
        assert!(focused.contains("esc blur, then back"));
        assert!(!focused.contains("pgup/pgdn scroll"));
        assert!(!focused.contains("click"));
    }

    #[test]
    fn escape_blurs_focused_item_before_returning_to_tree() {
        let mut tree = SubagentTree::new(ReasoningEffort::Medium);
        tree.apply(AgentUpdate::Added(descriptor()));
        focus_tool(&mut tree);
        let escape = || Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert!(tree.update_transcript(AgentId::new(1), escape()).is_none());
        assert!(!tree.nodes[0].transcript.component().expandables_focused());
        assert!(matches!(
            tree.update_transcript(AgentId::new(1), escape()),
            Some(SubagentEffect::Back)
        ));
    }
}
