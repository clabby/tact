//! The prompt pinned above a detached transcript.
//!
//! While the reader scrolls through a turn whose prompt has left the viewport,
//! up to [PINNED_PROMPT_MAX_HEIGHT] rows of that prompt stay visible above the
//! transcript. The pinned rows scroll independently of the transcript, and a
//! click jumps the transcript to the full prompt.

use super::{cache::LayoutCache, hits::HitMap, viewport::Anchor};
use crate::{app::theme::Theme, core::transcript::EntryId};
use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Modifier, Style},
};

const PINNED_PROMPT_MAX_HEIGHT: u16 = 3;

#[derive(Clone, Copy)]
pub(super) struct PinnedPrompt {
    pub(super) entry: EntryId,
    pub(super) area: Rect,
    /// The first prompt line shown in the pinned area.
    offset: usize,
    max_offset: usize,
}

impl PinnedPrompt {
    /// Lays out the pinned area at the top of `area` for a prompt with
    /// `line_count` content rows. One row of `area` always remains for the
    /// transcript. The scroll offset survives while the same prompt stays
    /// pinned.
    pub(super) fn place(
        previous: Option<Self>,
        entry: EntryId,
        line_count: usize,
        area: Rect,
    ) -> Option<Self> {
        let available_height = area.height.saturating_sub(1).min(PINNED_PROMPT_MAX_HEIGHT);
        let height = available_height.min(u16::try_from(line_count).unwrap_or(u16::MAX));
        if height == 0 {
            return None;
        }
        let max_offset = line_count.saturating_sub(usize::from(height));
        let offset = previous
            .filter(|prompt| prompt.entry == entry)
            .map_or(0, |prompt| prompt.offset.min(max_offset));
        Some(Self {
            entry,
            area: Rect { height, ..area },
            offset,
            max_offset,
        })
    }

    pub(super) fn contains(self, position: Position) -> bool {
        self.area.contains(position)
    }

    pub(super) const fn can_scroll_up(self) -> bool {
        self.offset > 0
    }

    pub(super) const fn can_scroll_down(self) -> bool {
        self.offset < self.max_offset
    }

    pub(super) fn scroll_by(&mut self, rows: i32) {
        let offset = i64::try_from(self.offset).unwrap_or(i64::MAX);
        let max_offset = i64::try_from(self.max_offset).unwrap_or(i64::MAX);
        self.offset = usize::try_from((offset + i64::from(rows)).clamp(0, max_offset))
            .unwrap_or(self.max_offset);
    }

    /// Draws the pinned rows on the code background, with ellipsis markers on
    /// the edges that hide more of the prompt.
    pub(super) fn render(
        self,
        frame: &mut Frame<'_>,
        cache: &LayoutCache,
        hits: &mut HitMap,
        theme: &Theme,
    ) {
        for row in 0..self.area.height {
            let y = self.area.y.saturating_add(row);
            let anchor = Anchor {
                entry: self.entry,
                line: self.offset.saturating_add(usize::from(row)),
            };
            if let Some(content) = cache.line(anchor) {
                frame
                    .buffer_mut()
                    .set_line(self.area.x, y, content, self.area.width);
            }
            hits.push_row(y, anchor);
        }
        frame
            .buffer_mut()
            .set_style(self.area, Style::default().bg(theme.code_background()));

        let marker_style = Style::default()
            .fg(theme.muted())
            .add_modifier(Modifier::BOLD);
        let marker_x = self.area.right().saturating_sub(1);
        let markers = [
            (self.can_scroll_up(), self.area.y),
            (self.can_scroll_down(), self.area.bottom().saturating_sub(1)),
        ];
        for (_, y) in markers.into_iter().filter(|(hidden, _)| *hidden) {
            if let Some(cell) = frame.buffer_mut().cell_mut(Position::new(marker_x, y)) {
                cell.set_symbol("…").set_style(marker_style);
            }
        }
    }
}
