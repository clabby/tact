//! Animated circular selector for reasoning effort.
//!
//! [EffortSelector] keeps the highlighted [ReasoningEffort] in an
//! [AnimatedDial] plus the pro-mode toggle. It consumes key presses through
//! [EffortEvent::Terminal] and animation ticks through
//! [EffortEvent::AnimationFrame]; the host schedules ticks from
//! [EffortSelector::animation_deadline]. Arrow keys move the dial, Enter emits
//! [EffortEffect::Apply] with the effort and pro flag, and Esc or Backspace emits
//! [EffortEffect::Dismiss].
//!
//! Pro mode is only shown and toggleable (`p`) when the model offers it, and it
//! is forced off otherwise, so an applied pro flag is always supported.

use super::{
    dial::AnimatedDial,
    floating::Floating,
    node::{Component, ComponentUpdate, RenderRequest},
};
use crate::app::{config::ReasoningEffort, theme::Theme};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use std::time::Instant;

const KEY_BINDINGS: [(&str, &str); 4] = [
    ("←/→", "effort"),
    ("p", "pro"),
    ("enter", "apply"),
    ("esc", "cancel"),
];

pub(super) enum EffortEvent {
    Terminal { event: Event, now: Instant },
    AnimationFrame(Instant),
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum EffortEffect {
    Apply(ReasoningEffort, bool),
    Dismiss,
}

pub(super) struct EffortSelector {
    dial: AnimatedDial,
    pro: bool,
    pro_available: bool,
}

impl EffortSelector {
    pub(super) fn new(initial: ReasoningEffort, pro: bool, pro_available: bool) -> Self {
        Self {
            dial: AnimatedDial::new(initial.index(), ReasoningEffort::ALL.len()),
            pro: pro && pro_available,
            pro_available,
        }
    }

    pub(super) fn animation_deadline(&self) -> Option<Instant> {
        self.dial.animation_deadline()
    }

    fn update_key(&mut self, key: KeyEvent, now: Instant) -> ComponentUpdate<EffortEffect> {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return ComponentUpdate::none();
        }

        match key.code {
            KeyCode::Left | KeyCode::Up => {
                self.dial.select_relative(-1, now);
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            KeyCode::Right | KeyCode::Down => {
                self.dial.select_relative(1, now);
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            KeyCode::Char('p') if self.pro_available => {
                self.pro = !self.pro;
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            KeyCode::Enter => ComponentUpdate {
                effects: vec![EffortEffect::Apply(self.selected_effort(), self.pro)],
                render: RenderRequest::Immediate,
            },
            KeyCode::Esc | KeyCode::Backspace => ComponentUpdate {
                effects: vec![EffortEffect::Dismiss],
                render: RenderRequest::Immediate,
            },
            _ => ComponentUpdate::none(),
        }
    }

    fn selected_effort(&self) -> ReasoningEffort {
        ReasoningEffort::ALL[self.dial.selected()]
    }

    fn render_labels(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let effort = self.selected_effort();
        let mut lines = vec![
            Line::from(vec![
                Span::styled("Selected Effort:", Style::default().fg(theme.border())),
                Span::styled(
                    format!(" {}", effort.as_str()),
                    Style::default()
                        .fg(theme.effort(effort))
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                Span::styled("Pro: ", Style::default().fg(Color::Green)),
                Span::styled(
                    if self.pro { "on" } else { "off" },
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
        ];
        if !self.pro_available {
            lines.truncate(1);
        }
        frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), area);
    }
}

impl Component for EffortSelector {
    type Event = EffortEvent;
    type Effect = EffortEffect;

    fn update(&mut self, event: Self::Event) -> ComponentUpdate<Self::Effect> {
        match event {
            EffortEvent::Terminal {
                event: Event::Key(key),
                now,
            } => self.update_key(key, now),
            EffortEvent::Terminal { .. } => ComponentUpdate::none(),
            EffortEvent::AnimationFrame(now) => {
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

        let bindings = if self.pro_available {
            &KEY_BINDINGS[..]
        } else {
            &[("←/→", "effort"), ("enter", "apply"), ("esc", "cancel")][..]
        };
        let layout = Floating::new("Effort", 48, 17, bindings).render(frame, area, theme);
        if layout.body.is_empty() {
            return;
        }

        let labels = self.dial.render(
            frame,
            layout.body,
            theme,
            theme.effort(self.selected_effort()),
        );
        self.render_labels(frame, labels, theme);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        super::dial::{ANIMATION_DURATION, ANIMATION_FRAME_INTERVAL},
        Component, EffortEffect, EffortEvent, EffortSelector,
    };
    use crate::app::{config::ReasoningEffort, theme::Theme};
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend, style::Color};
    use std::time::{Duration, Instant};

    fn key(code: KeyCode, now: Instant) -> EffortEvent {
        EffortEvent::Terminal {
            event: Event::Key(KeyEvent::new(code, KeyModifiers::NONE)),
            now,
        }
    }

    fn colored_dial_dots(terminal: &Terminal<TestBackend>, color: Color) -> usize {
        let buffer = terminal.backend().buffer();
        (2..=10)
            .flat_map(|y| (21..=37).map(move |x| (x, y)))
            .filter(|position| {
                matches!(buffer[*position].symbol(), "•" | "●") && buffer[*position].fg == color
            })
            .count()
    }

    #[test]
    fn dial_has_five_evenly_spaced_stops_with_one_at_the_top() {
        let mut selector = EffortSelector::new(ReasoningEffort::Low, false, true);
        let mut terminal = Terminal::new(TestBackend::new(60, 18)).unwrap();

        terminal
            .draw(|frame| selector.render(frame, frame.area(), &Theme::default()))
            .unwrap();

        let buffer = terminal.backend().buffer();
        let thick_dots = buffer
            .content
            .iter()
            .filter(|cell| cell.symbol() == "●")
            .count();
        assert_eq!(thick_dots, 5);
        assert_eq!(buffer[(29, 1)].symbol(), " ");
        assert_eq!(buffer[(29, 2)].symbol(), "●");
        assert_eq!(buffer[(29, 2)].fg, Color::Gray);
        assert_eq!(buffer[(20, 12)].fg, Color::DarkGray);
        let footer = (6..54)
            .map(|x| buffer[(x, 15)].symbol())
            .collect::<String>();
        assert_eq!(footer, "│ ←/→ effort · p pro · enter apply · esc cancel│");
        assert!((7..53).all(|x| buffer[(x, 14)].symbol() == " "));
    }

    #[test]
    fn effort_selector_hides_the_terminal_cursor() {
        let mut selector = EffortSelector::new(ReasoningEffort::Low, false, true);
        let mut terminal = Terminal::new(TestBackend::new(60, 18)).unwrap();

        terminal
            .draw(|frame| selector.render(frame, frame.area(), &Theme::default()))
            .unwrap();

        assert!(!terminal.backend().cursor_visible());
    }

    #[test]
    fn dial_is_symmetric_in_terminal_cells() {
        let mut selector = EffortSelector::new(ReasoningEffort::Low, false, true);
        let mut terminal = Terminal::new(TestBackend::new(60, 18)).unwrap();
        terminal
            .draw(|frame| selector.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        let buffer = terminal.backend().buffer();

        for y in 2..=10 {
            for x in 21..=37 {
                if !matches!(buffer[(x, y)].symbol(), "•" | "●") {
                    continue;
                }
                assert!(matches!(buffer[(58 - x, y)].symbol(), "•" | "●"));
                assert!(matches!(buffer[(x, 12 - y)].symbol(), "•" | "●"));
            }
        }
    }

    #[test]
    fn animation_fills_thick_dots_in_the_new_effort_color() {
        let start = Instant::now();
        let mut selector = EffortSelector::new(ReasoningEffort::Medium, false, true);
        let mut terminal = Terminal::new(TestBackend::new(60, 18)).unwrap();

        terminal
            .draw(|frame| selector.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        assert_eq!(terminal.backend().buffer()[(29, 2)].fg, Color::Cyan);
        assert_eq!(terminal.backend().buffer()[(37, 7)].symbol(), "•");
        assert_eq!(terminal.backend().buffer()[(37, 7)].fg, Color::DarkGray);

        selector.update(key(KeyCode::Right, start));
        terminal
            .draw(|frame| selector.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        assert_eq!(terminal.backend().buffer()[(29, 2)].fg, Color::Yellow);
        assert_eq!(terminal.backend().buffer()[(37, 7)].fg, Color::DarkGray);

        selector.update(EffortEvent::AnimationFrame(start + ANIMATION_DURATION / 2));
        terminal
            .draw(|frame| selector.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        assert_eq!(terminal.backend().buffer()[(37, 7)].fg, Color::Yellow);
    }

    #[test]
    fn arrows_wrap_around_the_effort_levels() {
        let now = Instant::now();
        let mut selector = EffortSelector::new(ReasoningEffort::Low, false, true);

        selector.update(key(KeyCode::Left, now));
        assert_eq!(selector.selected_effort(), ReasoningEffort::Max);
        selector.update(key(KeyCode::Right, now));
        assert_eq!(selector.selected_effort(), ReasoningEffort::Low);
        selector.update(key(KeyCode::Down, now));
        assert_eq!(selector.selected_effort(), ReasoningEffort::Medium);
        selector.update(key(KeyCode::Up, now));
        assert_eq!(selector.selected_effort(), ReasoningEffort::Low);
    }

    #[test]
    fn transitions_use_pi_timing_and_cubic_easing() {
        let start = Instant::now();
        let mut selector = EffortSelector::new(ReasoningEffort::Low, false, true);

        selector.update(key(KeyCode::Right, start));
        assert_eq!(
            selector.animation_deadline(),
            Some(start + ANIMATION_FRAME_INTERVAL)
        );

        selector.update(EffortEvent::AnimationFrame(start + ANIMATION_DURATION / 2));
        assert!((selector.dial.displayed_phase - 0.875).abs() < f64::EPSILON);

        selector.update(EffortEvent::AnimationFrame(start + ANIMATION_DURATION));
        assert_eq!(selector.dial.displayed_phase, 1.0);
        assert_eq!(selector.animation_deadline(), None);
    }

    #[test]
    fn max_to_low_wraps_clockwise_through_the_top_anchor() {
        let start = Instant::now();
        let mut selector = EffortSelector::new(ReasoningEffort::Max, false, true);

        selector.update(key(KeyCode::Right, start));
        selector.update(EffortEvent::AnimationFrame(start + ANIMATION_DURATION / 2));

        assert_eq!(selector.selected_effort(), ReasoningEffort::Low);
        assert!((selector.dial.displayed_phase - 4.875).abs() < f64::EPSILON);
        assert!((selector.dial.displayed_fill - 0.5).abs() < f64::EPSILON);

        selector.update(EffortEvent::AnimationFrame(start + ANIMATION_DURATION));
        assert_eq!(selector.dial.displayed_phase, 0.0);
        assert_eq!(selector.dial.displayed_fill, 0.0);
    }

    #[test]
    fn max_to_low_progressively_removes_the_colored_arc() {
        let start = Instant::now();
        let mut selector = EffortSelector::new(ReasoningEffort::Max, false, true);
        let mut terminal = Terminal::new(TestBackend::new(60, 18)).unwrap();
        selector.update(key(KeyCode::Right, start));

        let mut colored = Vec::new();
        for elapsed in [Duration::ZERO, ANIMATION_DURATION / 2, ANIMATION_DURATION] {
            if !elapsed.is_zero() {
                selector.update(EffortEvent::AnimationFrame(start + elapsed));
            }
            terminal
                .draw(|frame| selector.render(frame, frame.area(), &Theme::default()))
                .unwrap();

            colored.push(colored_dial_dots(&terminal, Color::Gray));
        }

        assert!(colored[0] > colored[1]);
        assert!(colored[1] > colored[2]);
        assert_eq!(colored[2], 1);
        assert_eq!(terminal.backend().buffer()[(29, 2)].fg, Color::Gray);
    }

    #[test]
    fn max_to_low_drains_clockwise_from_low_toward_max() {
        let start = Instant::now();
        let mut selector = EffortSelector::new(ReasoningEffort::Max, false, true);
        let mut terminal = Terminal::new(TestBackend::new(60, 18)).unwrap();
        selector.update(key(KeyCode::Right, start));
        selector.update(EffortEvent::AnimationFrame(start + ANIMATION_DURATION / 2));
        terminal
            .draw(|frame| selector.render(frame, frame.area(), &Theme::default()))
            .unwrap();

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(37, 5)].fg, Color::DarkGray);
        assert_eq!(buffer[(21, 5)].fg, Color::Gray);
    }

    #[test]
    fn low_to_max_is_the_reverse_of_the_forward_wrap() {
        let start = Instant::now();
        let mut selector = EffortSelector::new(ReasoningEffort::Low, false, true);
        let mut terminal = Terminal::new(TestBackend::new(60, 18)).unwrap();
        selector.update(key(KeyCode::Left, start));

        let mut colored = Vec::new();
        for elapsed in [Duration::ZERO, ANIMATION_DURATION / 2, ANIMATION_DURATION] {
            if !elapsed.is_zero() {
                selector.update(EffortEvent::AnimationFrame(start + elapsed));
            }
            terminal
                .draw(|frame| selector.render(frame, frame.area(), &Theme::default()))
                .unwrap();
            colored.push(colored_dial_dots(&terminal, Color::Magenta));
        }

        assert!(colored[0] < colored[1]);
        assert!(colored[1] < colored[2]);
        assert_eq!(selector.dial.displayed_phase, 4.0);
        assert_eq!(selector.dial.displayed_fill, 4.0);
    }

    #[test]
    fn rapid_input_across_low_never_uses_wrapped_phase_as_fill() {
        let start = Instant::now();
        let mut selector = EffortSelector::new(ReasoningEffort::Max, false, true);
        selector.update(key(KeyCode::Right, start));
        selector.update(EffortEvent::AnimationFrame(start + ANIMATION_DURATION / 2));
        selector.update(key(KeyCode::Right, start + ANIMATION_DURATION / 2));

        assert_eq!(selector.selected_effort(), ReasoningEffort::Medium);
        assert!((selector.dial.displayed_fill - 0.5).abs() < f64::EPSILON);
        assert!(selector.dial.target_phase > ReasoningEffort::ALL.len() as f64);

        selector.update(EffortEvent::AnimationFrame(start + ANIMATION_DURATION));
        assert!(selector.dial.displayed_fill <= 1.0);
    }

    #[test]
    fn reversing_a_wrap_continues_from_the_current_fill_keyframe() {
        let start = Instant::now();
        let mut selector = EffortSelector::new(ReasoningEffort::Low, false, true);
        selector.update(key(KeyCode::Left, start));
        selector.update(EffortEvent::AnimationFrame(start + ANIMATION_DURATION / 2));
        assert!((selector.dial.displayed_fill - 3.5).abs() < f64::EPSILON);

        selector.update(key(KeyCode::Right, start + ANIMATION_DURATION / 2));
        selector.update(EffortEvent::AnimationFrame(start + ANIMATION_DURATION));

        assert_eq!(selector.selected_effort(), ReasoningEffort::Low);
        assert!(selector.dial.displayed_fill < 3.5);
    }

    #[test]
    fn enter_applies_and_escape_cancels() {
        let now = Instant::now();
        let mut selector = EffortSelector::new(ReasoningEffort::Medium, false, true);
        selector.update(key(KeyCode::Right, now));

        assert_eq!(
            selector.update(key(KeyCode::Enter, now)).effects,
            [EffortEffect::Apply(ReasoningEffort::High, false)]
        );
        assert_eq!(
            selector.update(key(KeyCode::Esc, now)).effects,
            [EffortEffect::Dismiss]
        );
    }

    #[test]
    fn unavailable_pro_mode_cannot_be_selected() {
        let now = Instant::now();
        let mut selector = EffortSelector::new(ReasoningEffort::Max, true, false);
        selector.update(key(KeyCode::Char('p'), now));
        assert_eq!(
            selector.update(key(KeyCode::Enter, now)).effects,
            [EffortEffect::Apply(ReasoningEffort::Max, false)]
        );
    }

    #[test]
    fn p_toggles_the_local_pro_preference_applied_with_effort() {
        let now = Instant::now();
        let mut selector = EffortSelector::new(ReasoningEffort::High, true, true);

        selector.update(key(KeyCode::Char('p'), now));

        assert_eq!(
            selector.update(key(KeyCode::Enter, now)).effects,
            [EffortEffect::Apply(ReasoningEffort::High, false)]
        );
    }

    #[test]
    fn pro_state_and_toggle_help_are_green() {
        let now = Instant::now();
        let mut selector = EffortSelector::new(ReasoningEffort::Medium, false, true);
        let mut terminal = Terminal::new(TestBackend::new(60, 18)).unwrap();

        terminal
            .draw(|frame| selector.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let label = (6..54)
            .map(|x| buffer[(x, 13)].symbol())
            .collect::<String>();
        assert!(label.contains("Pro: off"));
        let row = &buffer.content[13 * 60..14 * 60];
        let pro_start = row
            .windows(3)
            .position(|cells| {
                cells[0].symbol() == "P" && cells[1].symbol() == "r" && cells[2].symbol() == "o"
            })
            .unwrap();
        assert!((pro_start..pro_start + 3).all(|x| row[x].fg == Color::Green));

        selector.update(key(KeyCode::Char('p'), now));
        terminal
            .draw(|frame| selector.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let label = (6..54)
            .map(|x| buffer[(x, 13)].symbol())
            .collect::<String>();
        assert!(label.contains("Pro: on"));
    }

    #[test]
    fn narrow_terminals_do_not_overflow_the_selector() {
        let mut selector = EffortSelector::new(ReasoningEffort::Medium, false, true);
        let mut terminal = Terminal::new(TestBackend::new(3, 4)).unwrap();

        terminal
            .draw(|frame| selector.render(frame, frame.area(), &Theme::default()))
            .unwrap();

        assert_eq!(terminal.backend().buffer().area.width, 3);
    }
}
