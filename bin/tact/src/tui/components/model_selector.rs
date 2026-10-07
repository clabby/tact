//! Searchable model menu for a new session.

use super::{
    file_finder::visible_query_tail,
    floating::Floating,
    node::{Component, ComponentUpdate, RenderRequest},
};
use crate::app::{
    model::{available, name},
    theme::Theme,
};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use nanocodex::HarnessModel as Model;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph},
};
use unicode_segmentation::UnicodeSegmentation;

const KEY_BINDINGS: [(&str, &str); 3] = [("↑↓", "move"), ("enter", "apply"), ("esc", "cancel")];

pub(super) enum ModelSelectorEvent {
    Terminal(Event),
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum ModelSelectorEffect {
    Apply(Model),
    Dismiss,
}

pub(super) struct ModelSelector {
    models: &'static [Model],
    current: Model,
    query: String,
    matches: Vec<usize>,
    selected: usize,
}

impl ModelSelector {
    pub(super) fn new(initial: Model, claude_enabled: bool) -> Self {
        let models = available(claude_enabled);
        let selected = models
            .iter()
            .position(|model| *model == initial)
            .unwrap_or(1);
        Self {
            models,
            current: initial,
            query: String::new(),
            matches: (0..models.len()).collect(),
            selected,
        }
    }

    fn update_key(&mut self, key: KeyEvent) -> ComponentUpdate<ModelSelectorEffect> {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return ComponentUpdate::none();
        }
        match key.code {
            KeyCode::Esc => ComponentUpdate {
                effects: vec![ModelSelectorEffect::Dismiss],
                render: RenderRequest::Immediate,
            },
            KeyCode::Enter => {
                let Some(&index) = self.matches.get(self.selected) else {
                    return ComponentUpdate::none();
                };
                ComponentUpdate {
                    effects: vec![ModelSelectorEffect::Apply(self.models[index])],
                    render: RenderRequest::Immediate,
                }
            }
            KeyCode::Up | KeyCode::Left => {
                self.selected = self.selected.saturating_sub(1);
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            KeyCode::Down | KeyCode::Right => {
                self.selected = (self.selected + 1).min(self.matches.len().saturating_sub(1));
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            KeyCode::Backspace => {
                if let Some((index, _)) = self.query.grapheme_indices(true).next_back() {
                    self.query.truncate(index);
                    self.refresh_matches();
                }
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    && !character.is_control() =>
            {
                self.query.push(character);
                self.refresh_matches();
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            _ => ComponentUpdate::none(),
        }
    }

    fn refresh_matches(&mut self) {
        let query = self.query.trim().to_ascii_lowercase();
        self.matches = self
            .models
            .iter()
            .enumerate()
            .filter(|(_, model)| {
                let provider = match model {
                    Model::Codex(_) => "openai codex",
                    Model::Claude(_) => "anthropic claude",
                };
                let label = name(**model).to_ascii_lowercase();
                label.contains(&query)
                    || label.replace(' ', "-").contains(&query)
                    || model.as_str().contains(&query)
                    || provider.contains(&query)
            })
            .map(|(index, _)| index)
            .collect();
        self.selected = 0;
    }
}

impl Component for ModelSelector {
    type Event = ModelSelectorEvent;
    type Effect = ModelSelectorEffect;

    fn update(&mut self, event: Self::Event) -> ComponentUpdate<Self::Effect> {
        match event {
            ModelSelectorEvent::Terminal(Event::Key(key)) => self.update_key(key),
            ModelSelectorEvent::Terminal(Event::Paste(text)) => {
                self.query
                    .extend(text.chars().filter(|character| !character.is_control()));
                self.refresh_matches();
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            ModelSelectorEvent::Terminal(_) => ComponentUpdate::none(),
        }
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        if area.is_empty() {
            return;
        }
        let selected_model = self
            .matches
            .get(self.selected)
            .map(|&index| self.models[index]);
        let color = selected_model.map_or(theme.accent(), |model| theme.model(model));
        let bindings = if area.height >= 6 && area.width >= 22 {
            &KEY_BINDINGS[..]
        } else {
            &[]
        };
        let layout = Floating::new("Select model", 68, self.models.len() as u16 + 6, bindings)
            .colors(color, color)
            .render(frame, area, theme);
        let mut body = layout.body;
        if body.is_empty() {
            return;
        }
        if body.height >= 2 {
            let query = visible_query_tail(&self.query, usize::from(body.width).saturating_sub(10));
            let search = Line::from(vec![
                Span::styled("  Search: ", Style::default().fg(theme.muted())),
                Span::styled(query, Style::default().fg(theme.text())),
                Span::styled("▏", Style::default().fg(color)),
            ]);
            frame.render_widget(Paragraph::new(search), Rect { height: 1, ..body });
            body.y += 1;
            body.height -= 1;
            if body.height > self.models.len() as u16 {
                body.y += 1;
                body.height -= 1;
            }
        }
        if self.matches.is_empty() {
            frame.render_widget(
                Paragraph::new("  No matching models").style(Style::default().fg(theme.muted())),
                body,
            );
            return;
        }
        let items = self.matches.iter().map(|&index| {
            let model = self.models[index];
            let provider = match model {
                Model::Codex(_) => "OpenAI",
                Model::Claude(_) => "Anthropic",
            };
            let mut spans = vec![Span::styled(
                if body.width >= 32 {
                    format!("{:<14}", name(model))
                } else {
                    name(model).to_owned()
                },
                Style::default().fg(theme.model(model)),
            )];
            if model == self.current {
                spans.push(Span::styled(" ✓", Style::default().fg(theme.model(model))));
            } else if body.width >= 32 {
                spans.push(Span::raw("  "));
            }
            if body.width >= 32 {
                spans.push(Span::styled(
                    format!("  {provider}"),
                    Style::default().fg(theme.muted()),
                ));
            }
            if body.width >= 56 {
                spans.push(Span::styled(
                    format!(" · {}", model.as_str()),
                    Style::default().fg(theme.muted()),
                ));
            }
            ListItem::new(Line::from(spans))
        });
        let list = List::new(items)
            .style(Style::default().fg(color))
            .highlight_symbol("▸ ")
            .highlight_style(
                Style::default()
                    .bg(theme.code_background())
                    .add_modifier(Modifier::BOLD),
            );
        let mut state = ListState::default().with_selected(Some(self.selected));
        frame.render_stateful_widget(list, body, &mut state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nanocodex::{ClaudeModel, Model as CodexModel};
    use ratatui::{Terminal, backend::TestBackend, style::Color};

    const SOL: Model = Model::Codex(CodexModel::Sol);
    const LUNA: Model = Model::Codex(CodexModel::Luna);
    const FABLE: Model = Model::Claude(ClaudeModel::Fable51);

    fn key(selector: &mut ModelSelector, code: KeyCode) -> ComponentUpdate<ModelSelectorEffect> {
        selector.update(ModelSelectorEvent::Terminal(Event::Key(KeyEvent::new(
            code,
            KeyModifiers::NONE,
        ))))
    }

    fn paste(selector: &mut ModelSelector, text: &str) {
        selector.update(ModelSelectorEvent::Terminal(Event::Paste(text.to_owned())));
    }

    fn render(
        selector: &mut ModelSelector,
        width: u16,
        height: u16,
        theme: &Theme,
    ) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| selector.render(frame, frame.area(), theme))
            .unwrap();
        terminal
    }

    fn rows(terminal: &Terminal<TestBackend>) -> Vec<String> {
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn navigation_and_apply_respect_the_enabled_roster() {
        let mut selector = ModelSelector::new(SOL, false);
        key(&mut selector, KeyCode::Up);
        key(&mut selector, KeyCode::Up);
        assert_eq!(
            key(&mut selector, KeyCode::Enter).effects,
            [ModelSelectorEffect::Apply(LUNA)]
        );
        for _ in 0..10 {
            key(&mut selector, KeyCode::Down);
        }
        assert_eq!(
            key(&mut selector, KeyCode::Enter).effects,
            [ModelSelectorEffect::Apply(Model::Codex(CodexModel::Astra))]
        );
        paste(&mut selector, "claude");
        assert!(key(&mut selector, KeyCode::Enter).effects.is_empty());
        let mut enabled = ModelSelector::new(FABLE, true);
        assert_eq!(
            key(&mut enabled, KeyCode::Enter).effects,
            [ModelSelectorEffect::Apply(FABLE)]
        );
        assert_eq!(
            key(&mut enabled, KeyCode::Esc).effects,
            [ModelSelectorEffect::Dismiss]
        );
    }

    #[test]
    fn searches_names_aliases_providers_and_canonical_ids() {
        for (query, expected) in [
            ("SoL", vec![SOL]),
            ("sonnet-5.5", vec![Model::Claude(ClaudeModel::Sonnet55)]),
            ("gpt-6.1-sol", vec![SOL]),
            ("claude-fable-5-1", vec![FABLE]),
            ("OpenAI", available(false).to_vec()),
            ("codex", available(false).to_vec()),
            ("Anthropic", available(true)[3..].to_vec()),
            ("claude", available(true)[3..].to_vec()),
        ] {
            let mut selector = ModelSelector::new(FABLE, true);
            paste(&mut selector, query);
            assert_eq!(
                selector
                    .matches
                    .iter()
                    .map(|&index| selector.models[index])
                    .collect::<Vec<_>>(),
                expected,
                "query: {query}"
            );
            assert_eq!(
                key(&mut selector, KeyCode::Enter).effects,
                [ModelSelectorEffect::Apply(expected[0])]
            );
        }
    }

    #[test]
    fn typing_paste_and_backspace_handle_graphemes_and_no_results() {
        let mut selector = ModelSelector::new(SOL, true);
        key(&mut selector, KeyCode::Char('é'));
        assert!(selector.matches.is_empty());
        assert!(key(&mut selector, KeyCode::Enter).effects.is_empty());
        assert!(
            rows(&render(&mut selector, 68, 12, &Theme::default()))
                .join("\n")
                .contains("No matching models")
        );
        key(&mut selector, KeyCode::Backspace);
        paste(&mut selector, "sol\ne\u{301}👩‍💻\t");
        assert_eq!(selector.query, "sole\u{301}👩‍💻");
        key(&mut selector, KeyCode::Backspace);
        assert_eq!(selector.query, "sole\u{301}");
        key(&mut selector, KeyCode::Backspace);
        assert_eq!(selector.query, "sol");
        assert_eq!(
            key(&mut selector, KeyCode::Enter).effects,
            [ModelSelectorEffect::Apply(SOL)]
        );
        for _ in 0..4 {
            key(&mut selector, KeyCode::Backspace);
        }
        assert!(selector.query.is_empty());
        assert_eq!(key(&mut selector, KeyCode::Enter).effects.len(), 1);
        let modified = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        selector.update(ModelSelectorEvent::Terminal(Event::Key(modified)));
        assert!(selector.query.is_empty());
    }

    #[test]
    fn vertical_rows_use_configured_colors_and_distinguish_current_from_selection() {
        let theme: Theme = toml::from_str("model_sol = '#123456'\nmodel_fable = 'red'\n").unwrap();
        let mut selector = ModelSelector::new(SOL, true);
        for _ in 0..4 {
            key(&mut selector, KeyCode::Down);
        }
        let terminal = render(&mut selector, 68, 12, &theme);
        let text = rows(&terminal);
        assert!(
            text.iter()
                .any(|row| row.contains("Sol") && row.contains('✓') && !row.contains('▸'))
        );
        assert!(text.iter().any(|row| row.contains("▸ Fable 5.1")
            && row.contains("Anthropic")
            && row.contains("claude-fable-5-1")));
        let buffer = terminal.backend().buffer();
        let sol_row = text.iter().position(|row| row.contains("Sol")).unwrap() as u16;
        let sol_x = text[usize::from(sol_row)]
            .chars()
            .position(|c| c == 'S')
            .unwrap() as u16;
        assert_eq!(buffer[(sol_x, sol_row)].fg, Color::Rgb(0x12, 0x34, 0x56));
        let marker = buffer
            .content
            .iter()
            .find(|cell| cell.symbol() == "▸")
            .unwrap();
        assert_eq!(marker.fg, Color::Red);
        assert_eq!(marker.bg, theme.code_background());
        assert!(marker.modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(0, 0)].fg, Color::Red);
    }

    #[test]
    fn narrow_and_tiny_frames_keep_selection_visible_and_preserve_chrome() {
        let theme = Theme::default();
        for (width, height) in [(30, 7), (22, 6), (12, 4), (6, 3), (1, 1), (0, 0)] {
            let mut selector = ModelSelector::new(FABLE, true);
            let terminal = render(&mut selector, width, height, &theme);
            if width >= 12 && height >= 4 {
                let text = rows(&terminal).join("\n");
                assert!(text.contains("▸ Fable"), "{width}x{height}: {text}");
                let buffer = terminal.backend().buffer();
                assert_eq!(buffer[(0, height - 1)].symbol(), "╰");
                assert_eq!(buffer[(width - 1, height - 1)].symbol(), "╯");
            }
        }
        let mut selector = ModelSelector::new(SOL, true);
        paste(&mut selector, &"界e\u{301}".repeat(40));
        let terminal = render(&mut selector, 30, 8, &theme);
        let buffer = terminal.backend().buffer();
        for y in 1..7 {
            assert_eq!(buffer[(29, y)].symbol(), "│");
        }
    }
}
