//! Overlay listing the live sessions (panes) of this Tact process.
//!
//! [SessionsOverlay] keeps a host-supplied [LiveSession] snapshot, whether a new
//! session may start, its own search query, the visible rows, and the highlighted
//! row. It consumes typed characters, pastes, and keys through
//! [SessionsEvent::Terminal], and the host refreshes the snapshot in place with
//! [SessionsOverlay::set_sessions].
//!
//! - Enter or Tab emits [SessionsEffect::Activate] for a session row, or
//!   [SessionsEffect::New] for the "New session" row when starting one is
//!   allowed.
//! - Ctrl+D or Delete emits [SessionsEffect::Close] for the highlighted session,
//!   and Esc emits [SessionsEffect::Cancel].
//! - The "New session" row always leads the list, even when the query filters
//!   out every session. When starting a session is not allowed, the row stays
//!   visible with the reason and selecting it emits nothing.

use super::{
    fit::ellipsize,
    floating::Floating,
    node::{Component, ComponentUpdate, RenderRequest},
};
use crate::{app::theme::Theme, core::pane::PaneId, tui::format::sanitize_terminal_text_inline};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const KEY_BINDINGS: [(&str, &str); 4] = [
    ("↑↓", "move"),
    ("enter", "open"),
    ("ctrl+d", "close session"),
    ("esc", "close"),
];
const SEARCH_LABEL: &str = "Search: ";
const NEW_SESSION_LABEL: &str = "New session";
const HIGHLIGHT_SYMBOL_WIDTH: usize = 2;

/// One live session as shown in the overlay.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LiveSession {
    pub(crate) pane: PaneId,
    /// First prompt of the session, or a placeholder when it has none.
    pub(crate) title: String,
    /// Display label of the session's model.
    pub(crate) model: String,
    pub(crate) running: bool,
    /// The session finished or failed while it was not displayed.
    pub(crate) unread: bool,
    pub(crate) has_draft: bool,
    /// The session is drawn in the terminal.
    pub(crate) displayed: bool,
    /// The session receives keyboard input.
    pub(crate) focused: bool,
}

pub(super) enum SessionsEvent {
    Terminal(Event),
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum SessionsEffect {
    Activate(PaneId),
    New,
    Close(PaneId),
    Cancel,
}

/// A selectable row. The "New session" row is always present so that filtering
/// never removes the way to start a session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Row {
    New,
    Session(usize),
}

pub(super) struct SessionsOverlay {
    sessions: Vec<LiveSession>,
    /// `Err` carries the reason a new session cannot be started.
    new_session: Result<(), String>,
    query: String,
    rows: Vec<Row>,
    selected: usize,
}

impl SessionsOverlay {
    pub(super) fn new(sessions: Vec<LiveSession>, new_session: Result<(), String>) -> Self {
        let mut overlay = Self {
            sessions,
            new_session,
            query: String::new(),
            rows: Vec::new(),
            selected: 0,
        };
        overlay.refresh_rows(None);
        overlay
    }

    /// Replaces the listed sessions while the overlay is open. The highlighted
    /// session stays highlighted when it still exists; otherwise the highlight
    /// is clamped to the remaining rows.
    pub(super) fn set_sessions(
        &mut self,
        sessions: Vec<LiveSession>,
        new_session: Result<(), String>,
    ) {
        let highlighted = self.selected_session().map(|session| session.pane);
        let previous = self.selected;
        self.sessions = sessions;
        self.new_session = new_session;
        self.refresh_rows(highlighted);
        if highlighted.is_none_or(|pane| self.selected_session().map(|s| s.pane) != Some(pane)) {
            self.selected = previous.min(self.rows.len() - 1);
        }
    }

    fn selected_session(&self) -> Option<&LiveSession> {
        match self.rows.get(self.selected)? {
            Row::New => None,
            Row::Session(index) => self.sessions.get(*index),
        }
    }

    fn update_key(&mut self, key: KeyEvent) -> ComponentUpdate<SessionsEffect> {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return ComponentUpdate::none();
        }
        match key.code {
            KeyCode::Esc => Self::effect(SessionsEffect::Cancel),
            KeyCode::Backspace => {
                if let Some((index, _)) = self.query.grapheme_indices(true).next_back() {
                    self.query.truncate(index);
                    self.refresh_rows(None);
                }
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(self.rows.len() - 1);
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            KeyCode::Enter | KeyCode::Tab => self.activate(),
            KeyCode::Char('d') if key.modifiers == KeyModifiers::CONTROL => self.close_selected(),
            KeyCode::Delete => self.close_selected(),
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.query.push(character);
                self.refresh_rows(None);
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            _ => ComponentUpdate::none(),
        }
    }

    fn insert_paste(&mut self, text: &str) -> ComponentUpdate<SessionsEffect> {
        self.query
            .extend(text.chars().filter(|character| !character.is_control()));
        self.refresh_rows(None);
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn activate(&self) -> ComponentUpdate<SessionsEffect> {
        match self.rows.get(self.selected) {
            Some(Row::New) if self.new_session.is_ok() => Self::effect(SessionsEffect::New),
            Some(Row::Session(index)) => {
                Self::effect(SessionsEffect::Activate(self.sessions[*index].pane))
            }
            _ => ComponentUpdate::none(),
        }
    }

    fn close_selected(&self) -> ComponentUpdate<SessionsEffect> {
        self.selected_session()
            .map_or_else(ComponentUpdate::none, |session| {
                Self::effect(SessionsEffect::Close(session.pane))
            })
    }

    fn effect(effect: SessionsEffect) -> ComponentUpdate<SessionsEffect> {
        ComponentUpdate {
            effects: vec![effect],
            render: RenderRequest::Immediate,
        }
    }

    /// Rebuilds the visible rows for the current query. The highlight follows
    /// `keep` when it is still visible; otherwise it moves to the first
    /// matching session, or to "New session" when nothing matches.
    fn refresh_rows(&mut self, keep: Option<PaneId>) {
        let query = self.query.to_ascii_lowercase();
        self.rows = std::iter::once(Row::New)
            .chain(
                self.sessions
                    .iter()
                    .enumerate()
                    .filter(|(_, session)| session.matches(&query))
                    .map(|(index, _)| Row::Session(index)),
            )
            .collect();
        self.selected = keep
            .and_then(|pane| {
                self.rows.iter().position(
                    |row| matches!(row, Row::Session(index) if self.sessions[*index].pane == pane),
                )
            })
            .unwrap_or_else(|| usize::from(self.rows.len() > 1 && !query.is_empty()));
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

    fn render_rows(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        if area.is_empty() {
            return;
        }
        let width = usize::from(area.width).saturating_sub(HIGHLIGHT_SYMBOL_WIDTH);
        let items = self.rows.iter().map(|row| match row {
            Row::New => self.new_session_item(theme),
            Row::Session(index) => self.sessions[*index].item(width, theme),
        });
        let list = List::new(items)
            .highlight_symbol("› ")
            .highlight_style(Style::default().fg(theme.accent()));
        let mut state = ListState::default().with_selected(Some(self.selected));
        frame.render_stateful_widget(list, area, &mut state);
    }

    fn new_session_item(&self, theme: &Theme) -> ListItem<'static> {
        let muted = Style::default().fg(theme.muted());
        let mut spans = vec![Span::styled("+ ", muted)];
        match &self.new_session {
            Ok(()) => spans.push(Span::styled(
                NEW_SESSION_LABEL,
                Style::default()
                    .fg(theme.text())
                    .add_modifier(Modifier::BOLD),
            )),
            Err(reason) => {
                let reason = sanitize_terminal_text_inline(reason);
                spans.push(Span::styled(
                    format!("{NEW_SESSION_LABEL} · {reason}"),
                    muted,
                ));
            }
        }
        ListItem::new(Line::from(spans))
    }
}

impl LiveSession {
    fn matches(&self, query: &str) -> bool {
        query.is_empty()
            || self.title.to_ascii_lowercase().contains(query)
            || self.model.to_ascii_lowercase().contains(query)
    }

    /// Renders one row: state glyph, truncated title, then right-hand details.
    /// The title absorbs all width pressure so the details stay visible.
    fn item(&self, width: usize, theme: &Theme) -> ListItem<'static> {
        let muted = Style::default().fg(theme.muted());
        let accent = Style::default().fg(theme.accent());
        let (glyph, glyph_style) = if self.running {
            ("◐", accent)
        } else {
            ("○", muted)
        };

        let mut details = vec![Span::styled(format!(" · {}", self.model), muted)];
        if self.unread {
            details.push(Span::styled(" · unread", accent));
        }
        if self.has_draft {
            details.push(Span::styled(" · draft", muted));
        }
        if self.focused {
            details.push(Span::styled(" · focused", accent));
        } else if self.displayed {
            details.push(Span::styled(" · shown", muted));
        }
        let details_width: usize = details.iter().map(Span::width).sum();
        let title_width = width.saturating_sub(glyph.width() + 1 + details_width);

        let mut spans = vec![
            Span::styled(glyph, glyph_style),
            Span::raw(" "),
            Span::styled(
                ellipsize(sanitize_terminal_text_inline(&self.title), title_width).into_owned(),
                Style::default()
                    .fg(theme.text())
                    .add_modifier(if self.unread {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
        ];
        spans.extend(details);
        ListItem::new(Line::from(spans))
    }
}

impl Component for SessionsOverlay {
    type Event = SessionsEvent;
    type Effect = SessionsEffect;

    fn update(&mut self, event: Self::Event) -> ComponentUpdate<Self::Effect> {
        match event {
            SessionsEvent::Terminal(Event::Key(key)) => self.update_key(key),
            SessionsEvent::Terminal(Event::Paste(text)) => self.insert_paste(&text),
            SessionsEvent::Terminal(_) => ComponentUpdate::none(),
        }
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let layout = Floating::new("Sessions", 76, 18, &KEY_BINDINGS).render(frame, area, theme);
        if layout.body.is_empty() {
            return;
        }
        let search = Rect {
            height: 1,
            ..layout.body
        };
        let rows = Rect {
            y: layout.body.y + 1,
            height: layout.body.height.saturating_sub(1),
            ..layout.body
        };
        self.render_search(frame, search, theme);
        self.render_rows(frame, rows, theme);
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
    use super::{Component, LiveSession, SessionsEffect, SessionsEvent, SessionsOverlay};
    use crate::{app::theme::Theme, core::pane::PaneId};
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend};

    fn press(code: KeyCode, modifiers: KeyModifiers) -> SessionsEvent {
        SessionsEvent::Terminal(Event::Key(KeyEvent::new(code, modifiers)))
    }

    fn key(code: KeyCode) -> SessionsEvent {
        press(code, KeyModifiers::NONE)
    }

    fn session(pane: PaneId, title: &str, model: &str) -> LiveSession {
        LiveSession {
            pane,
            title: title.to_owned(),
            model: model.to_owned(),
            running: false,
            unread: false,
            has_draft: false,
            displayed: false,
            focused: false,
        }
    }

    fn sessions() -> Vec<LiveSession> {
        vec![
            session(PaneId::Main, "fix parser", "gpt"),
            session(PaneId::Fork(1), "write docs", "claude"),
            session(PaneId::Fork(2), "refactor", "gpt"),
        ]
    }

    fn type_text(overlay: &mut SessionsOverlay, text: &str) {
        for character in text.chars() {
            overlay.update(key(KeyCode::Char(character)));
        }
    }

    fn render(overlay: &mut SessionsOverlay, width: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
        terminal
            .draw(|frame| overlay.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn rows_show_state_markers_title_and_model() {
        let mut live = sessions();
        live[0].running = true;
        live[0].focused = true;
        live[0].displayed = true;
        live[1].unread = true;
        live[1].has_draft = true;
        live[2].displayed = true;
        let mut overlay = SessionsOverlay::new(live, Ok(()));

        let rendered = render(&mut overlay, 90);

        assert!(rendered.contains("+ New session"));
        assert!(rendered.contains("◐ fix parser · gpt · focused"));
        assert!(rendered.contains("○ write docs · claude · unread · draft"));
        assert!(rendered.contains("○ refactor · gpt · shown"));
        assert!(rendered.contains("ctrl+d close session"));
    }

    #[test]
    fn long_titles_are_truncated_to_one_line_and_keep_details() {
        let mut live = vec![session(
            PaneId::Main,
            &format!("first line {}\nsecond line", "x".repeat(200)),
            "gpt",
        )];
        live[0].unread = true;
        let mut overlay = SessionsOverlay::new(live, Ok(()));

        let rendered = render(&mut overlay, 90);

        assert!(rendered.contains("…"));
        assert!(!rendered.contains("second line"));
        assert!(rendered.contains("· gpt · unread"));
    }

    #[test]
    fn unavailable_new_session_shows_its_reason_and_does_nothing() {
        let mut overlay = SessionsOverlay::new(sessions(), Err("limit of 8 sessions".to_owned()));

        assert!(render(&mut overlay, 90).contains("New session · limit of 8 sessions"));
        assert!(overlay.update(key(KeyCode::Enter)).effects.is_empty());
    }

    #[test]
    fn enter_on_the_new_row_starts_a_session() {
        let mut overlay = SessionsOverlay::new(sessions(), Ok(()));

        assert_eq!(
            overlay.update(key(KeyCode::Enter)).effects,
            [SessionsEffect::New]
        );
    }

    #[test]
    fn arrows_move_between_rows_and_enter_activates_a_session() {
        let mut overlay = SessionsOverlay::new(sessions(), Ok(()));

        overlay.update(key(KeyCode::Down));
        overlay.update(key(KeyCode::Down));
        assert_eq!(
            overlay.update(key(KeyCode::Enter)).effects,
            [SessionsEffect::Activate(PaneId::Fork(1))]
        );

        for _ in 0..5 {
            overlay.update(key(KeyCode::Down));
        }
        assert_eq!(
            overlay.update(key(KeyCode::Enter)).effects,
            [SessionsEffect::Activate(PaneId::Fork(2))]
        );

        for _ in 0..5 {
            overlay.update(key(KeyCode::Up));
        }
        assert_eq!(
            overlay.update(key(KeyCode::Enter)).effects,
            [SessionsEffect::New]
        );
    }

    #[test]
    fn search_filters_by_title_and_model_and_keeps_the_new_row() {
        let mut overlay = SessionsOverlay::new(sessions(), Ok(()));

        type_text(&mut overlay, "DOCS");
        let rendered = render(&mut overlay, 90);
        assert!(rendered.contains("write docs"));
        assert!(!rendered.contains("fix parser"));
        assert!(rendered.contains("New session"));
        assert_eq!(
            overlay.update(key(KeyCode::Enter)).effects,
            [SessionsEffect::Activate(PaneId::Fork(1))]
        );

        overlay.update(key(KeyCode::Backspace));
        overlay.update(key(KeyCode::Backspace));
        overlay.update(key(KeyCode::Backspace));
        overlay.update(key(KeyCode::Backspace));
        type_text(&mut overlay, "claude");
        assert_eq!(
            overlay.update(key(KeyCode::Enter)).effects,
            [SessionsEffect::Activate(PaneId::Fork(1))]
        );

        type_text(&mut overlay, "zzz");
        assert_eq!(
            overlay.update(key(KeyCode::Enter)).effects,
            [SessionsEffect::New]
        );
    }

    #[test]
    fn ctrl_d_closes_the_highlighted_session_but_not_the_new_row() {
        let mut overlay = SessionsOverlay::new(sessions(), Ok(()));
        let ctrl_d = || press(KeyCode::Char('d'), KeyModifiers::CONTROL);

        assert!(overlay.update(ctrl_d()).effects.is_empty());
        overlay.update(key(KeyCode::Down));
        overlay.update(key(KeyCode::Down));
        assert_eq!(
            overlay.update(ctrl_d()).effects,
            [SessionsEffect::Close(PaneId::Fork(1))]
        );

        type_text(&mut overlay, "d");
        assert!(overlay.query.ends_with('d'));
    }

    #[test]
    fn escape_cancels() {
        let mut overlay = SessionsOverlay::new(sessions(), Ok(()));

        assert_eq!(
            overlay.update(key(KeyCode::Esc)).effects,
            [SessionsEffect::Cancel]
        );
    }

    #[test]
    fn refresh_keeps_the_highlighted_session_or_clamps() {
        let mut overlay = SessionsOverlay::new(sessions(), Ok(()));
        overlay.update(key(KeyCode::Down));
        overlay.update(key(KeyCode::Down));

        let mut reordered = sessions();
        reordered.remove(0);
        overlay.set_sessions(reordered, Ok(()));
        assert_eq!(
            overlay.update(key(KeyCode::Enter)).effects,
            [SessionsEffect::Activate(PaneId::Fork(1))]
        );

        overlay.set_sessions(vec![session(PaneId::Main, "only", "gpt")], Ok(()));
        assert_eq!(
            overlay.update(key(KeyCode::Enter)).effects,
            [SessionsEffect::Activate(PaneId::Main)]
        );

        overlay.set_sessions(Vec::new(), Err("full".to_owned()));
        assert!(overlay.update(key(KeyCode::Enter)).effects.is_empty());
    }
}
