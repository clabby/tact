//! A yes/no confirmation popup.
//!
//! [Confirmation] holds the fixed wording of one question, chosen by its
//! constructor, and no other state. It consumes key presses through
//! [ConfirmationEvent::Terminal]: Enter or `y` emits [ConfirmationEffect::Confirm],
//! Esc or `n` emits [ConfirmationEffect::Dismiss], and every other input is
//! ignored. The host owns the guarded action and closes the popup on either
//! effect.

use super::{
    floating::Floating,
    node::{Component, ComponentUpdate, RenderRequest},
};
use crate::app::theme::Theme;
use crossterm::event::{Event, KeyCode, KeyEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::Line,
    widgets::{Paragraph, Wrap},
};

const WIDTH: u16 = 64;
const HEIGHT: u16 = 9;

pub(super) enum ConfirmationEvent {
    Terminal(Event),
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum ConfirmationEffect {
    Confirm,
    Dismiss,
}

/// Asks before an action whose consequence the user should acknowledge first.
pub(super) struct Confirmation {
    title: &'static str,
    statement: String,
    question: &'static str,
    key_bindings: [(&'static str, &'static str); 2],
}

impl Confirmation {
    pub(super) fn install_web_interface() -> Self {
        Self {
            title: "Install web interface",
            statement: "The browser interface is not installed.".to_owned(),
            question: "Download the matching, checksummed bundle from this Tact release?",
            key_bindings: [("enter/y", "download"), ("esc/n", "cancel")],
        }
    }

    pub(super) fn quit_with_running_sessions(running: usize) -> Self {
        let statement = if running == 1 {
            "A background session is still running.".to_owned()
        } else {
            format!("{running} background sessions are still running.")
        };
        Self {
            title: "Quit Tact",
            statement,
            question: "Quitting interrupts their work. Quit anyway?",
            key_bindings: [("enter/y", "quit"), ("esc/n", "cancel")],
        }
    }
}

impl Component for Confirmation {
    type Event = ConfirmationEvent;
    type Effect = ConfirmationEffect;

    fn update(&mut self, event: Self::Event) -> ComponentUpdate<Self::Effect> {
        let ConfirmationEvent::Terminal(Event::Key(key)) = event else {
            return ComponentUpdate::none();
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return ComponentUpdate::none();
        }
        let effect = match key.code {
            KeyCode::Enter | KeyCode::Char('y' | 'Y') => ConfirmationEffect::Confirm,
            KeyCode::Esc | KeyCode::Char('n' | 'N') => ConfirmationEffect::Dismiss,
            _ => return ComponentUpdate::none(),
        };
        ComponentUpdate {
            effects: vec![effect],
            render: RenderRequest::Immediate,
        }
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let layout =
            Floating::new(self.title, WIDTH, HEIGHT, &self.key_bindings).render(frame, area, theme);
        let lines = vec![
            Line::from(self.statement.as_str()),
            Line::from(""),
            Line::styled(self.question, Style::default().fg(theme.muted())),
        ];
        frame.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }),
            layout.body,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{Component, Confirmation, ConfirmationEffect, ConfirmationEvent, HEIGHT, WIDTH};
    use crate::app::theme::Theme;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend};

    fn text(confirmation: &mut Confirmation) -> String {
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).unwrap();
        terminal
            .draw(|frame| confirmation.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        for y in 1..HEIGHT - 1 {
            assert_eq!(buffer[(WIDTH - 1, y)].symbol(), "│");
        }
        let text = (0..buffer.area.height)
            .map(|y| {
                (1..WIDTH - 1)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join(" ");
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn messages_fit_inside_the_popup() {
        assert!(
            text(&mut Confirmation::install_web_interface())
                .contains("Download the matching, checksummed bundle from this Tact release?")
        );
        let quit = text(&mut Confirmation::quit_with_running_sessions(2));
        assert!(quit.contains("2 background sessions are still running."));
        assert!(quit.contains("Quit anyway?"));
    }

    #[test]
    fn yes_confirms_and_escape_dismisses() {
        let mut confirmation = Confirmation::install_web_interface();
        let key =
            |code| ConfirmationEvent::Terminal(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
        assert_eq!(
            confirmation.update(key(KeyCode::Char('y'))).effects,
            [ConfirmationEffect::Confirm]
        );
        assert_eq!(
            confirmation.update(key(KeyCode::Esc)).effects,
            [ConfirmationEffect::Dismiss]
        );
        assert!(
            confirmation
                .update(key(KeyCode::Char('x')))
                .effects
                .is_empty()
        );
    }
}
