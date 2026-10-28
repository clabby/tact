//! Searchable picker for resumable persisted sessions.
//!
//! [SessionPicker] keeps the [SessionSummary] list, its own search query, the
//! active [SessionListScope] (every checkout of the repository by default), the
//! matching indices, the highlighted row, and a [SessionPickerMode] that decides
//! what selection means. It consumes typed characters, pastes, and keys through
//! [SessionPickerEvent::Terminal]; Ctrl+F toggles the scope. Enter or Tab emits
//! [SessionPickerEffect::Resume] or [SessionPickerEffect::Mention] with the
//! highlighted session id, depending on the mode. Esc, or Backspace on an empty
//! query, emits [SessionPickerEffect::Dismiss].
//!
//! Sessions are ranked by [search_sessions]. A query long enough to search
//! conversations emits [SessionPickerEffect::Search]; the host answers with
//! [SessionPicker::set_conversation_matches], and until then the picker matches
//! the sessions' own fields only.

use super::{
    floating::Floating,
    node::{Component, ComponentUpdate, RenderRequest},
};
use crate::{
    app::theme::Theme,
    core::{
        session::{ConversationMatches, SessionListScope, SessionSummary, search_sessions},
        storage::search_terms,
    },
    tui::format::{format_age, sanitize_terminal_text_inline, shorten_home},
};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};
use std::path::PathBuf;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const RESUME_KEY_BINDINGS: [(&str, &str); 4] = [
    ("↑↓", "move"),
    ("enter/tab", "resume"),
    ("ctrl+f", "scope"),
    ("esc", "close"),
];
const MENTION_KEY_BINDINGS: [(&str, &str); 4] = [
    ("↑↓", "move"),
    ("enter/tab", "insert"),
    ("ctrl+f", "scope"),
    ("esc", "close"),
];
const SEARCH_LABEL: &str = "Search: ";

pub(super) enum SessionPickerEvent {
    Terminal(Event),
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum SessionPickerEffect {
    Dismiss,
    Resume(String),
    Mention(String),
    /// Search the conversations of stored sessions for this query.
    Search(String),
}

#[derive(Clone, Copy)]
pub(super) enum SessionPickerMode {
    Resume,
    Mention,
}

pub(super) struct SessionPicker {
    sessions: Vec<SessionSummary>,
    /// The workspace of the pane the picker was opened from, which
    /// [SessionListScope::Workspace] keeps sessions of.
    workspace: PathBuf,
    scope: SessionListScope,
    query: String,
    matches: Vec<usize>,
    selected: usize,
    mode: SessionPickerMode,
    /// Conversation matches and the query they answer; ignored once the query changes.
    conversation: (String, ConversationMatches),
}

impl SessionPicker {
    pub(super) fn new(
        sessions: Vec<SessionSummary>,
        mode: SessionPickerMode,
        workspace: PathBuf,
    ) -> Self {
        let matches = (0..sessions.len()).collect();
        Self {
            sessions,
            workspace,
            scope: SessionListScope::default(),
            query: String::new(),
            matches,
            selected: 0,
            mode,
            conversation: Default::default(),
        }
    }

    /// Applies the conversation matches for `query` unless the query has changed since. The
    /// highlighted session stays highlighted when it still matches.
    pub(super) fn set_conversation_matches(
        &mut self,
        query: String,
        matches: ConversationMatches,
    ) -> ComponentUpdate<SessionPickerEffect> {
        if query != self.query {
            return ComponentUpdate::none();
        }
        self.conversation = (query, matches);
        self.refresh_matches_keeping_highlight();
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    pub(super) fn update_title(&mut self, id: &str, title: &str) -> bool {
        let Some(session) = self
            .sessions
            .iter_mut()
            .find(|session| session.session_id == id)
        else {
            return false;
        };
        if session.preview == title {
            return false;
        }
        title.clone_into(&mut session.preview);
        let previous_row = self.selected;
        let selected_session = self.matches.get(previous_row).copied();
        self.refresh_matches();
        self.selected = selected_session
            .and_then(|index| {
                self.matches
                    .iter()
                    .position(|candidate| *candidate == index)
            })
            .unwrap_or_else(|| previous_row.min(self.matches.len().saturating_sub(1)));
        true
    }

    fn update_key(&mut self, key: KeyEvent) -> ComponentUpdate<SessionPickerEffect> {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return ComponentUpdate::none();
        }
        match key.code {
            KeyCode::Esc => Self::effect(SessionPickerEffect::Dismiss),
            KeyCode::Backspace if !self.query.is_empty() => {
                if let Some((index, _)) = self.query.grapheme_indices(true).next_back() {
                    self.query.truncate(index);
                    return self.query_changed();
                }
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            KeyCode::Backspace => Self::effect(SessionPickerEffect::Dismiss),
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            KeyCode::Down => {
                if !self.matches.is_empty() {
                    self.selected = (self.selected + 1).min(self.matches.len() - 1);
                }
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            KeyCode::Enter | KeyCode::Tab => self.select(),
            KeyCode::Char('f') if key.modifiers == KeyModifiers::CONTROL => {
                self.scope = self.scope.toggled();
                self.refresh_matches_keeping_highlight();
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.query.push(character);
                self.query_changed()
            }
            _ => ComponentUpdate::none(),
        }
    }

    fn insert_paste(&mut self, text: &str) -> ComponentUpdate<SessionPickerEffect> {
        self.query
            .extend(text.chars().filter(|character| !character.is_control()));
        self.query_changed()
    }

    fn query_changed(&mut self) -> ComponentUpdate<SessionPickerEffect> {
        self.refresh_matches();
        self.selected = 0;
        if search_terms(&self.query).next().is_none() {
            return ComponentUpdate::render(RenderRequest::Immediate);
        }
        Self::effect(SessionPickerEffect::Search(self.query.clone()))
    }

    fn select(&mut self) -> ComponentUpdate<SessionPickerEffect> {
        let Some(index) = self.matches.get(self.selected) else {
            return ComponentUpdate::none();
        };
        let session_id = self.sessions[*index].session_id.clone();
        let effect = match self.mode {
            SessionPickerMode::Resume => SessionPickerEffect::Resume(session_id),
            SessionPickerMode::Mention => SessionPickerEffect::Mention(session_id),
        };
        Self::effect(effect)
    }

    fn effect(effect: SessionPickerEffect) -> ComponentUpdate<SessionPickerEffect> {
        ComponentUpdate {
            effects: vec![effect],
            render: RenderRequest::Immediate,
        }
    }

    fn refresh_matches(&mut self) {
        let empty = ConversationMatches::new();
        let conversation = self.current_conversation().unwrap_or(&empty);
        self.matches = search_sessions(&self.sessions, &self.query, conversation);
        if self.scope == SessionListScope::Workspace {
            self.matches
                .retain(|&index| self.sessions[index].workspace == self.workspace);
        }
    }

    /// Refreshes the matches while the highlighted session stays highlighted, when it still
    /// matches.
    fn refresh_matches_keeping_highlight(&mut self) {
        let highlighted = self.matches.get(self.selected).copied();
        self.refresh_matches();
        self.selected = highlighted
            .and_then(|index| self.matches.iter().position(|&found| found == index))
            .unwrap_or(0);
    }

    fn current_conversation(&self) -> Option<&ConversationMatches> {
        let (query, conversation) = &self.conversation;
        (*query == self.query).then_some(conversation)
    }

    fn render_search(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        if area.is_empty() {
            return;
        }
        let marker = "  ";
        let prefix_width = marker.width() + SEARCH_LABEL.width();
        let query_width = usize::from(area.width).saturating_sub(prefix_width);
        let query = visible_tail(&self.query, query_width);
        let label_style = Style::default().fg(theme.muted());
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(marker, label_style),
                Span::styled(SEARCH_LABEL, label_style),
                Span::styled(query, Style::default().fg(theme.text())),
            ])),
            area,
        );
    }

    fn render_scope(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        if area.is_empty() {
            return;
        }
        let muted = Style::default().fg(theme.muted());
        let label = match self.scope {
            SessionListScope::Repository => "All checkouts",
            SessionListScope::Workspace => "This workspace",
        };
        let mut spans = vec![
            Span::raw("  "),
            Span::styled("Scope: ", muted),
            Span::styled(
                label,
                Style::default()
                    .fg(theme.text())
                    .add_modifier(Modifier::BOLD),
            ),
        ];
        if self.scope == SessionListScope::Workspace {
            spans.push(Span::styled(" · ", muted));
            // The end of a path names the checkout, so a path that does not fit loses its start.
            let path = shorten_home(&self.workspace);
            let used = spans.iter().map(Span::width).sum::<usize>();
            let available = usize::from(area.width).saturating_sub(used);
            let path = if path.width() > available {
                format!("…{}", visible_tail(&path, available.saturating_sub(1)))
            } else {
                path
            };
            spans.push(Span::styled(path, muted));
        }
        let block = Block::new()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(theme.border()));
        frame.render_widget(Paragraph::new(Line::from(spans)).block(block), area);
    }

    fn render_sessions(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        if area.is_empty() {
            return;
        }
        if self.matches.is_empty() {
            let message = match (self.mode, self.scope) {
                (SessionPickerMode::Resume, SessionListScope::Repository) => {
                    "  No resumable sessions found"
                }
                (SessionPickerMode::Resume, SessionListScope::Workspace) => {
                    "  No resumable sessions in this workspace"
                }
                (SessionPickerMode::Mention, SessionListScope::Repository) => {
                    "  No other sessions found"
                }
                (SessionPickerMode::Mention, SessionListScope::Workspace) => {
                    "  No other sessions in this workspace"
                }
            };
            frame.render_widget(
                Paragraph::new(message).style(Style::default().fg(theme.muted())),
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
            let mut lines = vec![
                Line::from(Span::styled(
                    title,
                    Style::default()
                        .fg(theme.text())
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(detail, Style::default().fg(theme.muted()))),
            ];
            let found = self
                .current_conversation()
                .and_then(|conversation| conversation.get(&session.session_id));
            if let Some(found) = found {
                lines.push(Line::from(Span::styled(
                    format!("“{}”", sanitize_terminal_text_inline(&found.snippet)),
                    Style::default()
                        .fg(theme.muted())
                        .add_modifier(Modifier::ITALIC),
                )));
            }
            ListItem::new(lines)
        });
        let list = List::new(items)
            .highlight_symbol("› ")
            .highlight_style(Style::default().fg(theme.accent()));
        let selected = (!self.matches.is_empty()).then_some(self.selected);
        let mut state = ListState::default().with_selected(selected);
        frame.render_stateful_widget(list, area, &mut state);
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
        let (title, key_bindings) = match self.mode {
            SessionPickerMode::Resume => ("Resume session", &RESUME_KEY_BINDINGS),
            SessionPickerMode::Mention => ("Mention session", &MENTION_KEY_BINDINGS),
        };
        let layout = Floating::new(title, 76, 18, key_bindings).render(frame, area, theme);
        if layout.body.is_empty() {
            return;
        }
        let search = Rect {
            height: 1,
            ..layout.body
        };
        let scope = Rect {
            y: search.bottom(),
            height: layout.body.height.saturating_sub(search.height).min(2),
            ..layout.body
        };
        let sessions = Rect {
            y: scope.bottom(),
            height: layout
                .body
                .height
                .saturating_sub(search.height + scope.height),
            ..layout.body
        };
        self.render_search(frame, search, theme);
        self.render_scope(frame, scope, theme);
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
    use super::{
        Component, SessionPicker, SessionPickerEffect, SessionPickerEvent, SessionPickerMode,
    };
    use crate::{
        app::{
            config::{ReasoningEffort, ReasoningMode},
            theme::Theme,
        },
        core::{session::SessionSummary, storage::ConversationMatch},
    };
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend};
    use std::path::PathBuf;

    fn key(code: KeyCode) -> SessionPickerEvent {
        SessionPickerEvent::Terminal(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn ctrl_f() -> SessionPickerEvent {
        SessionPickerEvent::Terminal(Event::Key(KeyEvent::new(
            KeyCode::Char('f'),
            KeyModifiers::CONTROL,
        )))
    }

    fn summary(id: &str, preview: &str) -> SessionSummary {
        SessionSummary {
            session_id: id.to_owned(),
            started_at_unix_ms: 1,
            model: "gpt".to_owned(),
            effort: ReasoningEffort::Medium,
            reasoning_mode: ReasoningMode::Standard,
            workspace: PathBuf::from("/work"),
            preview: preview.to_owned(),
            title_pending: false,
            snippet: None,
        }
    }

    fn summary_in(id: &str, preview: &str, workspace: &str) -> SessionSummary {
        SessionSummary {
            workspace: PathBuf::from(workspace),
            ..summary(id, preview)
        }
    }

    /// A picker opened from a pane working in `/work`.
    fn open_picker(sessions: Vec<SessionSummary>, mode: SessionPickerMode) -> SessionPicker {
        SessionPicker::new(sessions, mode, PathBuf::from("/work"))
    }

    fn render(picker: &mut SessionPicker, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| picker.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|row| {
                (0..width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn ctrl_f_toggles_between_all_checkouts_and_this_workspace() {
        let mut picker = open_picker(
            vec![
                summary_in("here", "fix parser", "/work"),
                summary_in("elsewhere", "fix lexer", "/work-feature"),
                summary_in("also-here", "write docs", "/work"),
            ],
            SessionPickerMode::Resume,
        );
        picker.update(key(KeyCode::Down));
        picker.update(key(KeyCode::Down));

        picker.update(ctrl_f());
        assert_eq!(picker.matches, [0, 2]);
        assert_eq!(
            picker.update(key(KeyCode::Enter)).effects,
            [SessionPickerEffect::Resume("also-here".to_owned())],
            "the highlighted session stays highlighted"
        );
        for character in "fix".chars() {
            picker.update(key(KeyCode::Char(character)));
        }
        assert_eq!(picker.matches, [0], "search stays within the scope");

        picker.update(ctrl_f());
        assert_eq!(picker.matches, [0, 1]);
    }

    #[test]
    fn this_workspace_scope_names_the_workspace_and_keeps_the_end_of_a_long_path() {
        let workspace = "/srv/checkouts/tact/worktrees/resume-picker-scope-and-wrapping";
        let mut picker = SessionPicker::new(
            vec![summary_in("one", "fix parser", workspace)],
            SessionPickerMode::Resume,
            PathBuf::from(workspace),
        );
        let scope_row = |picker: &mut SessionPicker| {
            render(picker, 70, 24)
                .into_iter()
                .find(|row| row.contains("Scope:"))
                .unwrap()
        };

        assert!(scope_row(&mut picker).contains("Scope: All checkouts"));
        picker.update(ctrl_f());
        let row = scope_row(&mut picker);
        assert!(row.contains("Scope: This workspace · …"), "{row}");
        assert!(row.contains("/resume-picker-scope-and-wrapping│"), "{row}");
    }

    #[test]
    fn conversation_matches_for_the_current_query_join_the_results() {
        let mut picker = open_picker(
            vec![summary("one", "fix parser"), summary("two", "write docs")],
            SessionPickerMode::Resume,
        );
        assert!(picker.update(key(KeyCode::Char('l'))).effects.is_empty());
        picker.update(key(KeyCode::Char('e')));
        assert_eq!(
            picker.update(key(KeyCode::Char('x'))).effects,
            [SessionPickerEffect::Search("lex".to_owned())]
        );
        assert!(picker.matches.is_empty());
        let found = |session_id: &str| {
            let found = ConversationMatch {
                rank: -1.0,
                snippet: "the lexer".to_owned(),
            };
            [(session_id.to_owned(), found)].into_iter().collect()
        };

        picker.set_conversation_matches("le".to_owned(), found("one"));
        assert!(picker.matches.is_empty(), "an answer to an earlier query");
        picker.set_conversation_matches("lex".to_owned(), found("two"));
        assert_eq!(picker.matches, [1]);
        assert_eq!(
            picker.update(key(KeyCode::Enter)).effects,
            [SessionPickerEffect::Resume("two".to_owned())]
        );
        picker.update(key(KeyCode::Char('e')));
        assert!(
            picker.matches.is_empty(),
            "matches for \"lex\" do not answer \"lexe\""
        );
    }

    #[test]
    fn title_updates_refresh_search_and_preserve_the_selected_session() {
        let mut picker = open_picker(
            vec![summary("one", "fix parser"), summary("two", "write docs")],
            SessionPickerMode::Resume,
        );
        picker.update(key(KeyCode::Down));
        assert!(picker.update_title("one", "new parser title"));
        assert_eq!(
            picker.select().effects,
            [SessionPickerEffect::Resume("two".to_owned())]
        );
        assert!(!picker.update_title("one", "new parser title"));
        assert!(!picker.update_title("missing", "unused"));

        picker.update(SessionPickerEvent::Terminal(Event::Paste(
            "updated".to_owned(),
        )));
        assert!(picker.matches.is_empty());
        assert!(picker.update_title("two", "updated docs"));
        assert_eq!(
            picker.select().effects,
            [SessionPickerEffect::Resume("two".to_owned())]
        );
        assert!(picker.update_title("two", "renamed docs"));
        assert!(picker.matches.is_empty());
        assert_eq!(picker.selected, 0);
    }

    #[test]
    fn search_selects_a_session_by_preview() {
        let mut picker = open_picker(
            vec![summary("one", "fix parser"), summary("two", "write docs")],
            SessionPickerMode::Resume,
        );
        for character in "docs".chars() {
            picker.update(key(KeyCode::Char(character)));
        }
        assert_eq!(
            picker.update(key(KeyCode::Enter)).effects,
            [SessionPickerEffect::Resume("two".to_owned())]
        );
    }

    #[test]
    fn mention_mode_returns_a_reference_instead_of_resuming() {
        let mut picker = open_picker(
            vec![summary("one", "fix parser")],
            SessionPickerMode::Mention,
        );

        assert_eq!(
            picker.update(key(KeyCode::Enter)).effects,
            [SessionPickerEffect::Mention("one".to_owned())]
        );
    }

    #[test]
    fn tab_resumes_the_selected_session() {
        let mut picker = open_picker(
            vec![summary("one", "fix parser"), summary("two", "write docs")],
            SessionPickerMode::Resume,
        );
        for character in "docs".chars() {
            picker.update(key(KeyCode::Char(character)));
        }

        assert_eq!(
            picker.update(key(KeyCode::Tab)).effects,
            [SessionPickerEffect::Resume("two".to_owned())]
        );
    }

    #[test]
    fn arrows_navigate_while_typing_continues_to_search() {
        let mut picker = open_picker(
            vec![summary("one", "fix parser"), summary("two", "write docs")],
            SessionPickerMode::Resume,
        );

        picker.update(key(KeyCode::Down));
        assert_eq!(
            picker.update(key(KeyCode::Enter)).effects,
            [SessionPickerEffect::Resume("two".to_owned())]
        );

        for character in "fix".chars() {
            picker.update(key(KeyCode::Char(character)));
        }
        assert_eq!(picker.query, "fix");
        assert_eq!(picker.matches, [0]);
        assert_eq!(picker.selected, 0);
    }
}
