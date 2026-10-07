//! Animated selector for the speed preference.

use super::{
    dial::AnimatedDial,
    floating::Floating,
    node::{Component, ComponentUpdate, RenderRequest},
};
use crate::app::{config::Speed, theme::Theme};
use crossterm::event::{Event, KeyCode, KeyEventKind};
use nanocodex::HarnessModel as Model;
use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use std::time::Instant;

const KEY_BINDINGS: [(&str, &str); 3] = [("←/→", "speed"), ("enter", "apply"), ("esc", "cancel")];

pub(super) enum SpeedEvent {
    Terminal { event: Event, now: Instant },
    AnimationFrame(Instant),
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum SpeedEffect {
    Apply(Speed),
    Dismiss,
}

pub(super) struct SpeedSelector {
    dial: AnimatedDial,
    model: Model,
}

impl SpeedSelector {
    pub(super) fn new(initial: Speed, model: Model) -> Self {
        let selected = Speed::ALL
            .iter()
            .position(|speed| *speed == initial)
            .unwrap();
        Self {
            dial: AnimatedDial::new(selected, Speed::ALL.len()),
            model,
        }
    }

    pub(super) fn animation_deadline(&self) -> Option<Instant> {
        self.dial.animation_deadline()
    }

    fn selected_speed(&self) -> Speed {
        Speed::ALL[self.dial.selected()]
    }
}

impl Component for SpeedSelector {
    type Event = SpeedEvent;
    type Effect = SpeedEffect;

    fn update(&mut self, event: Self::Event) -> ComponentUpdate<Self::Effect> {
        match event {
            SpeedEvent::Terminal {
                event: Event::Key(key),
                now,
            } if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => match key.code {
                KeyCode::Left | KeyCode::Up => {
                    self.dial.select_relative(-1, now);
                    ComponentUpdate::render(RenderRequest::Immediate)
                }
                KeyCode::Right | KeyCode::Down => {
                    self.dial.select_relative(1, now);
                    ComponentUpdate::render(RenderRequest::Immediate)
                }
                KeyCode::Enter => ComponentUpdate {
                    effects: vec![SpeedEffect::Apply(self.selected_speed())],
                    render: RenderRequest::Immediate,
                },
                KeyCode::Esc | KeyCode::Backspace => ComponentUpdate {
                    effects: vec![SpeedEffect::Dismiss],
                    render: RenderRequest::Immediate,
                },
                _ => ComponentUpdate::none(),
            },
            SpeedEvent::Terminal { .. } => ComponentUpdate::none(),
            SpeedEvent::AnimationFrame(now) => {
                if self.dial.advance_animation(now) {
                    ComponentUpdate::render(RenderRequest::Immediate)
                } else {
                    ComponentUpdate::none()
                }
            }
        }
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        if area.is_empty() {
            return;
        }
        let layout = Floating::new("Speed", 48, 17, &KEY_BINDINGS).render(frame, area, theme);
        if layout.body.is_empty() {
            return;
        }
        let selected = self.selected_speed();
        let effective = selected.for_model(self.model);
        let labels = self
            .dial
            .render(frame, layout.body, theme, theme.speed(selected));
        let mut lines = vec![Line::from(vec![
            Span::styled("Selected Speed:", Style::default().fg(theme.border())),
            Span::styled(
                format!(" {}", selected.as_str()),
                Style::default()
                    .fg(theme.speed(selected))
                    .add_modifier(Modifier::BOLD),
            ),
        ])];
        if effective != selected {
            lines.push(Line::styled(
                format!("Uses {} with this model", effective.as_str()),
                Style::default().fg(theme.muted()),
            ));
        }
        frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), labels);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        super::{
            dial::{ANIMATION_DURATION, ANIMATION_FRAME_INTERVAL},
            node::Component,
        },
        SpeedEffect, SpeedEvent, SpeedSelector,
    };
    use crate::app::{config::Speed, theme::Theme};
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use nanocodex::{ClaudeModel, HarnessModel as Model, Model as CodexModel};
    use ratatui::{Terminal, backend::TestBackend};
    use std::time::Instant;

    fn key(code: KeyCode, now: Instant) -> SpeedEvent {
        SpeedEvent::Terminal {
            event: Event::Key(KeyEvent::new(code, KeyModifiers::NONE)),
            now,
        }
    }

    fn render(selector: &mut SpeedSelector, width: u16, height: u16) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| selector.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        terminal
    }

    fn text(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn speed_dial_has_three_stops_and_shares_effort_animation_timing() {
        let start = Instant::now();
        let mut selector = SpeedSelector::new(Speed::Standard, Model::Codex(CodexModel::Astra));
        let terminal = render(&mut selector, 60, 18);
        assert_eq!(
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .filter(|cell| cell.symbol() == "●")
                .count(),
            3
        );
        assert_eq!(terminal.backend().buffer()[(29, 2)].symbol(), "●");
        assert!(!terminal.backend().cursor_visible());
        selector.update(key(KeyCode::Right, start));
        assert_eq!(
            selector.animation_deadline(),
            Some(start + ANIMATION_FRAME_INTERVAL)
        );
        selector.update(SpeedEvent::AnimationFrame(start + ANIMATION_DURATION));
        assert_eq!(selector.animation_deadline(), None);
    }

    #[test]
    fn arrows_wrap_preferences_and_enter_applies_the_selected_preference() {
        let now = Instant::now();
        let mut selector = SpeedSelector::new(Speed::Standard, Model::Codex(CodexModel::Sol));
        selector.update(key(KeyCode::Left, now));
        assert_eq!(selector.selected_speed(), Speed::Ultrafast);
        assert_eq!(
            selector.update(key(KeyCode::Enter, now)).effects,
            [SpeedEffect::Apply(Speed::Ultrafast)]
        );
        selector.update(key(KeyCode::Right, now));
        assert_eq!(selector.selected_speed(), Speed::Standard);
        selector.update(key(KeyCode::Down, now));
        assert_eq!(selector.selected_speed(), Speed::Fast);
        selector.update(key(KeyCode::Up, now));
        assert_eq!(selector.selected_speed(), Speed::Standard);
        assert_eq!(
            selector.update(key(KeyCode::Esc, now)).effects,
            [SpeedEffect::Dismiss]
        );
        assert_eq!(
            selector.update(key(KeyCode::Backspace, now)).effects,
            [SpeedEffect::Dismiss]
        );
    }

    #[test]
    fn fallback_is_shown_without_changing_the_preference() {
        for (model, fallback) in [
            (
                Model::Codex(CodexModel::Sol),
                Some("Uses fast with this model"),
            ),
            (
                Model::Claude(ClaudeModel::Opus55),
                Some("Uses fast with this model"),
            ),
            (
                Model::Claude(ClaudeModel::Sonnet55),
                Some("Uses standard with this model"),
            ),
            (
                Model::Claude(ClaudeModel::Fable51),
                Some("Uses standard with this model"),
            ),
            (Model::Codex(CodexModel::Astra), None),
        ] {
            let mut selector = SpeedSelector::new(Speed::Ultrafast, model);
            let rendered = text(&render(&mut selector, 60, 18));
            assert!(rendered.contains("Selected Speed: ultrafast"));
            match fallback {
                Some(label) => assert!(rendered.contains(label)),
                None => assert!(!rendered.contains("Uses ")),
            }
            assert_eq!(
                selector.update(key(KeyCode::Enter, Instant::now())).effects,
                [SpeedEffect::Apply(Speed::Ultrafast)]
            );
        }
    }

    #[test]
    fn narrow_terminals_do_not_overflow_the_selector() {
        let mut selector = SpeedSelector::new(Speed::Ultrafast, Model::Codex(CodexModel::Sol));
        let terminal = render(&mut selector, 3, 4);
        assert_eq!(terminal.backend().buffer().area.width, 3);
    }
}
