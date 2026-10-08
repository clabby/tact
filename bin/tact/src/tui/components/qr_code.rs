//! A QR code that signs a phone in to the web interface.

use super::{
    floating::Floating,
    node::{Component, ComponentUpdate, RenderRequest},
};
use crate::{app::theme::Theme, tui::spinner::Spinner};
use crossterm::event::{Event, KeyCode, KeyEventKind};
use qrcode::{Color as Module, EcLevel, QrCode};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};
use std::time::Instant;

const FOOTER: [(&str, &str); 1] = [("esc", "close")];
/// Blank modules around the code that scanners need to find its edges.
const QUIET_ZONE: usize = 2;
const CAPTION: &str = "Scan with your phone's camera. It signs the phone in to Tact, so treat the code like a password.";
const MIN_WIDTH: u16 = 46;
/// Body rows of the waiting popup: the message sits in the middle one.
const PREPARING_ROWS: u16 = 3;
/// Border, title, and footer rows that surround the popup's body.
const CHROME_ROWS: u16 = 3;
/// Columns the popup's border takes from its width.
const BORDER_COLUMNS: u16 = 2;

pub(super) enum QrCodeEvent {
    Terminal(Event),
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum QrCodeEffect {
    Dismiss,
}

/// What the QR overlay shows: a wait while the link is prepared, then the code for it.
///
/// Preparing a link can take seconds (Tailscale may have to publish the server first), so the
/// overlay opens at once with a spinner and the code replaces it when the link is ready.
pub(super) enum QrCodeOverlay {
    Preparing(Spinner),
    Ready(QrCodeView),
}

impl QrCodeOverlay {
    pub(super) fn preparing(now: Instant) -> Self {
        Self::Preparing(Spinner::new(now))
    }

    pub(super) fn ready(link: &str) -> Result<Self, String> {
        QrCodeView::new(link).map(Self::Ready)
    }

    pub(super) const fn is_preparing(&self) -> bool {
        matches!(self, Self::Preparing(_))
    }

    /// Advances the spinner; returns whether the frame changed.
    pub(super) fn advance(&mut self, now: Instant) -> bool {
        match self {
            Self::Preparing(spinner) => spinner.advance(now),
            Self::Ready(_) => false,
        }
    }

    pub(super) fn animation_deadline(&self) -> Option<Instant> {
        match self {
            Self::Preparing(spinner) => Some(spinner.deadline()),
            Self::Ready(_) => None,
        }
    }
}

impl Component for QrCodeOverlay {
    type Event = QrCodeEvent;
    type Effect = QrCodeEffect;

    fn update(&mut self, event: Self::Event) -> ComponentUpdate<Self::Effect> {
        match self {
            Self::Preparing(_) => dismiss_on_escape(event),
            Self::Ready(view) => view.update(event),
        }
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        match self {
            Self::Ready(view) => view.render(frame, area, theme),
            Self::Preparing(spinner) => {
                let layout = Floating::new(
                    "Open on your phone",
                    MIN_WIDTH,
                    CHROME_ROWS + PREPARING_ROWS,
                    &FOOTER,
                )
                .render(frame, area, theme);
                let line = Line::from(vec![
                    Span::styled(spinner.symbol(), Style::default().fg(theme.accent())),
                    Span::styled(
                        " Preparing the sign-in link…",
                        Style::default().fg(theme.muted()),
                    ),
                ]);
                let [_, middle, _] = Layout::vertical([
                    Constraint::Fill(1),
                    Constraint::Length(1),
                    Constraint::Fill(1),
                ])
                .areas(layout.body);
                frame.render_widget(Paragraph::new(line.centered()), middle);
            }
        }
    }
}

fn dismiss_on_escape(event: QrCodeEvent) -> ComponentUpdate<QrCodeEffect> {
    match event {
        QrCodeEvent::Terminal(Event::Key(key))
            if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
                && key.code == KeyCode::Esc =>
        {
            ComponentUpdate {
                effects: vec![QrCodeEffect::Dismiss],
                render: RenderRequest::Immediate,
            }
        }
        QrCodeEvent::Terminal(_) => ComponentUpdate::none(),
    }
}

/// The code for one sign-in link. The link carries the credential, so only its origin is ever shown
/// as text.
pub(super) struct QrCodeView {
    /// Dark modules, row by row, including the quiet zone.
    modules: Vec<Vec<bool>>,
    origin: String,
}

impl QrCodeView {
    pub(super) fn new(link: &str) -> Result<Self, String> {
        let code = QrCode::with_error_correction_level(link.as_bytes(), EcLevel::L)
            .map_err(|error| format!("Could not build the QR code: {error}"))?;
        let width = code.width();
        let colors = code.to_colors();
        let side = width + 2 * QUIET_ZONE;
        let mut modules = vec![vec![false; side]; side];
        for (index, color) in colors.into_iter().enumerate() {
            modules[QUIET_ZONE + index / width][QUIET_ZONE + index % width] = color == Module::Dark;
        }
        let origin = link
            .split_once("/#")
            .map_or(link, |(origin, _)| origin)
            .to_owned();
        Ok(Self { modules, origin })
    }

    fn columns(&self) -> u16 {
        u16::try_from(self.modules.len()).unwrap_or(u16::MAX)
    }

    /// Terminal rows for the code: each row shows two module rows with a half block.
    fn rows(&self) -> u16 {
        self.columns().div_ceil(2)
    }

    /// The origin and the explainer, wrapped to `width` columns so none of it is cut off.
    fn caption(&self, width: u16, theme: &Theme) -> Vec<Line<'static>> {
        let width = usize::from(width);
        let origin = wrap_words(&self.origin, width)
            .into_iter()
            .map(|row| Line::styled(row, Style::default().fg(theme.accent())).centered());
        let explainer = wrap_words(CAPTION, width)
            .into_iter()
            .map(|row| Line::styled(row, Style::default().fg(theme.muted())).centered());
        origin.chain(explainer).collect()
    }

    /// Black and white are fixed, whatever the theme: scanners expect dark modules on light.
    fn lines(&self, width: u16) -> Vec<Line<'static>> {
        let padding = usize::from(width.saturating_sub(self.columns()) / 2);
        let color = |dark: bool| {
            if dark {
                Color::Rgb(0, 0, 0)
            } else {
                Color::Rgb(255, 255, 255)
            }
        };
        (0..usize::from(self.rows()))
            .map(|row| {
                let mut spans = vec![Span::raw(" ".repeat(padding))];
                for column in 0..self.modules.len() {
                    let top = self.modules[row * 2][column];
                    let bottom = self
                        .modules
                        .get(row * 2 + 1)
                        .is_some_and(|line| line[column]);
                    spans.push(Span::styled(
                        "\u{2580}",
                        Style::default().fg(color(top)).bg(color(bottom)),
                    ));
                }
                Line::from(spans)
            })
            .collect()
    }
}

impl Component for QrCodeView {
    type Event = QrCodeEvent;
    type Effect = QrCodeEffect;

    fn update(&mut self, event: Self::Event) -> ComponentUpdate<Self::Effect> {
        dismiss_on_escape(event)
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let width = (self.columns() + 4).max(MIN_WIDTH);
        let body_width = width - BORDER_COLUMNS;
        let caption = self.caption(body_width, theme);
        let caption_rows = u16::try_from(caption.len()).unwrap_or(u16::MAX);
        let height = self.rows() + caption_rows + CHROME_ROWS;
        let layout =
            Floating::new("Open on your phone", width, height, &FOOTER).render(frame, area, theme);
        let body = layout.body;
        if body.is_empty() {
            return;
        }
        if body.width < body_width || body.height < self.rows() + caption_rows {
            let message = format!(
                "Enlarge the terminal to show the code ({}x{} needed).",
                width + 2,
                height + 2
            );
            frame.render_widget(
                Paragraph::new(message)
                    .style(Style::default().fg(theme.muted()))
                    .wrap(Wrap { trim: true }),
                body,
            );
            return;
        }
        let [code, caption_area] =
            Layout::vertical([Constraint::Length(self.rows()), Constraint::Min(0)]).areas(body);
        frame.render_widget(Paragraph::new(self.lines(code.width)), code);
        frame.render_widget(Paragraph::new(caption), caption_area);
    }
}

/// Greedy word wrap that splits any word wider than a row.
fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let mut rows = Vec::new();
    let mut row = String::new();
    for word in text.split_whitespace() {
        let mut word = word;
        while !row.is_empty() && row.chars().count() + 1 + word.chars().count() > width {
            rows.push(std::mem::take(&mut row));
        }
        while word.chars().count() > width {
            let split = word
                .char_indices()
                .nth(width)
                .map_or(word.len(), |(index, _)| index);
            rows.push(word[..split].to_owned());
            word = &word[split..];
        }
        if !row.is_empty() {
            row.push(' ');
        }
        row.push_str(word);
    }
    if !row.is_empty() {
        rows.push(row);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::{Component, QrCodeEffect, QrCodeEvent, QrCodeView};
    use crate::app::theme::Theme;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend, style::Color};

    const LINK: &str = "https://laptop.tail1234.ts.net/#k=secret-token-value";

    fn render(view: &mut QrCodeView, width: u16, height: u16) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| view.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        terminal
    }

    fn text(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn the_code_has_finder_patterns_inside_a_quiet_zone() {
        let view = QrCodeView::new(LINK).unwrap();
        let side = view.modules.len();
        assert!(
            view.modules[..2]
                .iter()
                .all(|row| row.iter().all(|dark| !dark))
        );
        assert!(
            view.modules[2][2..9].iter().all(|dark| *dark),
            "top-left finder"
        );
        assert!(
            view.modules[2][side - 9..side - 2].iter().all(|dark| *dark),
            "top-right finder"
        );
        assert!(
            view.modules[side - 9][2..9].iter().all(|dark| *dark),
            "bottom-left finder"
        );
    }

    #[test]
    fn modules_are_drawn_black_on_white_and_the_credential_is_never_shown() {
        let mut view = QrCodeView::new(LINK).unwrap();
        let terminal = render(&mut view, 90, 40);

        let buffer = terminal.backend().buffer();
        let code = buffer
            .content()
            .iter()
            .filter(|cell| cell.symbol() == "\u{2580}")
            .collect::<Vec<_>>();
        assert!(!code.is_empty());
        assert!(code.iter().all(|cell| {
            [Color::Rgb(0, 0, 0), Color::Rgb(255, 255, 255)].contains(&cell.fg)
                && [Color::Rgb(0, 0, 0), Color::Rgb(255, 255, 255)].contains(&cell.bg)
        }));
        let rendered = text(&terminal);
        assert!(rendered.contains("https://laptop.tail1234.ts.net"));
        assert!(!rendered.contains("secret-token-value"));
        assert!(rendered.contains("esc close"));
    }

    #[test]
    fn the_explainer_is_shown_in_full() {
        let mut view = QrCodeView::new(LINK).unwrap();
        let terminal = render(&mut view, 90, 40);

        let buffer = terminal.backend().buffer();
        let caption = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .filter(|row| !row.contains('\u{2580}'))
            .map(|row| row.replace('\u{2502}', " ").trim().to_owned())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            caption.contains("Scan with your phone's camera."),
            "{caption}"
        );
        assert!(
            caption.contains("treat the code like a password."),
            "{caption}"
        );
    }

    #[test]
    fn a_terminal_that_is_too_small_explains_what_is_needed() {
        let mut view = QrCodeView::new(LINK).unwrap();
        let terminal = render(&mut view, 50, 14);

        let rendered = text(&terminal);
        assert!(rendered.contains("Enlarge the terminal"));
        assert!(!rendered.contains("\u{2580}"));
    }

    #[test]
    fn escape_dismisses_the_popup() {
        let mut view = QrCodeView::new(LINK).unwrap();
        let update = view.update(QrCodeEvent::Terminal(Event::Key(KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE,
        ))));
        assert_eq!(update.effects, [QrCodeEffect::Dismiss]);
    }

    #[test]
    fn the_waiting_overlay_can_be_dismissed_and_draws_the_spinner() {
        let mut overlay = super::QrCodeOverlay::preparing(std::time::Instant::now());
        let mut terminal = Terminal::new(TestBackend::new(90, 20)).unwrap();
        terminal
            .draw(|frame| overlay.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        assert!(text(&terminal).contains('\u{280b}'), "first spinner frame");

        // The message is centred in the popup both ways.
        let buffer = terminal.backend().buffer();
        let width = usize::from(buffer.area.width);
        let rows = buffer
            .content()
            .chunks(width)
            .enumerate()
            .filter(|(_, row)| row.iter().any(|cell| cell.symbol() == "\u{280b}"))
            .collect::<Vec<_>>();
        let [(message_row, row)] = rows.as_slice() else {
            panic!("the spinner is drawn once");
        };
        let first = row
            .iter()
            .position(|cell| cell.symbol() == "\u{280b}")
            .unwrap();
        let last = row
            .iter()
            .rposition(|cell| !cell.symbol().trim().is_empty() && cell.symbol() != "\u{2502}")
            .unwrap();
        let popup = (0..width)
            .filter(|column| row[*column].symbol() == "\u{2502}")
            .collect::<Vec<_>>();
        let (left, right) = (popup[0], popup[popup.len() - 1]);
        assert!(
            (first - left).abs_diff(right - last) <= 1,
            "message spans {first}..={last} in a popup spanning {left}..={right}"
        );
        let borders = buffer
            .content()
            .chunks(width)
            .enumerate()
            .filter(|(_, row)| row[left].symbol() == "\u{2502}")
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        let (top, bottom) = (borders[0], borders[borders.len() - 1]);
        assert!(
            (message_row - top).abs_diff(bottom - message_row) <= 2,
            "message row {message_row} in a popup spanning rows {top}..={bottom}"
        );

        let update = overlay.update(QrCodeEvent::Terminal(Event::Key(KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE,
        ))));
        assert_eq!(update.effects, [QrCodeEffect::Dismiss]);
    }
}
