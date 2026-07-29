use super::{
    floating::Floating,
    node::{Component, ComponentUpdate, RenderRequest},
};
use crate::{review::ReviewRange, tui::theme::Theme};
use crossterm::event::{Event, KeyCode, KeyEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{List, ListItem, ListState},
};

const KEY_BINDINGS: [&str; 3] = ["↑↓ select", "enter review", "esc cancel"];

pub(super) enum ReviewRangeEvent {
    Terminal(Event),
}

pub(super) enum ReviewRangeEffect {
    Selected(ReviewRange),
    Dismiss,
}

pub(super) struct ReviewRangePicker {
    ranges: Vec<ReviewRange>,
    selected: usize,
}

impl ReviewRangePicker {
    pub(super) fn new(ranges: Vec<ReviewRange>) -> Self {
        Self {
            selected: ranges.len().saturating_sub(1),
            ranges,
        }
    }
}

impl Component for ReviewRangePicker {
    type Event = ReviewRangeEvent;
    type Effect = ReviewRangeEffect;

    fn update(&mut self, event: Self::Event) -> ComponentUpdate<Self::Effect> {
        let ReviewRangeEvent::Terminal(Event::Key(key)) = event else {
            return ComponentUpdate::none();
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return ComponentUpdate::none();
        }
        match key.code {
            KeyCode::Esc => ComponentUpdate {
                effects: vec![ReviewRangeEffect::Dismiss],
                render: RenderRequest::Immediate,
            },
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(self.ranges.len().saturating_sub(1));
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            KeyCode::Enter => {
                let Some(range) = self.ranges.get(self.selected).cloned() else {
                    return ComponentUpdate::none();
                };
                ComponentUpdate {
                    effects: vec![ReviewRangeEffect::Selected(range)],
                    render: RenderRequest::Immediate,
                }
            }
            _ => ComponentUpdate::none(),
        }
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let height = (self.ranges.len().saturating_mul(2)).min(18) as u16 + 4;
        let layout = Floating::new("Select changes to review", 72, height, &KEY_BINDINGS)
            .render(frame, area, theme);
        let items = self.ranges.iter().map(|range| {
            ListItem::new(vec![
                Line::from(Span::styled(
                    range.label(),
                    Style::default().fg(theme.text()),
                )),
                Line::from(Span::styled(
                    range.detail(),
                    Style::default().fg(theme.muted()),
                )),
            ])
        });
        let list = List::new(items).highlight_symbol("› ").highlight_style(
            Style::default()
                .fg(theme.accent())
                .add_modifier(Modifier::BOLD),
        );
        let mut state = ListState::default().with_selected(Some(self.selected));
        frame.render_stateful_widget(list, layout.body, &mut state);
    }
}
