//! Shared animation and geometry for circular selectors.

use crate::app::theme::Theme;
use ratatui::{
    Frame,
    buffer::Buffer,
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
};
use std::{
    f64::consts::{FRAC_PI_2, TAU},
    time::{Duration, Instant},
};

pub(super) const ANIMATION_DURATION: Duration = Duration::from_millis(420);
pub(super) const ANIMATION_FRAME_INTERVAL: Duration = Duration::from_millis(16);

// Terminal cells are roughly twice as tall as they are wide, so a 2:1 cell
// ratio produces a visually circular dial.
const DIAL_WIDTH: u16 = 17;
const DIAL_HEIGHT: u16 = 9;
const DIAL_SAMPLES: usize = 96;
const FILLER_DOT: &str = "•";
const THICK_DOT: &str = "●";

pub(super) struct AnimatedDial {
    selected: usize,
    stops: usize,
    pub(super) displayed_phase: f64,
    pub(super) displayed_fill: f64,
    pub(super) target_phase: f64,
    animation: Option<Animation>,
}

struct Animation {
    phase_from: f64,
    phase_to: f64,
    fill_from: f64,
    fill_to: f64,
    wrapping_fill: bool,
    started_at: Instant,
    next_frame: Instant,
}

impl AnimatedDial {
    pub(super) fn new(selected: usize, stops: usize) -> Self {
        let phase = selected as f64;
        Self {
            selected,
            stops,
            displayed_phase: phase,
            displayed_fill: phase,
            target_phase: phase,
            animation: None,
        }
    }

    pub(super) const fn selected(&self) -> usize {
        self.selected
    }

    pub(super) fn animation_deadline(&self) -> Option<Instant> {
        self.animation
            .as_ref()
            .map(|animation| animation.next_frame)
    }

    pub(super) fn select_relative(&mut self, direction: isize, now: Instant) {
        self.advance_animation(now);
        let previous = self.selected;
        if direction < 0 {
            self.selected = if self.selected == 0 {
                self.stops - 1
            } else {
                self.selected - 1
            };
        } else {
            self.selected = (self.selected + 1) % self.stops;
        }
        self.target_phase += direction as f64;
        let wrapping_fill = (previous == self.stops - 1 && self.selected == 0)
            || (previous == 0 && self.selected == self.stops - 1);
        self.animation = Some(Animation {
            phase_from: self.displayed_phase,
            phase_to: self.target_phase,
            fill_from: self.displayed_fill,
            fill_to: self.selected as f64,
            wrapping_fill,
            started_at: now,
            next_frame: now + ANIMATION_FRAME_INTERVAL,
        });
    }

    pub(super) fn advance_animation(&mut self, now: Instant) -> bool {
        let Some(animation) = &mut self.animation else {
            return false;
        };
        let elapsed = now.saturating_duration_since(animation.started_at);
        let progress = (elapsed.as_secs_f64() / ANIMATION_DURATION.as_secs_f64()).min(1.0);
        let eased = 1.0 - (1.0 - progress).powi(3);
        self.displayed_phase =
            animation.phase_from + (animation.phase_to - animation.phase_from) * eased;
        self.displayed_fill =
            animation.fill_from + (animation.fill_to - animation.fill_from) * eased;

        if progress >= 1.0 {
            let phase = self.selected as f64;
            self.displayed_phase = phase;
            self.displayed_fill = phase;
            self.target_phase = phase;
            self.animation = None;
        } else {
            animation.next_frame = now + ANIMATION_FRAME_INTERVAL;
        }
        true
    }

    fn render_dial(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme, selected_color: Color) {
        if area.is_empty() {
            return;
        }

        let center_x = f64::from(area.width.saturating_sub(1)) / 2.0;
        let center_y = f64::from(area.height.saturating_sub(1)) / 2.0;
        let radius_x = center_x;
        let radius_y = center_y;
        let indicator = dial_position(
            self.stops,
            area,
            center_x,
            center_y,
            radius_x,
            radius_y,
            self.displayed_phase,
        );
        let filled_phase = self.displayed_fill.clamp(0.0, (self.stops - 1) as f64);
        let wrapping_fill = self
            .animation
            .as_ref()
            .is_some_and(|animation| animation.wrapping_fill);
        let buffer = frame.buffer_mut();
        for sample in 0..DIAL_SAMPLES {
            let phase = sample as f64 / DIAL_SAMPLES as f64 * self.stops as f64;
            let point = dial_position(
                self.stops, area, center_x, center_y, radius_x, radius_y, phase,
            );
            draw_dot(
                buffer,
                point,
                FILLER_DOT,
                Style::default().fg(theme.muted()),
            );
        }
        for sample in 0..DIAL_SAMPLES {
            let phase = sample as f64 / DIAL_SAMPLES as f64 * self.stops as f64;
            if !phase_is_filled(self.stops, phase, filled_phase, wrapping_fill) {
                continue;
            }
            let point = dial_position(
                self.stops, area, center_x, center_y, radius_x, radius_y, phase,
            );
            draw_dot(
                buffer,
                point,
                FILLER_DOT,
                Style::default().fg(selected_color),
            );
        }

        for index in 0..self.stops {
            let point = dial_position(
                self.stops,
                area,
                center_x,
                center_y,
                radius_x,
                radius_y,
                index as f64,
            );
            let color = if phase_is_filled(self.stops, index as f64, filled_phase, wrapping_fill) {
                selected_color
            } else {
                theme.muted()
            };
            draw_dot(buffer, point, THICK_DOT, Style::default().fg(color));
        }

        draw_dot(
            buffer,
            indicator,
            THICK_DOT,
            Style::default()
                .fg(
                    if phase_is_filled(
                        self.stops,
                        self.displayed_phase.rem_euclid(self.stops as f64),
                        filled_phase,
                        wrapping_fill,
                    ) {
                        selected_color
                    } else {
                        theme.muted()
                    },
                )
                .add_modifier(Modifier::BOLD),
        );
    }

    pub(super) fn render(
        &self,
        frame: &mut Frame<'_>,
        body: Rect,
        theme: &Theme,
        selected_color: Color,
    ) -> Rect {
        let dial_width = DIAL_WIDTH.min(body.width);
        let dial_height = DIAL_HEIGHT.min(body.height.saturating_sub(4));
        let dial = Rect {
            x: body.x + body.width.saturating_sub(dial_width) / 2,
            y: body.y.saturating_add(1),
            width: dial_width,
            height: dial_height,
        }
        .intersection(body);
        self.render_dial(frame, dial, theme, selected_color);
        Rect {
            y: dial.bottom().min(body.bottom().saturating_sub(3)) + 1,
            height: 2,
            ..body
        }
        .intersection(body)
    }
}

fn phase_is_filled(stops: usize, phase: f64, filled_phase: f64, wrapping: bool) -> bool {
    if !wrapping {
        return phase <= filled_phase + f64::EPSILON;
    }

    let max_phase = (stops - 1) as f64;
    phase <= f64::EPSILON
        || (phase >= max_phase - filled_phase - f64::EPSILON && phase <= max_phase + f64::EPSILON)
}

fn dial_position(
    stops: usize,
    area: Rect,
    center_x: f64,
    center_y: f64,
    radius_x: f64,
    radius_y: f64,
    phase: f64,
) -> Position {
    let angle = -FRAC_PI_2 + TAU * phase / stops as f64;
    let x = center_x + angle.cos() * radius_x;
    let y = center_y + angle.sin() * radius_y;
    Position::new(
        area.x
            + x.round()
                .clamp(0.0, f64::from(area.width.saturating_sub(1))) as u16,
        area.y
            + y.round()
                .clamp(0.0, f64::from(area.height.saturating_sub(1))) as u16,
    )
}

fn draw_dot(buffer: &mut Buffer, position: Position, symbol: &str, style: Style) {
    buffer[position].set_symbol(symbol).set_style(style);
}
