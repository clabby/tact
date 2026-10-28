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
//!
//! Rows wrap rather than clip, so a session's workspace path is always visible.
//! Only its title and conversation match are capped, because a session without
//! a generated title uses its whole first prompt as one.

use super::{
    fit::wrap,
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
use std::path::{Path, PathBuf};
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
const SCOPE_LABEL: &str = "  Scope: ";
const REPOSITORY_SCOPE: &str = "All checkouts";
const WORKSPACE_SCOPE: &str = "This workspace";
const SEPARATOR: &str = " · ";
const HIGHLIGHT_SYMBOL: &str = "› ";
/// The narrowest the picker gets, so that a short list does not look cramped.
const MIN_WIDTH: u16 = 76;
/// The widest the picker gets. Longer headings and paths wrap.
const MAX_WIDTH: u16 = 120;
const HEIGHT: u16 = 28;
/// The most rows a session's title takes before it is cut.
const TITLE_ROWS: usize = 2;
/// The most rows a conversation match takes before it is cut.
const SNIPPET_ROWS: usize = 2;

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
    /// Fixed when the picker opens, so that searching and switching scopes do not resize it.
    width: u16,
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
        let width = Self::fitting_width(&sessions, &workspace);
        Self {
            sessions,
            workspace,
            scope: SessionListScope::default(),
            width,
            query: String::new(),
            matches,
            selected: 0,
            mode,
            conversation: Default::default(),
        }
    }

    /// The width that fits every session's heading and workspace path, and the scope row naming
    /// `workspace`, between [MIN_WIDTH] and [MAX_WIDTH]. Titles wrap to whatever width this
    /// leaves.
    fn fitting_width(sessions: &[SessionSummary], workspace: &Path) -> u16 {
        let widest_row = sessions
            .iter()
            .flat_map(|session| {
                [
                    heading(session).width(),
                    shorten_home(&session.workspace).width(),
                ]
            })
            .max()
            .unwrap_or(0);
        let scope_row = [SCOPE_LABEL, WORKSPACE_SCOPE, SEPARATOR]
            .iter()
            .map(|text| text.width())
            .sum::<usize>()
            + shorten_home(workspace).width();
        let borders = 2;
        let width = (HIGHLIGHT_SYMBOL.width() + widest_row).max(scope_row) + borders;
        u16::try_from(width)
            .unwrap_or(u16::MAX)
            .clamp(MIN_WIDTH, MAX_WIDTH)
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
            SessionListScope::Repository => REPOSITORY_SCOPE,
            SessionListScope::Workspace => WORKSPACE_SCOPE,
        };
        let mut spans = vec![
            Span::styled(SCOPE_LABEL, muted),
            Span::styled(
                label,
                Style::default()
                    .fg(theme.text())
                    .add_modifier(Modifier::BOLD),
            ),
        ];
        if self.scope == SessionListScope::Workspace {
            spans.push(Span::styled(SEPARATOR, muted));
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
        let width = usize::from(area.width).saturating_sub(HIGHLIGHT_SYMBOL.width());
        let items = self
            .matches
            .iter()
            .map(|index| self.session_item(&self.sessions[*index], width, theme));
        let list = List::new(items)
            .highlight_symbol(HIGHLIGHT_SYMBOL)
            .highlight_style(Style::default().fg(theme.accent()));
        let selected = (!self.matches.is_empty()).then_some(self.selected);
        let mut state = ListState::default().with_selected(selected);
        frame.render_stateful_widget(list, area, &mut state);
    }

    /// A session's rows, wrapped to `width`: a heading, the title, the workspace when the scope
    /// spans several, and the conversation match when a search found one.
    fn session_item(
        &self,
        session: &SessionSummary,
        width: usize,
        theme: &Theme,
    ) -> ListItem<'static> {
        let muted = Style::default().fg(theme.muted());
        let mut lines = Vec::new();
        let mut push = |text: &str, max_rows: usize, style: Style| {
            lines.extend(
                wrap(text, width, max_rows)
                    .into_iter()
                    .map(|row| Line::styled(row, style)),
            );
        };
        push(
            &heading(session),
            usize::MAX,
            Style::default()
                .fg(theme.text())
                .add_modifier(Modifier::BOLD),
        );
        push(&session.preview, TITLE_ROWS, muted);
        if self.scope == SessionListScope::Repository {
            push(&shorten_home(&session.workspace), usize::MAX, muted);
        }
        let found = self
            .current_conversation()
            .and_then(|conversation| conversation.get(&session.session_id));
        if let Some(found) = found {
            push(
                &format!("“{}”", sanitize_terminal_text_inline(&found.snippet)),
                SNIPPET_ROWS,
                muted.add_modifier(Modifier::ITALIC),
            );
        }
        ListItem::new(lines)
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
        let layout =
            Floating::new(title, self.width, HEIGHT, key_bindings).render(frame, area, theme);
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

/// A session's first row: its age, model, effort, and id.
fn heading(session: &SessionSummary) -> String {
    format!(
        "{}{SEPARATOR}{}{SEPARATOR}{:?}{SEPARATOR}{}",
        format_age(session.started_at_unix_ms),
        session.model,
        session.effort,
        session.session_id,
    )
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
        Component, MAX_WIDTH, MIN_WIDTH, SessionPicker, SessionPickerEffect, SessionPickerEvent,
        SessionPickerMode,
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
    fn long_rows_wrap_so_the_workspace_stays_visible() {
        let workspace = "/srv/checkouts/tact/worktrees/resume-picker-scope-and-wrapping";
        let title = "why does the resume picker cut off long rows? ".repeat(5);
        let mut picker = SessionPicker::new(
            vec![summary_in("one", &title, workspace)],
            SessionPickerMode::Resume,
            PathBuf::from(workspace),
        );

        let rows = render(&mut picker, 70, 24);
        assert!(rows.iter().any(|row| row.contains("Scope: All checkouts")));
        assert!(rows.iter().any(|row| row.contains(workspace)));
        assert_eq!(
            rows.iter().filter(|row| row.contains("cut off")).count(),
            2,
            "a long title is cut after two rows"
        );
        assert_eq!(rows.iter().filter(|row| row.contains('…')).count(), 1);

        picker.update(ctrl_f());
        let rows = render(&mut picker, 70, 24);
        let mentions = rows
            .iter()
            .filter(|row| row.contains("scope-and-wrapping"))
            .collect::<Vec<_>>();
        assert_eq!(mentions.len(), 1, "only the scope row names the workspace");
        assert!(mentions[0].contains("Scope: This workspace"));
    }

    #[test]
    fn the_picker_fits_its_widest_heading_or_path() {
        let picker_width = |workspace: &str| {
            let mut picker = open_picker(
                vec![summary_in("one", "fix parser", workspace)],
                SessionPickerMode::Resume,
            );
            let rows = render(&mut picker, 200, 30);
            let top = rows
                .iter()
                .find(|row| row.contains("Resume session"))
                .unwrap();
            top.trim().chars().count()
        };

        assert_eq!(picker_width("/work"), usize::from(MIN_WIDTH));
        let path = format!("/srv/{}", "a".repeat(85));
        assert_eq!(
            picker_width(&path),
            2 + path.len() + 2,
            "the highlight marker, the path, and the borders"
        );
        let path = format!("/srv/{}", "a".repeat(300));
        assert_eq!(picker_width(&path), usize::from(MAX_WIDTH));
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
