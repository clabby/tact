//! Shared chrome and layout for centered modal components.

use crate::tui::theme::Theme;
use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
};

const KEY_BINDING_SEPARATOR: &str = " · ";

pub(super) struct Floating<'a> {
    title: &'a str,
    width: u16,
    height: u16,
    key_bindings: &'a [&'a str],
    placement: Placement,
    border_color: Option<Color>,
    title_color: Option<Color>,
}

#[derive(Clone, Copy, Default)]
enum Placement {
    #[default]
    Center,
    Top,
}

pub(super) struct FloatingLayout {
    pub(super) body: Rect,
}

impl<'a> Floating<'a> {
    pub(super) const fn new(
        title: &'a str,
        width: u16,
        height: u16,
        key_bindings: &'a [&'a str],
    ) -> Self {
        Self {
            title,
            width,
            height,
            key_bindings,
            placement: Placement::Center,
            border_color: None,
            title_color: None,
        }
    }

    pub(super) const fn at_top(mut self) -> Self {
        self.placement = Placement::Top;
        self
    }

    pub(super) const fn colors(mut self, border: Color, title: Color) -> Self {
        self.border_color = Some(border);
        self.title_color = Some(title);
        self
    }

    pub(super) fn render(self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) -> FloatingLayout {
        let popup = match self.placement {
            Placement::Center => centered(area, self.width, self.height),
            Placement::Top => top_centered(area, self.width, self.height),
        };
        let border_color = self.border_color.unwrap_or_else(|| theme.border());
        let title_color = self.title_color.unwrap_or_else(|| theme.accent());
        let mut block = Block::new()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(border_color));
        if !self.title.is_empty() {
            block = block
                .title(format!(" {} ", self.title))
                .title_alignment(Alignment::Center)
                .title_style(
                    Style::default()
                        .fg(title_color)
                        .add_modifier(Modifier::BOLD),
                );
        }
        let inner = block.inner(popup);
        frame.render_widget(Clear, popup);
        frame.render_widget(block, popup);

        let (body, footer) = if self.key_bindings.is_empty() {
            (inner, Rect::default())
        } else {
            split_footer(inner)
        };
        self.render_key_bindings(frame, footer, theme);
        FloatingLayout { body }
    }

    fn render_key_bindings(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        if area.is_empty() || self.key_bindings.is_empty() {
            return;
        }

        let mut spans = Vec::with_capacity(self.key_bindings.len().saturating_mul(2));
        for (index, key_binding) in self.key_bindings.iter().enumerate() {
            if index > 0 {
                spans.push(Span::raw(KEY_BINDING_SEPARATOR));
            }
            spans.push(Span::raw(*key_binding));
        }
        let line = Line::from(spans).style(Style::default().fg(theme.border()));
        frame.render_widget(Paragraph::new(line).alignment(Alignment::Center), area);
    }
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

fn top_centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y,
        width,
        height: height.min(area.height),
    }
}

fn split_footer(inner: Rect) -> (Rect, Rect) {
    if inner.is_empty() {
        return (inner, inner);
    }
    let footer = Rect {
        y: inner.bottom() - 1,
        height: 1,
        ..inner
    };
    let body = Rect {
        height: inner.height - 1,
        ..inner
    };
    (body, footer)
}

#[cfg(test)]
mod tests {
    use super::Floating;
    use crate::tui::theme::Theme;
    use ratatui::{Terminal, backend::TestBackend, style::Color};

    #[test]
    fn floating_centers_rounded_chrome_and_border_colored_key_bindings() {
        let mut terminal = Terminal::new(TestBackend::new(20, 8)).unwrap();

        terminal
            .draw(|frame| {
                Floating::new("Test", 16, 6, &["left", "right"]).render(
                    frame,
                    frame.area(),
                    &Theme::default(),
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(2, 1)].symbol(), "╭");
        assert_eq!(buffer[(17, 6)].symbol(), "╯");
        assert_eq!(buffer[(4, 5)].symbol(), "l");
        assert_eq!(buffer[(9, 5)].symbol(), "·");
        assert_eq!(buffer[(9, 5)].fg, Color::DarkGray);
    }
}
