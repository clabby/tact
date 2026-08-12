//! Demand-driven empty transcript animation.

use crate::tui::theme::Theme;
use nanocodex::Model;
use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Modifier, Style},
};
use std::{
    f64::consts::TAU,
    time::{Duration, Instant},
};
use unicode_width::UnicodeWidthStr;

const FRAME_INTERVAL: Duration = Duration::from_millis(120);
const FRAME_COUNT: usize = 120;
const MAX_WIDTH: u16 = 35;
const MAX_HEIGHT: u16 = 17;
const HORIZONTAL_MARGIN: u16 = 2;
const VERTICAL_MARGIN: u16 = 1;
const BODY_RADIUS_X: f64 = 13.0;
const BODY_RADIUS_Y: f64 = 6.0;
const SHADING_RAMP: [&str; 9] = ["·", ":", "-", "=", "+", "*", "#", "%", "@"];
const WORDMARK: &str = "𝒕𝒂𝒄𝒕";

#[derive(Clone, Copy)]
struct CelestialCell {
    symbol: &'static str,
    intensity: f64,
}

pub(super) struct EmptyLogo {
    started_at: Instant,
    next_frame: Instant,
    frame: usize,
}

impl EmptyLogo {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            started_at: now,
            next_frame: now + FRAME_INTERVAL,
            frame: 0,
        }
    }

    pub(super) const fn deadline(&self) -> Instant {
        self.next_frame
    }

    pub(super) fn advance(&mut self, now: Instant) -> bool {
        if now < self.next_frame {
            return false;
        }

        let elapsed = now.saturating_duration_since(self.started_at).as_millis();
        let frame = usize::try_from(elapsed / FRAME_INTERVAL.as_millis()).unwrap_or(usize::MAX)
            % FRAME_COUNT;
        self.next_frame = now + FRAME_INTERVAL;
        if frame == self.frame {
            return false;
        }
        self.frame = frame;
        true
    }

    pub(super) fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme, model: Model) {
        let Some(canvas) = canvas(area) else {
            return;
        };
        let phase = TAU * self.frame as f64 / FRAME_COUNT as f64;
        let (radius_x, radius_y) = radii(canvas);
        let center_x = f64::from(canvas.width.saturating_sub(1)) / 2.0;
        let center_y = f64::from(canvas.height.saturating_sub(1)) / 2.0;

        for row in 0..canvas.height {
            for column in 0..canvas.width {
                let x = (f64::from(column) - center_x) / radius_x;
                let y = (f64::from(row) - center_y) / radius_y;
                let cell = match model {
                    Model::Luna => moon(x, y, phase),
                    Model::Terra => earth(x, y, phase),
                    Model::Sol => sun(x, y, phase),
                    _ => sun(x, y, phase),
                };
                let Some(cell) = cell else {
                    continue;
                };
                frame.buffer_mut()[Position::new(canvas.x + column, canvas.y + row)]
                    .set_symbol(cell.symbol)
                    .set_style(celestial_style(cell.intensity, theme, model));
            }
        }
        render_wordmark(frame, canvas, theme, model);
    }
}

fn moon(x: f64, y: f64, phase: f64) -> Option<CelestialCell> {
    let distance = x.hypot(y);
    if distance > 1.0 {
        return orbital_glint(x, y, phase);
    }

    let craters = [
        crater(x, y, -0.43, -0.27, 0.18),
        crater(x, y, 0.38, 0.34, 0.23),
        crater(x, y, 0.24, -0.42, 0.11),
        crater(x, y, -0.15, 0.48, 0.13),
    ]
    .into_iter()
    .fold(0.0_f64, f64::max);
    let terminator = ((x + 0.72) / 1.25).clamp(0.0, 1.0);
    let shimmer = (x * 8.0 + y * 5.0 + phase * 0.7).sin() * 0.08;
    let edge = ((1.0 - distance) * 3.5).clamp(0.0, 1.0);
    let intensity =
        (0.2 + terminator * 0.65 + edge * 0.15 + shimmer - craters * 0.34).clamp(0.05, 1.0);
    Some(CelestialCell {
        symbol: density_symbol(intensity),
        intensity,
    })
}

fn crater(x: f64, y: f64, center_x: f64, center_y: f64, radius: f64) -> f64 {
    let distance = (x - center_x).hypot(y - center_y) / radius;
    if distance >= 1.0 {
        0.0
    } else {
        (1.0 - distance).powi(2)
    }
}

fn orbital_glint(x: f64, y: f64, phase: f64) -> Option<CelestialCell> {
    let glint_x = phase.cos() * 1.18;
    let glint_y = phase.sin() * 1.08;
    ((x - glint_x).hypot(y - glint_y) < 0.08).then_some(CelestialCell {
        symbol: "✦",
        intensity: 1.0,
    })
}

fn earth(x: f64, y: f64, phase: f64) -> Option<CelestialCell> {
    let distance = x.hypot(y);
    if distance > 1.0 {
        return (distance < 1.07 && ((x * 13.0 + y * 7.0 + phase).sin() > 0.85)).then_some(
            CelestialCell {
                symbol: "·",
                intensity: 0.45,
            },
        );
    }

    let longitude = x.atan2((1.0 - x * x).max(0.0).sqrt()) + phase * 0.72;
    let latitude = y.asin();
    let land = (longitude * 2.1).sin()
        + (longitude * 3.7 - latitude * 4.2).cos() * 0.72
        + (longitude * 5.3 + latitude * 2.6).sin() * 0.36
        + latitude.sin() * 0.28;
    let edge = ((1.0 - distance) * 4.0).clamp(0.0, 1.0);
    let light = (0.66 + x * 0.2 - y * 0.08).clamp(0.25, 0.95) * (0.55 + edge * 0.45);
    let intensity = if land > 0.35 {
        light.max(0.6)
    } else {
        light * 0.48
    };
    Some(CelestialCell {
        symbol: density_symbol(intensity),
        intensity,
    })
}

fn sun(x: f64, y: f64, phase: f64) -> Option<CelestialCell> {
    let distance = x.hypot(y);
    if distance > 1.0 {
        return corona(x, y, distance, phase);
    }

    let turbulence = (x * 9.0 + phase * 1.4).sin()
        + (y * 8.0 - phase * 1.9).cos()
        + ((x - phase.cos() * 0.28).hypot(y - phase.sin() * 0.2) * 13.0 - phase).sin();
    let edge = ((1.0 - distance) * 4.5).clamp(0.0, 1.0);
    let intensity = (0.68 + turbulence * 0.09 + edge * 0.2).clamp(0.3, 1.0);
    Some(CelestialCell {
        symbol: density_symbol(intensity),
        intensity,
    })
}

fn corona(x: f64, y: f64, distance: f64, phase: f64) -> Option<CelestialCell> {
    if distance > 1.3 {
        return None;
    }
    let angle = y.atan2(x);
    let billow = (angle * 3.0 + phase * 0.7).sin() * 0.06
        + (angle * 7.0 - phase * 1.1).sin() * 0.035
        + (angle * 11.0 + phase * 0.45).cos() * 0.02;
    let reach = 1.14 + billow;
    if distance > reach {
        return None;
    }
    let depth = ((reach - distance) / (reach - 1.0)).clamp(0.0, 1.0);
    let shimmer = (angle * 13.0 - phase * 1.3 + distance * 9.0).sin() * 0.06;
    let intensity = (0.16 + depth * 0.48 + shimmer).clamp(0.12, 0.68);
    Some(CelestialCell {
        symbol: density_symbol(intensity),
        intensity,
    })
}

fn density_symbol(intensity: f64) -> &'static str {
    let level = (intensity * (SHADING_RAMP.len() - 1) as f64)
        .round()
        .clamp(0.0, (SHADING_RAMP.len() - 1) as f64) as usize;
    SHADING_RAMP[level]
}

fn celestial_style(intensity: f64, theme: &Theme, model: Model) -> Style {
    let style = Style::default().fg(theme.model(model));
    if intensity < 0.4 {
        style.add_modifier(Modifier::DIM)
    } else if intensity > 0.78 {
        style.add_modifier(Modifier::BOLD)
    } else {
        style
    }
}

fn render_wordmark(frame: &mut Frame<'_>, canvas: Rect, theme: &Theme, model: Model) {
    let width = u16::try_from(WORDMARK.width()).unwrap_or(u16::MAX);
    if width > canvas.width {
        return;
    }
    let x = canvas.x + canvas.width.saturating_sub(width) / 2;
    let y = canvas.y + canvas.height / 2;
    frame.buffer_mut().set_string(
        x,
        y,
        WORDMARK,
        Style::reset()
            .fg(theme.model(model))
            .add_modifier(Modifier::BOLD),
    );
}

fn radii(canvas: Rect) -> (f64, f64) {
    let available_x = f64::from(canvas.width.saturating_sub(1)) / 2.0;
    let available_y = f64::from(canvas.height.saturating_sub(1)) / 2.0;
    let scale = (available_x / (BODY_RADIUS_X * 1.4))
        .min(available_y / (BODY_RADIUS_Y * 1.4))
        .min(1.0);
    (
        (BODY_RADIUS_X * scale).max(0.5),
        (BODY_RADIUS_Y * scale).max(0.5),
    )
}

fn canvas(area: Rect) -> Option<Rect> {
    if area.is_empty() {
        return None;
    }
    let margin_x = HORIZONTAL_MARGIN.min(area.width.saturating_sub(1));
    let margin_y = VERTICAL_MARGIN.min(area.height.saturating_sub(1));
    let width = MAX_WIDTH.min(area.width.saturating_sub(margin_x)).max(1);
    let height = MAX_HEIGHT.min(area.height.saturating_sub(margin_y)).max(1);
    Some(Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    ))
}

#[cfg(test)]
mod tests {
    use super::{EmptyLogo, FRAME_INTERVAL, WORDMARK, canvas};
    use crate::tui::theme::Theme;
    use nanocodex::Model;
    use ratatui::{Terminal, backend::TestBackend, layout::Rect, style::Color};
    use std::{collections::HashSet, time::Instant};

    fn render(logo: &EmptyLogo, model: Model, width: u16, height: u16) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| logo.render(frame, frame.area(), &Theme::default(), model))
            .unwrap();
        terminal
    }

    fn symbols(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn every_model_renders_distinct_round_artwork_in_its_color() {
        let logo = EmptyLogo::new(Instant::now());
        let expected = [
            (Model::Luna, Color::White),
            (Model::Terra, Color::Green),
            (Model::Sol, Color::Yellow),
        ];
        let mut renderings = HashSet::new();

        for (model, color) in expected {
            let terminal = render(&logo, model, 80, 24);
            let buffer = terminal.backend().buffer();
            let occupied = buffer
                .content()
                .iter()
                .filter(|cell| cell.symbol() != " ")
                .collect::<Vec<_>>();

            assert!(!occupied.is_empty());
            assert!(occupied.iter().all(|cell| cell.fg == color));
            assert!(occupied.iter().all(|cell| !matches!(
                cell.symbol(),
                "░" | "▒" | "▓" | "█" | "─" | "╱" | "│" | "╲"
            )));
            renderings.insert(symbols(&terminal));
        }

        assert_eq!(renderings.len(), 3);
    }

    #[test]
    fn celestial_body_is_centered_and_taller_than_the_old_banner() {
        let logo = EmptyLogo::new(Instant::now());
        let terminal = render(&logo, Model::Terra, 80, 24);
        let canvas = canvas(Rect::new(0, 0, 80, 24)).unwrap();
        let buffer = terminal.backend().buffer();
        let occupied_rows = (canvas.y..canvas.bottom())
            .filter(|&y| (canvas.x..canvas.right()).any(|x| buffer[(x, y)].symbol() != " "))
            .collect::<Vec<_>>();

        assert_eq!(canvas, Rect::new(22, 3, 35, 17));
        assert!(occupied_rows.len() >= 11);
        assert_eq!(
            occupied_rows.first().unwrap() + occupied_rows.last().unwrap(),
            2 * (canvas.y + canvas.height / 2)
        );

        let narrow = render(&logo, Model::Luna, 1, 1);
        assert_ne!(symbols(&narrow), " ");
    }

    #[test]
    fn wordmark_is_centered_in_the_model_color() {
        let logo = EmptyLogo::new(Instant::now());
        let terminal = render(&logo, Model::Terra, 80, 24);
        let canvas = canvas(Rect::new(0, 0, 80, 24)).unwrap();
        let x = canvas.x + (canvas.width - 4) / 2;
        let y = canvas.y + canvas.height / 2;
        let buffer = terminal.backend().buffer();
        let rendered = (x..x + 4)
            .map(|column| buffer[(column, y)].symbol())
            .collect::<String>();

        assert_eq!(rendered, WORDMARK);
        for column in x..x + 4 {
            assert_eq!(buffer[(column, y)].fg, Color::Green);
        }
    }

    #[test]
    fn animation_advances_only_after_its_deadline() {
        let start = Instant::now();
        let mut logo = EmptyLogo::new(start);
        let first = symbols(&render(&logo, Model::Sol, 60, 18));

        assert!(!logo.advance(start + FRAME_INTERVAL / 2));
        assert!(logo.advance(start + FRAME_INTERVAL));
        assert_ne!(symbols(&render(&logo, Model::Sol, 60, 18)), first);
        assert!(logo.deadline() > start + FRAME_INTERVAL);
    }
}
