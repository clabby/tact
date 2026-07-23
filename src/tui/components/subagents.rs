//! Presentation model for the subagent tree and read-only transcript inspector.

use super::{
    floating::Floating,
    node::Node,
    transcript::{Transcript, TranscriptEvent},
};
use crate::{
    subagents::{AgentDescriptor, AgentId, AgentOrigin, AgentStatus, AgentUpdate},
    tui::{theme::Theme, transcript::TranscriptRecord},
};
use crossterm::event::{Event, KeyCode, KeyEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph, Wrap},
};
use std::{sync::Arc, time::Instant};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const TREE_KEYS: [&str; 3] = ["↑↓ select", "enter inspect", "esc close"];
const TRANSCRIPT_KEYS: [&str; 4] = [
    "pgup/pgdn scroll",
    "ctrl+home/end",
    "ctrl+o expand all",
    "esc back",
];
const FOCUSED_TOOL_KEYS: [&str; 3] = ["↑↓ tool", "enter toggle", "esc blur, then back"];

struct AgentNode {
    descriptor: AgentDescriptor,
    status: AgentStatus,
    transcript: Node<Transcript>,
}

struct VisibleNode {
    index: usize,
    ancestor_is_last: Vec<bool>,
    is_last: bool,
    has_children: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SubagentOverlay {
    Tree,
    Transcript(AgentId),
}

pub(super) enum SubagentEffect {
    Dismiss,
    Inspect(AgentId),
    Back,
    OpenLink(String),
}

pub(super) struct SubagentTree {
    nodes: Vec<AgentNode>,
    selected: usize,
    effort: crate::config::ReasoningEffort,
}

impl SubagentTree {
    pub(super) const fn new(effort: crate::config::ReasoningEffort) -> Self {
        Self {
            nodes: Vec::new(),
            selected: 0,
            effort,
        }
    }

    pub(super) fn apply(&mut self, update: AgentUpdate) -> bool {
        match update {
            AgentUpdate::Added(descriptor) => {
                if let Some(node) = self.node_mut(descriptor.id) {
                    node.descriptor = descriptor;
                } else {
                    self.nodes.push(AgentNode {
                        descriptor,
                        status: AgentStatus::Running,
                        transcript: Node::new(Transcript::with_effort(self.effort)),
                    });
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
                node.status = status;
                self.selected = self.selected.min(self.nodes.len().saturating_sub(1));
                true
            }
        }
    }

    pub(super) fn active_count(&self) -> usize {
        self.nodes
            .iter()
            .filter(|node| node.status.is_active())
            .count()
    }

    pub(super) fn set_effort(&mut self, effort: crate::config::ReasoningEffort) {
        self.effort = effort;
        for node in &mut self.nodes {
            node.transcript.component_mut().set_effort(effort);
        }
    }

    pub(super) fn contains(&self, id: AgentId) -> bool {
        self.nodes.iter().any(|node| node.descriptor.id == id)
    }

    pub(super) fn animation_deadline(&self) -> Option<Instant> {
        self.nodes
            .iter()
            .filter_map(|node| node.transcript.component().animation_deadline())
            .min()
    }

    pub(super) fn advance(&mut self, now: Instant) -> bool {
        self.nodes.iter_mut().fold(false, |changed, node| {
            let node_changed = node
                .transcript
                .update(TranscriptEvent::AnimationFrame(now))
                .render
                != super::node::RenderRequest::None;
            changed || node_changed
        })
    }

    pub(super) fn update_tree(&mut self, event: Event) -> Option<SubagentEffect> {
        let Event::Key(key) = event else {
            return None;
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return None;
        }
        match key.code {
            KeyCode::Esc => Some(SubagentEffect::Dismiss),
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(self.nodes.len().saturating_sub(1));
                None
            }
            KeyCode::Enter => self
                .visible_nodes()
                .get(self.selected)
                .map(|visible| SubagentEffect::Inspect(self.nodes[visible.index].descriptor.id)),
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
            if node.transcript.component().tools_focused() {
                node.transcript.update(TranscriptEvent::BlurTools);
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
        } else if let Some(command) = node.transcript.component().tool_command(&event) {
            node.transcript.update(TranscriptEvent::Tool(command));
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
        let visible = self.visible_nodes();
        let height = u16::try_from(visible.len().saturating_mul(2).saturating_add(6))
            .unwrap_or(u16::MAX)
            .clamp(8, 22);
        let layout = Floating::new("Subagents", 74, height, &TREE_KEYS).render(frame, area, theme);
        if layout.body.is_empty() {
            return;
        }
        if visible.is_empty() {
            frame.render_widget(
                Paragraph::new("No subagents have been delegated yet.")
                    .style(Style::default().fg(theme.muted()))
                    .wrap(Wrap { trim: true }),
                inset(layout.body, 2, 1),
            );
            return;
        }

        let mut items = Vec::with_capacity(visible.len() + 1);
        items.push(ListItem::new(Line::from(vec![
            Span::styled("● ", Style::default().fg(theme.accent())),
            Span::styled("main agent", Style::default().add_modifier(Modifier::BOLD)),
            Span::styled(
                format!(
                    "  ·  {} active · {} total",
                    self.active_count(),
                    visible.len()
                ),
                Style::default().fg(theme.muted()),
            ),
        ])));
        for (selection_index, visible_node) in visible.iter().enumerate() {
            let node = &self.nodes[visible_node.index];
            let (symbol, color, label) = state_style(&node.status);
            let selected = selection_index == self.selected;
            let text_color = if selected { Color::Black } else { theme.text() };
            let detail_color = if selected { Color::Black } else { color };
            let muted_color = if selected {
                Color::Black
            } else {
                theme.muted()
            };
            let branch_color = if selected {
                Color::Black
            } else {
                theme.border()
            };
            let origin = match node.descriptor.origin {
                AgentOrigin::Spawn => "spawn",
                AgentOrigin::Fork => "fork",
            };
            let branch_prefix = tree_prefix(visible_node, false);
            let task_prefix = tree_prefix(visible_node, true);
            let indentation = u16::try_from(visible_node.ancestor_is_last.len())
                .unwrap_or(u16::MAX)
                .saturating_mul(3);
            let task_width = layout.body.width.saturating_sub(9 + indentation);
            let task = truncate_with_ellipsis(&node.descriptor.task, task_width);
            items.push(ListItem::new(vec![
                Line::from(vec![
                    Span::styled(branch_prefix, Style::default().fg(branch_color)),
                    Span::styled(format!("{symbol} "), Style::default().fg(detail_color)),
                    Span::styled(
                        node.descriptor.role.clone(),
                        Style::default().fg(text_color).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("  {label} · {origin} · #{}", node.descriptor.id),
                        Style::default().fg(detail_color),
                    ),
                ]),
                Line::from(vec![
                    Span::styled(task_prefix, Style::default().fg(branch_color)),
                    Span::styled(task, Style::default().fg(muted_color)),
                ]),
            ]));
        }
        let mut state = ListState::default().with_selected(Some(self.selected + 1));
        let list = List::new(items).highlight_symbol("  ").highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(theme.accent())
                .add_modifier(Modifier::BOLD),
        );
        frame.render_stateful_widget(list, layout.body, &mut state);
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
        let title = format!("{} · #{}", node.descriptor.role, node.descriptor.id);
        let width = area.width.saturating_mul(4) / 5;
        let height = area.height.saturating_mul(4) / 5;
        let keys: &[&str] = if node.transcript.component().tools_focused() {
            &FOCUSED_TOOL_KEYS
        } else {
            &TRANSCRIPT_KEYS
        };
        let layout = Floating::new(&title, width, height, keys)
            .colors(theme.border(), theme.accent())
            .render(frame, area, theme);
        node.transcript.render(frame, layout.body, theme);
    }

    fn node_mut(&mut self, id: AgentId) -> Option<&mut AgentNode> {
        self.nodes.iter_mut().find(|node| node.descriptor.id == id)
    }

    fn visible_nodes(&self) -> Vec<VisibleNode> {
        let roots = self
            .nodes
            .iter()
            .enumerate()
            .filter_map(|(index, node)| {
                let parent_exists = node.descriptor.parent.is_some_and(|parent| {
                    self.nodes
                        .iter()
                        .any(|candidate| candidate.descriptor.id == parent)
                });
                (!parent_exists).then_some(index)
            })
            .collect::<Vec<_>>();
        let mut visible = Vec::new();
        self.append_visible(&roots, &[], &mut visible);
        visible
    }

    fn append_visible(
        &self,
        siblings: &[usize],
        ancestors: &[bool],
        visible: &mut Vec<VisibleNode>,
    ) {
        for (position, &index) in siblings.iter().enumerate() {
            let is_last = position + 1 == siblings.len();
            let id = self.nodes[index].descriptor.id;
            let children = self
                .nodes
                .iter()
                .enumerate()
                .filter_map(|(child_index, child)| {
                    (child.descriptor.parent == Some(id)).then_some(child_index)
                })
                .collect::<Vec<_>>();
            visible.push(VisibleNode {
                index,
                ancestor_is_last: ancestors.to_vec(),
                is_last,
                has_children: !children.is_empty(),
            });
            let mut child_ancestors = ancestors.to_vec();
            child_ancestors.push(is_last);
            self.append_visible(&children, &child_ancestors, visible);
        }
    }
}

fn tree_prefix(node: &VisibleNode, task_line: bool) -> String {
    let mut prefix = String::new();
    for &is_last in &node.ancestor_is_last {
        prefix.push_str(if is_last { "   " } else { "│  " });
    }
    if task_line {
        prefix.push_str(match (node.is_last, node.has_children) {
            (true, true) => "   ├─ ",
            (true, false) => "   ╰─ ",
            (false, true) => "│  ├─ ",
            (false, false) => "│  ╰─ ",
        });
    } else {
        prefix.push_str(if node.is_last { "└─ " } else { "├─ " });
    }
    prefix
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
    if UnicodeWidthStr::width(text) <= usize::from(width) {
        return text.to_owned();
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

fn unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use super::{SubagentEffect, SubagentTree};
    use crate::{
        config::ReasoningEffort,
        subagents::{AgentDescriptor, AgentId, AgentOrigin, AgentStatus, AgentUpdate},
        tui::theme::Theme,
    };
    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use nanocodex::{AgentEvent, AgentEventKind};
    use ratatui::{Terminal, backend::TestBackend};
    use serde_json::{json, value::to_raw_value};
    use std::sync::Arc;

    fn descriptor() -> AgentDescriptor {
        AgentDescriptor {
            id: AgentId::new(1),
            session_id: "child-session".to_owned(),
            role: "researcher".to_owned(),
            task: "Trace the event lifecycle".to_owned(),
            origin: AgentOrigin::Fork,
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
                payload: to_raw_value(&payload).unwrap(),
            },
        }
    }

    fn render_transcript(tree: &mut SubagentTree) -> TestBackend {
        let mut terminal = Terminal::new(TestBackend::new(100, 40)).unwrap();
        terminal
            .draw(|frame| {
                tree.render_transcript(AgentId::new(1), frame, frame.area(), &Theme::default());
            })
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
        assert!(tree.nodes[0].transcript.component().tools_focused());
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
    fn lifecycle_updates_active_count_and_preserves_reusable_agent() {
        let mut tree = SubagentTree::new(ReasoningEffort::Medium);
        assert!(tree.apply(AgentUpdate::Added(descriptor())));
        assert_eq!(tree.active_count(), 1);

        tree.apply(event(AgentEventKind::RunCompleted, json!({})));
        tree.apply(AgentUpdate::Status {
            id: AgentId::new(1),
            status: AgentStatus::Completed {
                report: "done".to_owned(),
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
    fn completed_agents_remain_visible_and_inspectable() {
        let mut tree = SubagentTree::new(ReasoningEffort::Medium);
        tree.apply(AgentUpdate::Added(descriptor()));
        tree.apply(event(AgentEventKind::RunCompleted, json!({})));
        tree.apply(AgentUpdate::Status {
            id: AgentId::new(1),
            status: AgentStatus::Completed {
                report: "done".to_owned(),
            },
        });

        let visible = tree.visible_nodes();

        assert_eq!(visible.len(), 1);
        assert_eq!(tree.nodes[visible[0].index].descriptor.id, AgentId::new(1));
    }

    #[test]
    fn tree_renders_role_task_origin_and_state_as_one_joined_branch() {
        let mut tree = SubagentTree::new(ReasoningEffort::Medium);
        tree.apply(AgentUpdate::Added(descriptor()));
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();

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

        assert!(rendered.contains("main agent"));
        assert!(rendered.contains("researcher"));
        assert!(rendered.contains("running · fork · #1"));
        assert!(rendered.contains("Trace the event lifecycle"));
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

        let visible = tree.visible_nodes();
        assert_eq!(visible.len(), 2);
        assert!(visible[0].ancestor_is_last.is_empty());
        assert_eq!(visible[1].ancestor_is_last, [true]);

        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
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

        assert!(rendered.contains("└─ ◐ parent"));
        assert!(rendered.contains("   ├─ Trace the event lifecycle"));
        assert!(rendered.contains("   └─ ◐ child"));
    }

    #[test]
    fn transcript_inspector_uses_ten_percent_margins() {
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

        assert_eq!(buffer[(10, 4)].symbol(), "╭");
        assert_eq!(buffer[(89, 35)].symbol(), "╯");
    }

    #[test]
    fn transcript_footer_reflects_tool_focus_without_permanent_mouse_help() {
        let mut tree = SubagentTree::new(ReasoningEffort::Medium);
        tree.apply(AgentUpdate::Added(descriptor()));

        let unfocused = rendered_text(&render_transcript(&mut tree));
        assert!(unfocused.contains("pgup/pgdn scroll"));
        assert!(unfocused.contains("esc back"));
        assert!(!unfocused.contains("click"));

        focus_tool(&mut tree);
        let focused = rendered_text(&render_transcript(&mut tree));
        assert!(focused.contains("↑↓ tool"));
        assert!(focused.contains("enter toggle"));
        assert!(focused.contains("esc blur, then back"));
        assert!(!focused.contains("pgup/pgdn scroll"));
        assert!(!focused.contains("click"));
    }

    #[test]
    fn escape_blurs_focused_tool_before_returning_to_tree() {
        let mut tree = SubagentTree::new(ReasoningEffort::Medium);
        tree.apply(AgentUpdate::Added(descriptor()));
        focus_tool(&mut tree);
        let escape = || Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert!(tree.update_transcript(AgentId::new(1), escape()).is_none());
        assert!(!tree.nodes[0].transcript.component().tools_focused());
        assert!(matches!(
            tree.update_transcript(AgentId::new(1), escape()),
            Some(SubagentEffect::Back)
        ));
    }
}
