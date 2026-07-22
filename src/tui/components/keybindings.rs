//! Styled global keyboard shortcut reference.

use super::{
    floating::Floating,
    node::{Component, ComponentUpdate, RenderRequest},
};
use crate::tui::theme::Theme;
use crossterm::event::{Event, KeyCode, KeyEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

const FOOTER: [&str; 1] = ["esc close"];
const BINDINGS: [(&str, &str); 17] = [
    ("ctrl+s", "change reasoning effort"),
    ("ctrl+f", "fork session · when available"),
    ("ctrl+g", "edit prompt in $EDITOR"),
    ("ctrl/cmd+v", "paste clipboard image"),
    ("ctrl+o", "expand · collapse all tool calls"),
    (
        "ctrl+c",
        "clear focused draft · split closes pane · else exit",
    ),
    ("esc esc", "interrupt the active response"),
    ("enter", "submit prompt"),
    ("shift+enter/ctrl+j", "insert newline"),
    ("↑/↓", "move cursor · prompt history at edge"),
    ("tab", "focus queue · when present"),
    ("/", "open actions · empty prompt only"),
    ("@", "insert workspace file"),
    ("!", "local shell command · prompt start"),
    ("mouse click/drag", "open links/tools · copy text"),
    ("pgup/pgdn · wheel", "scroll transcript"),
    ("ctrl+home/end", "jump to start · follow latest"),
];

pub(super) enum KeybindingsEvent {
    Terminal(Event),
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum KeybindingsEffect {
    Dismiss,
}

pub(super) struct KeybindingsHelp;

impl Component for KeybindingsHelp {
    type Event = KeybindingsEvent;
    type Effect = KeybindingsEffect;

    fn update(&mut self, event: Self::Event) -> ComponentUpdate<Self::Effect> {
        let KeybindingsEvent::Terminal(Event::Key(key)) = event else {
            return ComponentUpdate::none();
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
            || key.code != KeyCode::Esc
        {
            return ComponentUpdate::none();
        }
        ComponentUpdate {
            effects: vec![KeybindingsEffect::Dismiss],
            render: RenderRequest::Immediate,
        }
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let layout =
            Floating::new("Keyboard shortcuts", 72, 21, &FOOTER).render(frame, area, theme);
        if layout.body.is_empty() {
            return;
        }
        let lines = BINDINGS
            .iter()
            .map(|&(key, description)| {
                Line::from(vec![
                    Span::styled(
                        format!(" {key:<18}"),
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(description, Style::default().fg(theme.muted())),
                ])
            })
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(lines), layout.body);
    }
}

#[cfg(test)]
mod tests {
    use super::{Component, KeybindingsEffect, KeybindingsEvent, KeybindingsHelp};
    use crate::tui::theme::Theme;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend, style::Color};

    #[test]
    fn popup_centers_green_keys_with_muted_descriptions() {
        let mut help = KeybindingsHelp;
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();

        terminal
            .draw(|frame| help.render(frame, frame.area(), &Theme::default()))
            .unwrap();

        let buffer = terminal.backend().buffer();
        let row = buffer
            .content()
            .chunks(80)
            .position(|cells| {
                cells
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>()
                    .contains("ctrl+s")
            })
            .expect("effort shortcut should render");
        assert!(buffer.content().chunks(80).any(|cells| {
            cells
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
                .contains("ctrl+o")
        }));
        assert_eq!(buffer[(5, u16::try_from(row).unwrap())].fg, Color::Green);
        assert_eq!(
            buffer[(24, u16::try_from(row).unwrap())].fg,
            Color::DarkGray
        );
    }

    #[test]
    fn popup_documents_context_sensitive_composer_shortcuts() {
        let mut help = KeybindingsHelp;
        let mut terminal = Terminal::new(TestBackend::new(80, 22)).unwrap();

        terminal
            .draw(|frame| help.render(frame, frame.area(), &Theme::default()))
            .unwrap();

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .chunks(80)
            .map(|cells| cells.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>();
        for expected in [
            "ctrl/cmd+v",
            "clear focused draft · split closes pane · else exit",
            "shift+enter/ctrl+j",
            "prompt history at edge",
            "focus queue · when present",
            "open actions · empty prompt only",
            "insert workspace file",
            "local shell command · prompt start",
            "mouse click/drag",
        ] {
            assert!(rendered.iter().any(|line| line.contains(expected)));
        }
    }

    #[test]
    fn narrow_terminals_do_not_overflow_the_popup() {
        let mut help = KeybindingsHelp;
        let mut terminal = Terminal::new(TestBackend::new(8, 4)).unwrap();

        terminal
            .draw(|frame| help.render(frame, frame.area(), &Theme::default()))
            .unwrap();

        assert_eq!(terminal.backend().buffer().area.width, 8);
    }

    #[test]
    fn escape_dismisses_the_popup() {
        let mut help = KeybindingsHelp;

        let update = help.update(KeybindingsEvent::Terminal(Event::Key(KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE,
        ))));

        assert_eq!(update.effects, [KeybindingsEffect::Dismiss]);
    }
}
