//! Searchable picker for resumable persisted sessions.

use super::{
    floating::Floating,
    node::{Component, ComponentUpdate, RenderRequest},
};
use crate::tui::{
    session::{SessionSummary, format_age},
    theme::Theme,
};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const KEY_BINDINGS: [&str; 4] = ["tab focus", "↑↓ move", "enter resume", "esc close"];
const SEARCH_LABEL: &str = "Search: ";

pub(super) enum SessionPickerEvent {
    Terminal(Event),
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum SessionPickerEffect {
    Dismiss,
    Resume(String),
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Focus {
    Search,
    List,
}

pub(super) struct SessionPicker {
    sessions: Vec<SessionSummary>,
    query: String,
    matches: Vec<usize>,
    selected: usize,
    focus: Focus,
}

impl SessionPicker {
    pub(super) fn new(sessions: Vec<SessionSummary>) -> Self {
        let matches = (0..sessions.len()).collect();
        Self {
            sessions,
            query: String::new(),
            matches,
            selected: 0,
            focus: Focus::Search,
        }
    }

    fn update_key(
        &mut self,
        key: crossterm::event::KeyEvent,
    ) -> ComponentUpdate<SessionPickerEffect> {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return ComponentUpdate::none();
        }
        match key.code {
            KeyCode::Esc => Self::effect(SessionPickerEffect::Dismiss),
            KeyCode::Backspace if self.focus == Focus::Search && !self.query.is_empty() => {
                if let Some((index, _)) = self.query.grapheme_indices(true).next_back() {
                    self.query.truncate(index);
                    self.refresh_matches();
                }
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            KeyCode::Backspace => Self::effect(SessionPickerEffect::Dismiss),
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::Search => Focus::List,
                    Focus::List => Focus::Search,
                };
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            KeyCode::Up if self.focus == Focus::List => {
                self.selected = self.selected.saturating_sub(1);
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            KeyCode::Down if self.focus == Focus::List => {
                if !self.matches.is_empty() {
                    self.selected = (self.selected + 1).min(self.matches.len() - 1);
                }
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            KeyCode::Enter => self.resume_selected(),
            KeyCode::Char(character)
                if self.focus == Focus::Search
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.query.push(character);
                self.refresh_matches();
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            _ => ComponentUpdate::none(),
        }
    }

    fn insert_paste(&mut self, text: &str) -> ComponentUpdate<SessionPickerEffect> {
        if self.focus != Focus::Search {
            return ComponentUpdate::none();
        }
        self.query
            .extend(text.chars().filter(|character| !character.is_control()));
        self.refresh_matches();
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn resume_selected(&mut self) -> ComponentUpdate<SessionPickerEffect> {
        if self.focus == Focus::Search && self.matches.len() != 1 {
            self.focus = Focus::List;
            return ComponentUpdate::render(RenderRequest::Immediate);
        }
        let Some(index) = self.matches.get(self.selected) else {
            return ComponentUpdate::none();
        };
        Self::effect(SessionPickerEffect::Resume(
            self.sessions[*index].session_id.clone(),
        ))
    }

    fn effect(effect: SessionPickerEffect) -> ComponentUpdate<SessionPickerEffect> {
        ComponentUpdate {
            effects: vec![effect],
            render: RenderRequest::Immediate,
        }
    }

    fn refresh_matches(&mut self) {
        let query = self.query.to_ascii_lowercase();
        self.matches = self
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, session)| session.matches(&query))
            .map(|(index, _)| index)
            .collect();
        self.selected = 0;
    }

    fn render_search(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        if area.is_empty() {
            return;
        }
        let focused = self.focus == Focus::Search;
        let marker = if focused { "› " } else { "  " };
        let prefix_width = marker.width() + SEARCH_LABEL.width();
        let query_width = usize::from(area.width).saturating_sub(prefix_width);
        let query = visible_tail(&self.query, query_width);
        let label_style = Style::default()
            .fg(if focused {
                theme.accent()
            } else {
                theme.muted()
            })
            .add_modifier(if focused {
                Modifier::BOLD
            } else {
                Modifier::empty()
            });
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(marker, label_style),
                Span::styled(SEARCH_LABEL, label_style),
                Span::styled(query, Style::default().fg(theme.text())),
            ])),
            area,
        );
        if focused {
            let x = area.x
                + u16::try_from(prefix_width + query.width())
                    .unwrap_or(u16::MAX)
                    .min(area.width.saturating_sub(1));
            frame.set_cursor_position(Position::new(x, area.y));
        }
    }

    fn render_sessions(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        if area.is_empty() {
            return;
        }
        if self.matches.is_empty() {
            frame.render_widget(
                Paragraph::new("  No resumable sessions found")
                    .style(Style::default().fg(theme.muted())),
                area,
            );
            return;
        }
        let items = self.matches.iter().map(|index| {
            let session = &self.sessions[*index];
            let title = format!(
                "{} · {}",
                format_age(session.started_at_unix_ms),
                session.session_id,
            );
            let detail = format!(
                "{} · {} · {:?} · {}",
                session.preview,
                session.model,
                session.effort,
                session.workspace.display()
            );
            ListItem::new(vec![
                Line::from(Span::styled(
                    title,
                    Style::default()
                        .fg(theme.text())
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(detail, Style::default().fg(theme.muted()))),
            ])
        });
        let focused = self.focus == Focus::List;
        let list = List::new(items)
            .highlight_symbol(if focused { "› " } else { "  " })
            .highlight_style(if focused {
                Style::default().fg(theme.accent())
            } else {
                Style::default()
            });
        let selected = (!self.matches.is_empty()).then_some(self.selected);
        let mut state = ListState::default().with_selected(selected);
        frame.render_stateful_widget(list, area, &mut state);
        if focused && selected.is_some() {
            let visible = self.selected.saturating_sub(state.offset());
            let row = area.y + u16::try_from(visible.saturating_mul(2)).unwrap_or(u16::MAX);
            frame.set_cursor_position(Position::new(area.x, row.min(area.bottom() - 1)));
        }
    }
}

impl SessionSummary {
    fn matches(&self, query: &str) -> bool {
        query.is_empty()
            || self.session_id.to_ascii_lowercase().contains(query)
            || self.preview.to_ascii_lowercase().contains(query)
            || self.model.to_ascii_lowercase().contains(query)
            || self
                .workspace
                .to_string_lossy()
                .to_ascii_lowercase()
                .contains(query)
    }
}

impl Component for SessionPicker {
    type Event = SessionPickerEvent;
    type Effect = SessionPickerEffect;

    fn update(&mut self, event: Self::Event) -> ComponentUpdate<Self::Effect> {
        match event {
            SessionPickerEvent::Terminal(Event::Key(key)) => self.update_key(key),
            SessionPickerEvent::Terminal(Event::Paste(text)) => self.insert_paste(&text),
            SessionPickerEvent::Terminal(_) => ComponentUpdate::none(),
        }
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let layout =
            Floating::new("Resume session", 76, 18, &KEY_BINDINGS).render(frame, area, theme);
        if layout.body.is_empty() {
            return;
        }
        let search = Rect {
            height: 1,
            ..layout.body
        };
        let sessions = Rect {
            y: layout.body.y + 1,
            height: layout.body.height.saturating_sub(1),
            ..layout.body
        };
        self.render_search(frame, search, theme);
        self.render_sessions(frame, sessions, theme);
    }
}

fn visible_tail(query: &str, width: usize) -> &str {
    let mut used = 0;
    for (index, grapheme) in query.grapheme_indices(true).rev() {
        used += grapheme.width();
        if used > width {
            return &query[index + grapheme.len()..];
        }
    }
    query
}

#[cfg(test)]
mod tests {
    use super::{Component, SessionPicker, SessionPickerEffect, SessionPickerEvent};
    use crate::{config::ReasoningEffort, tui::session::SessionSummary};
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use std::path::PathBuf;

    fn key(code: KeyCode) -> SessionPickerEvent {
        SessionPickerEvent::Terminal(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn summary(id: &str, preview: &str) -> SessionSummary {
        SessionSummary {
            session_id: id.to_owned(),
            started_at_unix_ms: 1,
            model: "gpt".to_owned(),
            effort: ReasoningEffort::Medium,
            workspace: PathBuf::from("/work"),
            preview: preview.to_owned(),
        }
    }

    #[test]
    fn search_selects_a_session_by_preview() {
        let mut picker = SessionPicker::new(vec![
            summary("one", "fix parser"),
            summary("two", "write docs"),
        ]);
        for character in "docs".chars() {
            picker.update(key(KeyCode::Char(character)));
        }
        assert_eq!(
            picker.update(key(KeyCode::Enter)).effects,
            [SessionPickerEffect::Resume("two".to_owned())]
        );
    }
}
