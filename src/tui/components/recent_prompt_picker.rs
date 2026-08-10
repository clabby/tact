//! Picker for prompts from the current session or all persisted sessions.

use super::{
    floating::Floating,
    node::{Component, ComponentUpdate, RenderRequest},
};
use crate::tui::{session::RecentPrompt, theme::Theme};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};

const KEY_BINDINGS: [&str; 5] = [
    "↑↓ move",
    "pgup/pgdn preview",
    "enter/tab select",
    "f scope",
    "esc close",
];
const LIST_HEIGHT: u16 = 7;

pub(super) enum RecentPromptPickerEvent {
    Terminal(Event),
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum RecentPromptPickerEffect {
    Dismiss,
    Insert(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RecentPromptScope {
    Global,
    CurrentSession,
}

pub(super) struct RecentPromptPicker {
    prompts: Vec<RecentPrompt>,
    current_session_id: String,
    scope: RecentPromptScope,
    visible: Vec<usize>,
    selected: usize,
    preview_scroll: u16,
}

impl RecentPromptPicker {
    pub(super) fn new(prompts: Vec<RecentPrompt>, current_session_id: String) -> Self {
        let visible = (0..prompts.len()).collect();
        Self {
            prompts,
            current_session_id,
            scope: RecentPromptScope::Global,
            visible,
            selected: 0,
            preview_scroll: 0,
        }
    }

    #[cfg(test)]
    const fn scope(&self) -> RecentPromptScope {
        self.scope
    }

    fn update_key(&mut self, key: KeyEvent) -> ComponentUpdate<RecentPromptPickerEffect> {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return ComponentUpdate::none();
        }

        match key.code {
            KeyCode::Esc => Self::effect(RecentPromptPickerEffect::Dismiss),
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                self.preview_scroll = 0;
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            KeyCode::Down => {
                if !self.visible.is_empty() {
                    self.selected = (self.selected + 1).min(self.visible.len() - 1);
                }
                self.preview_scroll = 0;
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            KeyCode::PageUp => {
                self.preview_scroll = self.preview_scroll.saturating_sub(1);
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            KeyCode::PageDown => {
                self.preview_scroll = self.preview_scroll.saturating_add(1);
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            KeyCode::Enter | KeyCode::Tab => self.select(),
            KeyCode::Char('f') if key.modifiers == KeyModifiers::NONE => self.toggle_scope(),
            _ => ComponentUpdate::none(),
        }
    }

    fn toggle_scope(&mut self) -> ComponentUpdate<RecentPromptPickerEffect> {
        self.scope = match self.scope {
            RecentPromptScope::Global => RecentPromptScope::CurrentSession,
            RecentPromptScope::CurrentSession => RecentPromptScope::Global,
        };
        self.refresh_visible();
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn refresh_visible(&mut self) {
        self.visible = self
            .prompts
            .iter()
            .enumerate()
            .filter(|(_, prompt)| {
                self.scope == RecentPromptScope::Global
                    || prompt.session_id == self.current_session_id
            })
            .map(|(index, _)| index)
            .collect();
        self.selected = 0;
        self.preview_scroll = 0;
    }

    fn select(&self) -> ComponentUpdate<RecentPromptPickerEffect> {
        let Some(prompt) = self.selected_prompt() else {
            return ComponentUpdate::none();
        };
        Self::effect(RecentPromptPickerEffect::Insert(prompt.text.clone()))
    }

    fn selected_prompt(&self) -> Option<&RecentPrompt> {
        let index = self.visible.get(self.selected)?;
        self.prompts.get(*index)
    }

    fn effect(effect: RecentPromptPickerEffect) -> ComponentUpdate<RecentPromptPickerEffect> {
        ComponentUpdate {
            effects: vec![effect],
            render: RenderRequest::Immediate,
        }
    }

    fn render_scope(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        if area.is_empty() {
            return;
        }

        let scope = match self.scope {
            RecentPromptScope::Global => "Global",
            RecentPromptScope::CurrentSession => "Current session",
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Scope: ", Style::default().fg(theme.muted())),
                Span::styled(
                    scope,
                    Style::default()
                        .fg(theme.text())
                        .add_modifier(Modifier::BOLD),
                ),
            ])),
            area,
        );
    }

    fn render_prompts(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        if area.is_empty() {
            return;
        }
        if self.visible.is_empty() {
            frame.render_widget(
                Paragraph::new("No prompts in this scope")
                    .style(Style::default().fg(theme.muted())),
                area,
            );
            return;
        }

        let items = self.visible.iter().enumerate().map(|(position, index)| {
            let prompt = &self.prompts[*index];
            let mut spans = vec![Span::styled(
                format!("{}. {}", position + 1, one_line_preview(&prompt.text)),
                Style::default().fg(theme.text()),
            )];
            if self.scope == RecentPromptScope::Global {
                spans.push(Span::styled(
                    format!("  · {} · {}", prompt.workspace.display(), prompt.session_id),
                    Style::default().fg(theme.muted()),
                ));
            }
            ListItem::new(Line::from(spans))
        });
        let list = List::new(items)
            .highlight_symbol("› ")
            .highlight_style(Style::default().fg(theme.accent()));
        let mut state = ListState::default().with_selected(Some(self.selected));
        frame.render_stateful_widget(list, area, &mut state);
    }

    fn render_preview(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        if area.is_empty() {
            return;
        }

        let block = Block::new()
            .borders(Borders::TOP)
            .title(" Preview ")
            .border_style(Style::default().fg(theme.border()))
            .title_style(Style::default().fg(theme.muted()));
        let text = self
            .selected_prompt()
            .map_or("", |prompt| prompt.text.as_str());
        frame.render_widget(
            Paragraph::new(text)
                .style(Style::default().fg(theme.text()))
                .block(block)
                .wrap(Wrap { trim: false })
                .scroll((self.preview_scroll, 0)),
            area,
        );
    }
}

impl Component for RecentPromptPicker {
    type Event = RecentPromptPickerEvent;
    type Effect = RecentPromptPickerEffect;

    fn update(&mut self, event: Self::Event) -> ComponentUpdate<Self::Effect> {
        match event {
            RecentPromptPickerEvent::Terminal(Event::Key(key)) => self.update_key(key),
            RecentPromptPickerEvent::Terminal(_) => ComponentUpdate::none(),
        }
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        if area.is_empty() {
            return;
        }

        let layout =
            Floating::new("Recent prompts", 82, 22, &KEY_BINDINGS).render(frame, area, theme);
        if layout.body.is_empty() {
            return;
        }

        let scope_area = Rect {
            height: 1,
            ..layout.body
        };
        let remaining_height = layout.body.height.saturating_sub(scope_area.height);
        let list_height = LIST_HEIGHT.min(remaining_height.saturating_add(1) / 2);
        let list_area = Rect {
            y: scope_area.bottom(),
            height: list_height,
            ..layout.body
        };
        let preview_area = Rect {
            y: list_area.bottom(),
            height: remaining_height.saturating_sub(list_height),
            ..layout.body
        };

        self.render_scope(frame, scope_area, theme);
        self.render_prompts(frame, list_area, theme);
        self.render_preview(frame, preview_area, theme);
    }
}

fn one_line_preview(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::{
        Component, RecentPromptPicker, RecentPromptPickerEffect, RecentPromptPickerEvent,
        RecentPromptScope,
    };
    use crate::tui::{session::RecentPrompt, theme::Theme};
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend};
    use std::path::PathBuf;

    fn prompt(text: &str, session_id: &str, workspace: &str) -> RecentPrompt {
        RecentPrompt {
            text: text.to_owned(),
            recorded_at_unix_ms: 1,
            session_id: session_id.to_owned(),
            workspace: PathBuf::from(workspace),
        }
    }

    fn picker() -> RecentPromptPicker {
        RecentPromptPicker::new(
            vec![
                prompt("newest", "other", "/work/other"),
                prompt("  exact\n\n    prompt  ", "current", "/work/current"),
                prompt("oldest", "current", "/work/current"),
            ],
            "current".to_owned(),
        )
    }

    fn key(code: KeyCode) -> RecentPromptPickerEvent {
        key_with_modifiers(code, KeyModifiers::NONE)
    }

    fn key_with_modifiers(code: KeyCode, modifiers: KeyModifiers) -> RecentPromptPickerEvent {
        RecentPromptPickerEvent::Terminal(Event::Key(KeyEvent::new(code, modifiers)))
    }

    #[test]
    fn defaults_to_global_and_preserves_loader_order() {
        let mut picker = picker();

        assert_eq!(picker.scope(), RecentPromptScope::Global);
        assert_eq!(
            picker.update(key(KeyCode::Enter)).effects,
            [RecentPromptPickerEffect::Insert("newest".to_owned())]
        );
        picker.update(key(KeyCode::Down));
        assert_eq!(
            picker.update(key(KeyCode::Tab)).effects,
            [RecentPromptPickerEffect::Insert(
                "  exact\n\n    prompt  ".to_owned()
            )]
        );
    }

    #[test]
    fn unmodified_f_toggles_current_session_filter() {
        let mut picker = picker();

        picker.update(key_with_modifiers(
            KeyCode::Char('f'),
            KeyModifiers::CONTROL,
        ));
        assert_eq!(picker.scope(), RecentPromptScope::Global);

        picker.update(key(KeyCode::Char('f')));
        assert_eq!(picker.scope(), RecentPromptScope::CurrentSession);
        assert_eq!(picker.visible, [1, 2]);
        assert_eq!(
            picker.update(key(KeyCode::Enter)).effects,
            [RecentPromptPickerEffect::Insert(
                "  exact\n\n    prompt  ".to_owned()
            )]
        );

        picker.update(key(KeyCode::Char('f')));
        assert_eq!(picker.scope(), RecentPromptScope::Global);
        assert_eq!(picker.visible, [0, 1, 2]);
    }

    #[test]
    fn navigation_clamps_and_escape_dismisses() {
        let mut picker = picker();

        picker.update(key(KeyCode::Up));
        assert_eq!(picker.selected, 0);
        for _ in 0..5 {
            picker.update(key(KeyCode::Down));
        }
        assert_eq!(picker.selected, 2);
        assert_eq!(
            picker.update(key(KeyCode::Esc)).effects,
            [RecentPromptPickerEffect::Dismiss]
        );
    }

    #[test]
    fn long_previews_can_be_scrolled_and_reset_for_the_next_prompt() {
        let mut picker = picker();

        for _ in 0..12 {
            picker.update(key(KeyCode::PageDown));
        }
        assert_eq!(picker.preview_scroll, 12);

        picker.update(key(KeyCode::PageUp));
        assert_eq!(picker.preview_scroll, 11);

        picker.update(key(KeyCode::Down));
        assert_eq!(picker.preview_scroll, 0);
    }

    #[test]
    fn preview_preserves_leading_spaces_blank_lines_and_trailing_spaces() {
        let mut picker = RecentPromptPicker::new(
            vec![prompt(
                "first\n  indented\n\nlast  ",
                "current",
                "/work/current",
            )],
            "current".to_owned(),
        );
        let mut terminal = Terminal::new(TestBackend::new(90, 26)).unwrap();

        terminal
            .draw(|frame| picker.render(frame, frame.area(), &Theme::default()))
            .unwrap();

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(5, 12)].symbol(), "f");
        assert_eq!(buffer[(7, 13)].symbol(), "i");
        assert_eq!(buffer[(5, 14)].symbol(), " ");
        assert_eq!(buffer[(5, 15)].symbol(), "l");
        assert_eq!(buffer[(9, 15)].symbol(), " ");
        assert_eq!(buffer[(10, 15)].symbol(), " ");
    }

    #[test]
    fn render_includes_numbered_global_metadata() {
        let mut picker = picker();
        let mut terminal = Terminal::new(TestBackend::new(100, 26)).unwrap();

        terminal
            .draw(|frame| picker.render(frame, frame.area(), &Theme::default()))
            .unwrap();

        let buffer = terminal.backend().buffer();
        let row = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .find(|row| row.contains("1. newest"))
            .expect("numbered prompt row");
        assert!(row.contains("/work/other"));
        assert!(row.contains("other"));
    }

    #[test]
    fn renders_on_a_narrow_terminal() {
        let mut picker = picker();
        let mut terminal = Terminal::new(TestBackend::new(3, 3)).unwrap();

        terminal
            .draw(|frame| picker.render(frame, frame.area(), &Theme::default()))
            .unwrap();

        assert_eq!(terminal.backend().buffer().area.width, 3);
    }
}
