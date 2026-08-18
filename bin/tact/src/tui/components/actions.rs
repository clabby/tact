//! Searchable modal menu for actions exposed by the TUI.

use super::{
    floating::Floating,
    node::{Component, ComponentUpdate, RenderRequest},
};
use crate::tui::theme::Theme;
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

const ACTIONS: [Action; 17] = [
    Action::Effort,
    Action::FastMode,
    Action::Theme,
    Action::NewSession,
    Action::NewSessionAlternate,
    Action::ResumeSession,
    Action::Fork,
    Action::Keybindings,
    Action::ReloadConfig,
    Action::EditConfig,
    Action::Memory,
    Action::Subagents,
    Action::DebugContext,
    Action::Reflection,
    Action::Handoff,
    Action::Review,
    Action::Model,
];
const KEY_BINDINGS: [&str; 3] = ["↑↓ move", "enter/tab open", "esc close"];
const SEARCH_LABEL: &str = "Search: ";
const SELECTION_MARKER: &str = "› ";

pub(super) enum ActionsEvent {
    Terminal(Event),
}

pub(super) struct ActionAvailability {
    pub(super) new_session: bool,
    pub(super) fork: bool,
    pub(super) fast_mode: bool,
    pub(super) memory: bool,
    pub(super) model: bool,
    pub(super) default_memory_disabled: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Action {
    Handoff,
    Review,
    Subagents,
    Effort,
    Model,
    FastMode,
    Theme,
    NewSession,
    NewSessionAlternate,
    ResumeSession,
    Fork,
    Keybindings,
    ReloadConfig,
    EditConfig,
    Memory,
    DebugContext,
    Reflection,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum ActionsEffect {
    Dismiss,
    Trigger(Action),
}

pub(super) struct ActionsMenu {
    query: String,
    selected: usize,
    matches: Vec<usize>,
    availability: ActionAvailability,
}

impl ActionsMenu {
    pub(super) fn new(availability: ActionAvailability) -> Self {
        Self {
            query: String::new(),
            selected: 0,
            matches: (0..ACTIONS.len()).collect(),
            availability,
        }
    }

    pub(super) fn set_fork_available(&mut self, available: bool) {
        self.availability.fork = available;
    }

    fn update_key(&mut self, key: KeyEvent) -> ComponentUpdate<ActionsEffect> {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return ComponentUpdate::none();
        }

        match key.code {
            KeyCode::Esc => Self::dismiss(),
            KeyCode::Backspace if !self.query.is_empty() => {
                self.remove_last_grapheme();
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            KeyCode::Backspace => Self::dismiss(),
            KeyCode::Enter | KeyCode::Tab => self.trigger_selected(),
            KeyCode::Up => {
                self.select_previous();
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            KeyCode::Down => {
                self.select_next();
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            KeyCode::Char(character)
                if !key
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

    fn render_search(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        if area.is_empty() {
            return;
        }

        let marker = "  ";
        let prefix_width = marker.width() + SEARCH_LABEL.width();
        let query_width = usize::from(area.width).saturating_sub(prefix_width);
        let visible_query = visible_query_tail(&self.query, query_width);
        let label_style = Style::default().fg(theme.muted());
        let line = Line::from(vec![
            Span::styled(marker, label_style),
            Span::styled(SEARCH_LABEL, label_style),
            Span::styled(visible_query, Style::default().fg(theme.text())),
        ]);
        frame.render_widget(Paragraph::new(line), area);
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
            } else if selected {
                theme.accent()
            } else {
                theme.text()
            };
            let mut spans = vec![Span::styled(
                self.display_label(action),
                Style::default().fg(label_color),
            )];
            if let Some(alias) = action
                .alias()
                .filter(|_| enabled || action != Action::Memory)
            {
                spans.push(Span::styled(
                    format!(" (alias: {alias})"),
                    Style::default().fg(theme.muted()),
                ));
            }
            ListItem::new(Line::from(spans))
        });
        let selected_enabled = self
            .matches
            .get(self.selected)
            .is_some_and(|index| self.is_enabled(ACTIONS[*index]));
        let highlight = if selected_enabled {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let list = List::new(items)
            .style(Style::default().fg(theme.text()))
            .highlight_style(highlight)
            .highlight_symbol(SELECTION_MARKER);
        let selected = (!self.matches.is_empty()).then_some(self.selected);
        let mut state = ListState::default().with_selected(selected);
        frame.render_stateful_widget(list, area, &mut state);
    }

    const fn is_enabled(&self, action: Action) -> bool {
        match action {
            Action::Handoff | Action::Review | Action::Reflection => self.availability.new_session,
            Action::Subagents => true,
            Action::Effort => true,
            Action::Model => self.availability.model,
            Action::FastMode => true,
            Action::Theme => true,
            Action::NewSession | Action::NewSessionAlternate => self.availability.new_session,
            Action::ResumeSession => self.availability.new_session,
            Action::Fork => self.availability.fork,
            Action::Keybindings => true,
            Action::ReloadConfig => true,
            Action::EditConfig => true,
            Action::Memory => self.availability.memory,
            Action::DebugContext => true,
        }
    }

    const fn display_label(&self, action: Action) -> &'static str {
        match action {
            Action::NewSession if !self.availability.new_session => {
                "New session · finish active work first"
            }
            Action::NewSessionAlternate if !self.availability.new_session => {
                "New session with alternate memory policy · finish active work first"
            }
            Action::NewSessionAlternate if self.availability.default_memory_disabled => {
                "New session with configured memory"
            }
            Action::NewSessionAlternate => "New session without memory",
            Action::ResumeSession if !self.availability.new_session => {
                "Resume session · finish active work first"
            }
            Action::Fork if !self.availability.fork => "Fork session · one fork at a time",
            Action::Review if !self.availability.new_session => {
                "Review changes · finish active work first"
            }
            Action::Handoff if !self.availability.new_session => {
                "Prepare handoff · finish active work first"
            }
            Action::Reflection if !self.availability.new_session => {
                "Reflect on session · finish active work first"
            }
            Action::FastMode if self.availability.fast_mode => "Disable fast mode",
            Action::Model if !self.availability.model => "Select model · start a new session first",
            Action::Memory if !self.availability.memory => {
                "Memory · enable in config: memory.enabled = true"
            }
            _ => action.label(),
        }
    }
}

impl Action {
    const fn label(self) -> &'static str {
        match self {
            Self::Handoff => "Prepare handoff",
            Self::Review => "Review changes",
            Self::Subagents => "Subagents",
            Self::Effort => "Change effort",
            Self::Model => "Select model",
            Self::FastMode => "Enable fast mode",
            Self::Theme => "Select theme",
            Self::NewSession => "New session",
            Self::NewSessionAlternate => "New session without memory",
            Self::ResumeSession => "Resume session",
            Self::Fork => "Fork session",
            Self::Keybindings => "Keyboard shortcuts",
            Self::ReloadConfig => "Reload config",
            Self::EditConfig => "Edit config",
            Self::Memory => "Memory",
            Self::DebugContext => "Debug context",
            Self::Reflection => "Reflect on session",
        }
    }

    const fn alias(self) -> Option<&'static str> {
        match self {
            Self::Handoff => Some("handoff"),
            Self::Review => Some("review"),
            Self::Subagents => Some("agents"),
            Self::Effort => Some("thinking"),
            Self::Model => Some("intelligence"),
            Self::FastMode => Some("priority"),
            Self::Theme => Some("appearance"),
            Self::NewSession => Some("clear"),
            Self::NewSessionAlternate => None,
            Self::ResumeSession => Some("restore"),
            Self::Fork => Some("btw"),
            Self::ReloadConfig => Some("refresh"),
            Self::Memory => Some("remember/forget"),
            Self::Reflection => Some("reflection"),
            Self::Keybindings | Self::EditConfig | Self::DebugContext => None,
        }
    }

    fn matches(self, query: &str) -> bool {
        if self == Self::NewSessionAlternate {
            if query.is_empty() {
                return true;
            }
            if query.eq_ignore_ascii_case("memory")
                || query.eq_ignore_ascii_case("remember")
                || query.eq_ignore_ascii_case("forget")
            {
                return false;
            }
            return contains_ignore_ascii_case("New session without memory", query)
                || contains_ignore_ascii_case("New session with configured memory", query)
                || contains_ignore_ascii_case("private", query)
                || contains_ignore_ascii_case("no memory", query);
        }
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

        let layout = Floating::new("Actions", 58, 19, &KEY_BINDINGS).render(frame, area, theme);
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
    use super::{Action, ActionAvailability, ActionsEffect, ActionsEvent, ActionsMenu, Component};
    use crate::tui::theme::Theme;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend};

    fn key(code: KeyCode) -> ActionsEvent {
        ActionsEvent::Terminal(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn available() -> ActionAvailability {
        ActionAvailability {
            new_session: true,
            fork: true,
            fast_mode: false,
            memory: true,
            model: true,
            default_memory_disabled: false,
        }
    }

    fn render(menu: &mut ActionsMenu) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
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
    fn popup_is_centered_with_the_first_action_selected() {
        let mut menu = ActionsMenu::new(available());
        let terminal = render(&mut menu);

        assert_eq!(
            row_segment(&terminal, 0, 1, 58),
            "╭─────────────────────── Actions ────────────────────────╮"
        );
        assert_eq!(
            row_segment(&terminal, 1, 1, 58),
            "│  Search:                                               │"
        );
        assert_eq!(
            row_segment(&terminal, 2, 1, 58),
            "│› Change effort (alias: thinking)                       │"
        );
        assert_eq!(
            row_segment(&terminal, 3, 1, 58),
            "│  Enable fast mode (alias: priority)                    │"
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
            "│  New session without memory                            │"
        );
        assert_eq!(
            row_segment(&terminal, 7, 1, 58),
            "│  Resume session (alias: restore)                       │"
        );
        assert_eq!(
            row_segment(&terminal, 8, 1, 58),
            "│  Fork session (alias: btw)                             │"
        );
        assert_eq!(
            row_segment(&terminal, 9, 1, 58),
            "│  Keyboard shortcuts                                    │"
        );
        assert_eq!(
            row_segment(&terminal, 10, 1, 58),
            "│  Reload config (alias: refresh)                        │"
        );
        assert_eq!(
            row_segment(&terminal, 11, 1, 58),
            "│  Edit config                                           │"
        );
        assert_eq!(
            row_segment(&terminal, 12, 1, 58),
            "│  Memory (alias: remember/forget)                       │"
        );
        assert_eq!(
            row_segment(&terminal, 13, 1, 58),
            "│  Subagents (alias: agents)                             │"
        );
        assert_eq!(
            row_segment(&terminal, 14, 1, 58),
            "│  Debug context                                         │"
        );
        assert_eq!(
            row_segment(&terminal, 15, 1, 58),
            "│  Reflect on session (alias: reflection)                │"
        );
        assert_eq!(
            row_segment(&terminal, 16, 1, 58),
            "│  Prepare handoff (alias: handoff)                      │"
        );
        assert_eq!(
            row_segment(&terminal, 17, 1, 58),
            "│          ↑↓ move · enter/tab open · esc close          │"
        );
        assert_eq!(
            row_segment(&terminal, 18, 1, 58),
            "╰────────────────────────────────────────────────────────╯"
        );
        assert_eq!(
            terminal.backend().buffer()[(18, 2)].fg,
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
    fn arrows_navigate_while_typing_continues_to_search() {
        let mut menu = ActionsMenu::new(available());
        menu.update(key(KeyCode::Down));
        assert_eq!(menu.selected, 1);
        assert_eq!(
            menu.update(key(KeyCode::Enter)).effects,
            [ActionsEffect::Trigger(Action::FastMode)]
        );

        menu.update(key(KeyCode::Char('t')));
        menu.update(key(KeyCode::Down));
        menu.update(key(KeyCode::Char('h')));
        assert_eq!(menu.query, "th");
        assert_eq!(menu.selected, 0);
    }

    #[test]
    fn tab_triggers_the_selected_action() {
        let mut menu = ActionsMenu::new(available());
        menu.update(key(KeyCode::Down));

        assert_eq!(
            menu.update(key(KeyCode::Tab)).effects,
            [ActionsEffect::Trigger(Action::FastMode)]
        );
    }

    #[test]
    fn handoff_alias_triggers_the_handoff_action() {
        let mut menu = ActionsMenu::new(available());
        for character in "handoff".chars() {
            menu.update(key(KeyCode::Char(character)));
        }

        assert_eq!(
            menu.update(key(KeyCode::Enter)).effects,
            [ActionsEffect::Trigger(Action::Handoff)]
        );
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
    fn model_action_is_available_only_before_the_first_prompt() {
        let mut enabled = ActionsMenu::new(available());
        for character in "intelligence".chars() {
            enabled.update(key(KeyCode::Char(character)));
        }
        assert_eq!(
            enabled.update(key(KeyCode::Enter)).effects,
            [ActionsEffect::Trigger(Action::Model)]
        );

        let mut availability = available();
        availability.model = false;
        let mut disabled = ActionsMenu::new(availability);
        for character in "intelligence".chars() {
            disabled.update(key(KeyCode::Char(character)));
        }
        let terminal = render(&mut disabled);
        assert!((0..20).any(|row| {
            row_segment(&terminal, row, 0, 60).contains("Select model · start a new session first")
        }));
        assert!(disabled.update(key(KeyCode::Enter)).effects.is_empty());
    }

    #[test]
    fn review_action_is_searchable() {
        let mut menu = ActionsMenu::new(available());
        for character in "review".chars() {
            menu.update(key(KeyCode::Char(character)));
        }

        assert_eq!(
            menu.update(key(KeyCode::Enter)).effects,
            [ActionsEffect::Trigger(Action::Review)]
        );
    }

    #[test]
    fn reflection_action_is_searchable_and_waits_for_idle_work() {
        let mut enabled = ActionsMenu::new(available());
        for character in "reflection".chars() {
            enabled.update(key(KeyCode::Char(character)));
        }
        assert_eq!(
            enabled.update(key(KeyCode::Enter)).effects,
            [ActionsEffect::Trigger(Action::Reflection)]
        );

        let mut availability = available();
        availability.new_session = false;
        let mut disabled = ActionsMenu::new(availability);
        for character in "reflection".chars() {
            disabled.update(key(KeyCode::Char(character)));
        }
        assert!(disabled.update(key(KeyCode::Enter)).effects.is_empty());
    }

    #[test]
    fn fast_mode_action_reflects_the_current_setting() {
        let mut enabled = ActionsMenu::new(available());
        for character in "priority".chars() {
            enabled.update(key(KeyCode::Char(character)));
        }
        assert_eq!(
            enabled.update(key(KeyCode::Enter)).effects,
            [ActionsEffect::Trigger(Action::FastMode)]
        );

        let mut availability = available();
        availability.fast_mode = true;
        let mut disabled = ActionsMenu::new(availability);
        let terminal = render(&mut disabled);
        assert_eq!(
            row_segment(&terminal, 3, 1, 58),
            "│  Disable fast mode (alias: priority)                   │"
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
        disabled.update(key(KeyCode::Down));
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
    fn alternate_new_session_action_is_searchable_for_both_window_defaults() {
        let mut without_memory = ActionsMenu::new(available());
        for character in "without memory".chars() {
            without_memory.update(key(KeyCode::Char(character)));
        }
        assert_eq!(
            without_memory.update(key(KeyCode::Enter)).effects,
            [ActionsEffect::Trigger(Action::NewSessionAlternate)]
        );

        let mut availability = available();
        availability.default_memory_disabled = true;
        let mut configured = ActionsMenu::new(availability);
        for character in "configured memory".chars() {
            configured.update(key(KeyCode::Char(character)));
        }
        assert_eq!(
            configured.update(key(KeyCode::Enter)).effects,
            [ActionsEffect::Trigger(Action::NewSessionAlternate)]
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
    fn debug_context_action_is_searchable_and_triggers() {
        let mut menu = ActionsMenu::new(available());
        for character in "debug context".chars() {
            menu.update(key(KeyCode::Char(character)));
        }

        assert_eq!(
            menu.update(key(KeyCode::Enter)).effects,
            [ActionsEffect::Trigger(Action::DebugContext)]
        );
    }

    #[test]
    fn memory_aliases_are_searchable_and_disabled_state_explains_configuration() {
        for alias in ["memory", "remember", "forget"] {
            let mut enabled = ActionsMenu::new(available());
            for character in alias.chars() {
                enabled.update(key(KeyCode::Char(character)));
            }
            assert_eq!(
                enabled.update(key(KeyCode::Enter)).effects,
                [ActionsEffect::Trigger(Action::Memory)]
            );
        }

        let mut availability = available();
        availability.memory = false;
        let mut disabled = ActionsMenu::new(availability);
        for character in "memory".chars() {
            disabled.update(key(KeyCode::Char(character)));
        }
        assert!(disabled.update(key(KeyCode::Enter)).effects.is_empty());

        let terminal = render(&mut disabled);
        assert_eq!(
            row_segment(&terminal, 2, 1, 58),
            "│› Memory · enable in config: memory.enabled = true      │"
        );
        assert_eq!(
            terminal.backend().buffer()[(4, 2)].fg,
            Theme::default().muted()
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
    fn enter_does_nothing_when_search_has_no_matches() {
        let mut menu = ActionsMenu::new(available());
        menu.update(key(KeyCode::Char('z')));

        assert!(menu.update(key(KeyCode::Enter)).effects.is_empty());
    }

    #[test]
    fn escape_dismisses() {
        let mut menu = ActionsMenu::new(available());

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
