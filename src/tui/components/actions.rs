//! Searchable modal menu for actions exposed by the TUI.

use super::{
    floating::Floating,
    node::{Component, ComponentUpdate, RenderRequest},
};
use crate::tui::theme::Theme;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const ACTIONS: [Action; 9] = [
    Action::Effort,
    Action::Theme,
    Action::NewSession,
    Action::ResumeSession,
    Action::Fork,
    Action::Keybindings,
    Action::ReloadConfig,
    Action::EditConfig,
    Action::Subagents,
];
const KEY_BINDINGS: [&str; 4] = ["tab focus", "↑↓ move", "enter focus/open", "esc close"];
const SEARCH_LABEL: &str = "Search: ";
const FOCUS_MARKER: &str = "› ";

pub(super) enum ActionsEvent {
    Terminal(Event),
}

pub(super) struct ActionAvailability {
    pub(super) new_session: bool,
    pub(super) fork: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Action {
    Subagents,
    Effort,
    Theme,
    NewSession,
    ResumeSession,
    Fork,
    Keybindings,
    ReloadConfig,
    EditConfig,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum ActionsEffect {
    Dismiss,
    Trigger(Action),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Focus {
    Search,
    List,
}

pub(super) struct ActionsMenu {
    query: String,
    focus: Focus,
    selected: usize,
    matches: Vec<usize>,
    availability: ActionAvailability,
}

impl ActionsMenu {
    pub(super) fn new(availability: ActionAvailability) -> Self {
        Self {
            query: String::new(),
            focus: Focus::Search,
            selected: 0,
            matches: (0..ACTIONS.len()).collect(),
            availability,
        }
    }

    fn update_key(&mut self, key: KeyEvent) -> ComponentUpdate<ActionsEffect> {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return ComponentUpdate::none();
        }

        match key.code {
            KeyCode::Esc => Self::dismiss(),
            KeyCode::Backspace if self.focus == Focus::Search && !self.query.is_empty() => {
                self.remove_last_grapheme();
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            KeyCode::Backspace => Self::dismiss(),
            KeyCode::Enter => self.handle_enter(),
            KeyCode::Tab | KeyCode::BackTab => {
                self.toggle_focus();
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            KeyCode::Up if self.focus == Focus::List => {
                self.select_previous();
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            KeyCode::Down if self.focus == Focus::List => {
                self.select_next();
                ComponentUpdate::render(RenderRequest::Immediate)
            }
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

    fn insert_paste(&mut self, text: &str) -> ComponentUpdate<ActionsEffect> {
        if self.focus != Focus::Search {
            return ComponentUpdate::none();
        }

        self.query
            .extend(text.chars().filter(|character| !character.is_control()));
        self.refresh_matches();
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn dismiss() -> ComponentUpdate<ActionsEffect> {
        ComponentUpdate {
            effects: vec![ActionsEffect::Dismiss],
            render: RenderRequest::Immediate,
        }
    }

    fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Search => Focus::List,
            Focus::List => Focus::Search,
        };
    }

    fn remove_last_grapheme(&mut self) {
        let Some((index, _)) = self.query.grapheme_indices(true).next_back() else {
            return;
        };
        self.query.truncate(index);
        self.refresh_matches();
    }

    fn refresh_matches(&mut self) {
        self.matches.clear();
        self.matches.extend(
            ACTIONS
                .iter()
                .enumerate()
                .filter(|(_, action)| action.matches(&self.query))
                .map(|(index, _)| index),
        );
        self.selected = 0;
    }

    fn select_previous(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        self.selected = self.selected.saturating_sub(1);
    }

    fn select_next(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        self.selected = (self.selected + 1).min(self.matches.len() - 1);
    }

    fn trigger_selected(&self) -> ComponentUpdate<ActionsEffect> {
        let Some(action) = self.matches.get(self.selected) else {
            return ComponentUpdate::none();
        };
        self.trigger(ACTIONS[*action])
    }

    fn trigger(&self, action: Action) -> ComponentUpdate<ActionsEffect> {
        if !self.is_enabled(action) {
            return ComponentUpdate::none();
        }
        ComponentUpdate {
            effects: vec![ActionsEffect::Trigger(action)],
            render: RenderRequest::Immediate,
        }
    }

    fn handle_enter(&mut self) -> ComponentUpdate<ActionsEffect> {
        if self.focus == Focus::List {
            return self.trigger_selected();
        }

        let mut enabled_matches = self
            .matches
            .iter()
            .map(|index| ACTIONS[*index])
            .filter(|action| self.is_enabled(*action));
        if let Some(action) = enabled_matches.next()
            && enabled_matches.next().is_none()
        {
            return self.trigger(action);
        }

        self.toggle_focus();
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn render_search(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        if area.is_empty() {
            return;
        }

        let focused = self.focus == Focus::Search;
        let marker = if focused { FOCUS_MARKER } else { "  " };
        let prefix_width = marker.width() + SEARCH_LABEL.width();
        let query_width = usize::from(area.width).saturating_sub(prefix_width);
        let visible_query = visible_query_tail(&self.query, query_width);
        let focus_style = if focused {
            Style::default()
                .fg(theme.accent())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.muted())
        };
        let line = Line::from(vec![
            Span::styled(marker, focus_style),
            Span::styled(SEARCH_LABEL, focus_style),
            Span::styled(visible_query, Style::default().fg(theme.text())),
        ]);
        frame.render_widget(Paragraph::new(line), area);

        if !focused {
            return;
        }
        let cursor_offset = prefix_width + visible_query.width();
        let cursor_x = area.x
            + u16::try_from(cursor_offset)
                .unwrap_or(u16::MAX)
                .min(area.width.saturating_sub(1));
        frame.set_cursor_position(Position::new(cursor_x, area.y));
    }

    fn render_actions(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        if area.is_empty() {
            return;
        }

        let items = self.matches.iter().enumerate().map(|(row, index)| {
            let action = ACTIONS[*index];
            let enabled = self.is_enabled(action);
            let selected = row == self.selected;
            let label_color = if !enabled {
                theme.muted()
            } else if self.focus == Focus::List && selected {
                theme.accent()
            } else {
                theme.text()
            };
            let mut spans = vec![Span::styled(
                self.display_label(action),
                Style::default().fg(label_color),
            )];
            if let Some(alias) = action.alias() {
                spans.push(Span::styled(
                    format!(" (alias: {alias})"),
                    Style::default().fg(theme.muted()),
                ));
            }
            ListItem::new(Line::from(spans))
        });
        let focused = self.focus == Focus::List;
        let selected_enabled = self
            .matches
            .get(self.selected)
            .is_some_and(|index| self.is_enabled(ACTIONS[*index]));
        let highlight = if selected_enabled && focused {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let list = List::new(items)
            .style(Style::default().fg(theme.text()))
            .highlight_style(highlight)
            .highlight_symbol(if focused { FOCUS_MARKER } else { "  " });
        let selected = (!self.matches.is_empty()).then_some(self.selected);
        let mut state = ListState::default().with_selected(selected);
        frame.render_stateful_widget(list, area, &mut state);

        if focused && selected.is_some() {
            let cursor_y = area.y + u16::try_from(self.selected).unwrap_or(u16::MAX);
            frame.set_cursor_position(Position::new(area.x, cursor_y.min(area.bottom() - 1)));
        }
    }

    const fn is_enabled(&self, action: Action) -> bool {
        match action {
            Action::Subagents => true,
            Action::Effort => true,
            Action::Theme => true,
            Action::NewSession => self.availability.new_session,
            Action::ResumeSession => self.availability.new_session,
            Action::Fork => self.availability.fork,
            Action::Keybindings => true,
            Action::ReloadConfig => true,
            Action::EditConfig => true,
        }
    }

    const fn display_label(&self, action: Action) -> &'static str {
        match action {
            Action::NewSession if !self.availability.new_session => {
                "New session · finish active work first"
            }
            Action::ResumeSession if !self.availability.new_session => {
                "Resume session · finish active work first"
            }
            Action::Fork if !self.availability.fork => "Fork session · one fork at a time",
            _ => action.label(),
        }
    }
}

impl Action {
    const fn label(self) -> &'static str {
        match self {
            Self::Subagents => "Subagents",
            Self::Effort => "Change effort",
            Self::Theme => "Select theme",
            Self::NewSession => "New session",
            Self::ResumeSession => "Resume session",
            Self::Fork => "Fork session",
            Self::Keybindings => "Keyboard shortcuts",
            Self::ReloadConfig => "Reload config",
            Self::EditConfig => "Edit config",
        }
    }

    const fn alias(self) -> Option<&'static str> {
        match self {
            Self::Subagents => Some("agents"),
            Self::Effort => Some("thinking"),
            Self::Theme => Some("appearance"),
            Self::NewSession => Some("clear"),
            Self::ResumeSession => Some("restore"),
            Self::Fork => Some("btw"),
            Self::ReloadConfig => Some("refresh"),
            Self::Keybindings | Self::EditConfig => None,
        }
    }

    fn matches(self, query: &str) -> bool {
        contains_ignore_ascii_case(self.label(), query)
            || self
                .alias()
                .is_some_and(|alias| contains_ignore_ascii_case(alias, query))
    }
}

impl Component for ActionsMenu {
    type Event = ActionsEvent;
    type Effect = ActionsEffect;

    fn update(&mut self, event: Self::Event) -> ComponentUpdate<Self::Effect> {
        match event {
            ActionsEvent::Terminal(Event::Key(key)) => self.update_key(key),
            ActionsEvent::Terminal(Event::Paste(text)) => self.insert_paste(&text),
            ActionsEvent::Terminal(_) => ComponentUpdate::none(),
        }
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        if area.is_empty() {
            return;
        }

        let layout = Floating::new("Actions", 58, 13, &KEY_BINDINGS).render(frame, area, theme);
        if layout.body.is_empty() {
            return;
        }
        let search_area = Rect {
            height: 1,
            ..layout.body
        };
        let actions_area = Rect {
            y: layout.body.y + 1,
            height: layout.body.height.saturating_sub(1),
            ..layout.body
        };
        self.render_search(frame, search_area, theme);
        self.render_actions(frame, actions_area, theme);
    }
}

fn contains_ignore_ascii_case(value: &str, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    if query.len() > value.len() {
        return false;
    }
    value
        .as_bytes()
        .windows(query.len())
        .any(|window| window.eq_ignore_ascii_case(query.as_bytes()))
}

fn visible_query_tail(query: &str, width: usize) -> &str {
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
        Action, ActionAvailability, ActionsEffect, ActionsEvent, ActionsMenu, Component, Focus,
    };
    use crate::tui::theme::Theme;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend, layout::Position};

    fn key(code: KeyCode) -> ActionsEvent {
        ActionsEvent::Terminal(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn available() -> ActionAvailability {
        ActionAvailability {
            new_session: true,
            fork: true,
        }
    }

    fn render(menu: &mut ActionsMenu) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(60, 15)).unwrap();
        terminal
            .draw(|frame| menu.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        terminal
    }

    fn row_segment(terminal: &Terminal<TestBackend>, y: u16, x: u16, width: u16) -> String {
        let buffer = terminal.backend().buffer();
        (x..x + width)
            .map(|column| buffer[(column, y)].symbol())
            .collect()
    }

    #[test]
    fn popup_is_centered_with_search_focused_and_available_actions() {
        let mut menu = ActionsMenu::new(available());
        let terminal = render(&mut menu);

        assert_eq!(
            row_segment(&terminal, 1, 1, 58),
            "╭─────────────────────── Actions ────────────────────────╮"
        );
        assert_eq!(
            row_segment(&terminal, 2, 1, 58),
            "│› Search:                                               │"
        );
        assert_eq!(
            row_segment(&terminal, 3, 1, 58),
            "│  Change effort (alias: thinking)                       │"
        );
        assert_eq!(
            row_segment(&terminal, 4, 1, 58),
            "│  Select theme (alias: appearance)                      │"
        );
        assert_eq!(
            row_segment(&terminal, 5, 1, 58),
            "│  New session (alias: clear)                            │"
        );
        assert_eq!(
            row_segment(&terminal, 6, 1, 58),
            "│  Resume session (alias: restore)                       │"
        );
        assert_eq!(
            row_segment(&terminal, 7, 1, 58),
            "│  Fork session (alias: btw)                             │"
        );
        assert_eq!(
            row_segment(&terminal, 8, 1, 58),
            "│  Keyboard shortcuts                                    │"
        );
        assert_eq!(
            row_segment(&terminal, 9, 1, 58),
            "│  Reload config (alias: refresh)                        │"
        );
        assert_eq!(
            row_segment(&terminal, 10, 1, 58),
            "│  Edit config                                           │"
        );
        assert_eq!(
            row_segment(&terminal, 11, 1, 58),
            "│  Subagents (alias: agents)                             │"
        );
        assert_eq!(
            row_segment(&terminal, 12, 1, 58),
            "│   tab focus · ↑↓ move · enter focus/open · esc close   │"
        );
        assert_eq!(
            row_segment(&terminal, 13, 1, 58),
            "╰────────────────────────────────────────────────────────╯"
        );
        assert_eq!(terminal.backend().cursor_position(), Position::new(12, 2));
        assert_eq!(
            terminal.backend().buffer()[(18, 3)].fg,
            Theme::default().muted()
        );
    }

    #[test]
    fn search_filters_actions_and_backspace_edits_before_dismissing() {
        let mut menu = ActionsMenu::new(available());
        menu.update(key(KeyCode::Char('E')));
        menu.update(key(KeyCode::Char('F')));
        menu.update(key(KeyCode::Char('F')));

        assert_eq!(menu.matches, [0]);
        assert!(menu.update(key(KeyCode::Backspace)).effects.is_empty());
        assert_eq!(menu.query, "EF");

        menu.update(key(KeyCode::Backspace));
        menu.update(key(KeyCode::Backspace));
        assert!(matches!(
            menu.update(key(KeyCode::Backspace)).effects.as_slice(),
            [ActionsEffect::Dismiss]
        ));
    }

    #[test]
    fn tab_swaps_focus_to_the_action_list() {
        let mut menu = ActionsMenu::new(available());
        menu.update(key(KeyCode::Tab));
        assert_eq!(menu.focus, Focus::List);

        menu.update(key(KeyCode::Down));
        menu.update(key(KeyCode::Down));
        menu.update(key(KeyCode::Down));
        menu.update(key(KeyCode::Down));
        menu.update(key(KeyCode::Down));
        assert_eq!(menu.selected, 5);

        let terminal = render(&mut menu);
        assert_eq!(
            row_segment(&terminal, 2, 1, 58),
            "│  Search:                                               │"
        );
        assert_eq!(
            row_segment(&terminal, 8, 1, 58),
            "│› Keyboard shortcuts                                    │"
        );
        assert_eq!(terminal.backend().cursor_position(), Position::new(2, 8));

        menu.update(key(KeyCode::BackTab));
        assert_eq!(menu.focus, Focus::Search);
    }

    #[test]
    fn effort_action_triggers_when_available() {
        let mut enabled = ActionsMenu::new(available());
        for character in "thinking".chars() {
            enabled.update(key(KeyCode::Char(character)));
        }
        assert_eq!(
            enabled.update(key(KeyCode::Enter)).effects,
            [ActionsEffect::Trigger(Action::Effort)]
        );
    }

    #[test]
    fn config_actions_are_individually_searchable() {
        let mut menu = ActionsMenu::new(available());
        for character in "edit config".chars() {
            menu.update(key(KeyCode::Char(character)));
        }

        assert_eq!(
            menu.update(key(KeyCode::Enter)).effects,
            [ActionsEffect::Trigger(Action::EditConfig)]
        );

        let mut reload = ActionsMenu::new(available());
        for character in "refresh".chars() {
            reload.update(key(KeyCode::Char(character)));
        }
        assert_eq!(
            reload.update(key(KeyCode::Enter)).effects,
            [ActionsEffect::Trigger(Action::ReloadConfig)]
        );
    }

    #[test]
    fn new_session_action_supports_clear_alias_and_busy_explanation() {
        let mut enabled = ActionsMenu::new(available());
        for character in "clear".chars() {
            enabled.update(key(KeyCode::Char(character)));
        }
        assert_eq!(
            enabled.update(key(KeyCode::Enter)).effects,
            [ActionsEffect::Trigger(Action::NewSession)]
        );

        let mut availability = available();
        availability.new_session = false;
        let mut disabled = ActionsMenu::new(availability);
        disabled.update(key(KeyCode::Tab));
        disabled.update(key(KeyCode::Down));
        disabled.update(key(KeyCode::Down));
        let terminal = render(&mut disabled);
        assert_eq!(
            row_segment(&terminal, 5, 1, 58),
            "│› New session · finish active work first (alias: clear) │"
        );
        assert_eq!(
            terminal.backend().buffer()[(4, 5)].fg,
            Theme::default().muted()
        );
    }

    #[test]
    fn resume_session_action_supports_restore_alias() {
        let mut menu = ActionsMenu::new(available());
        for character in "restore".chars() {
            menu.update(key(KeyCode::Char(character)));
        }
        assert_eq!(
            menu.update(key(KeyCode::Enter)).effects,
            [ActionsEffect::Trigger(Action::ResumeSession)]
        );
    }

    #[test]
    fn theme_action_is_searchable_by_appearance() {
        let mut menu = ActionsMenu::new(available());
        for character in "appearance".chars() {
            menu.update(key(KeyCode::Char(character)));
        }

        assert_eq!(
            menu.update(key(KeyCode::Enter)).effects,
            [ActionsEffect::Trigger(Action::Theme)]
        );
    }

    #[test]
    fn keybindings_action_is_searchable_and_triggers_immediately() {
        let mut menu = ActionsMenu::new(available());
        for character in "keyboard".chars() {
            menu.update(key(KeyCode::Char(character)));
        }

        assert_eq!(
            menu.update(key(KeyCode::Enter)).effects,
            [ActionsEffect::Trigger(Action::Keybindings)]
        );
    }

    #[test]
    fn fork_alias_is_searchable_and_disabled_while_a_fork_is_open() {
        let mut enabled = ActionsMenu::new(available());
        for character in "btw".chars() {
            enabled.update(key(KeyCode::Char(character)));
        }
        assert_eq!(
            enabled.update(key(KeyCode::Enter)).effects,
            [ActionsEffect::Trigger(Action::Fork)]
        );

        let mut availability = available();
        availability.fork = false;
        let mut disabled = ActionsMenu::new(availability);
        for character in "btw".chars() {
            disabled.update(key(KeyCode::Char(character)));
        }
        assert!(disabled.update(key(KeyCode::Enter)).effects.is_empty());
    }

    #[test]
    fn enter_toggles_focus_when_search_has_no_available_action() {
        let mut menu = ActionsMenu::new(available());
        menu.update(key(KeyCode::Char('z')));

        assert!(menu.update(key(KeyCode::Enter)).effects.is_empty());
        assert_eq!(menu.focus, Focus::List);
    }

    #[test]
    fn escape_dismisses_from_either_focus() {
        let mut menu = ActionsMenu::new(available());
        menu.update(key(KeyCode::Tab));

        assert!(matches!(
            menu.update(key(KeyCode::Esc)).effects.as_slice(),
            [ActionsEffect::Dismiss]
        ));
    }

    #[test]
    fn narrow_terminals_do_not_overflow_the_popup() {
        let mut menu = ActionsMenu::new(available());
        let mut terminal = Terminal::new(TestBackend::new(3, 2)).unwrap();

        terminal
            .draw(|frame| menu.render(frame, frame.area(), &Theme::default()))
            .unwrap();

        assert_eq!(terminal.backend().buffer().area.width, 3);
    }
}
