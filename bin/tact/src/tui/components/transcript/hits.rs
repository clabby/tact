//! Screen regions recorded while drawing the transcript.
//!
//! Each frame rebuilds the hit map; mouse handling and text selection read it
//! until the next frame. Rows and columns are absolute terminal coordinates.
//! Every painted transcript row maps back to its layout anchor so a terminal
//! position resolves to a byte range in the source text of an entry.

use super::{cache::LayoutCache, markdown::LinkSpan, viewport::Anchor};
use crate::{
    core::transcript::{EntryId, EntryKind, TranscriptModel},
    tui::components::selection::{TextRange, TextSpan},
};
use ratatui::{
    buffer::Buffer,
    layout::{Position, Rect},
    style::{Color, Style},
};
use std::{ops::Range, sync::Arc};

#[derive(Default)]
pub(super) struct HitMap {
    area: Rect,
    expandables: Vec<ExpandableHit>,
    links: Vec<LinkHit>,
    rows: Vec<(u16, Anchor)>,
}

/// The summary row of an expandable entry.
#[derive(Clone, Copy)]
struct ExpandableHit {
    entry: EntryId,
    row: u16,
}

struct LinkHit {
    destination: Arc<str>,
    row: u16,
    columns: Range<u16>,
}

/// How far a selection query may reach from the pointer position.
#[derive(Clone, Copy)]
pub(super) enum SelectionReach {
    /// Only spans on the pointer's row, or on the pointer's entry when that row
    /// has no selectable text.
    Row,
    /// The nearest selectable span anywhere in the viewport, preferring spans
    /// semantically farther from `origin` on ties so a drag keeps extending.
    Nearest { origin: Option<TextSpan> },
}

impl HitMap {
    /// Clears the previous frame's regions for a frame drawn into `area`.
    pub(super) fn begin_frame(&mut self, area: Rect) {
        self.area = area;
        self.expandables.clear();
        self.links.clear();
        self.rows.clear();
    }

    pub(super) fn push_row(&mut self, row: u16, anchor: Anchor) {
        self.rows.push((row, anchor));
    }

    /// Records the link spans of a row, clipping them to the transcript area.
    pub(super) fn push_links(&mut self, row: u16, links: &[LinkSpan]) {
        let area = self.area;
        self.links.extend(links.iter().map(|link| LinkHit {
            destination: Arc::clone(&link.destination),
            row,
            columns: area.x.saturating_add(link.start)
                ..area.x.saturating_add(link.end).min(area.right()),
        }));
    }

    pub(super) fn push_expandable(&mut self, entry: EntryId, row: u16) {
        self.expandables.push(ExpandableHit { entry, row });
    }

    pub(super) fn forget(&mut self, id: EntryId) {
        self.expandables.retain(|hit| hit.entry != id);
    }

    pub(super) fn expandable_at(&self, row: u16) -> Option<EntryId> {
        self.expandables
            .iter()
            .find(|hit| hit.row == row)
            .map(|hit| hit.entry)
    }

    /// The viewport-relative row of a visible expandable's summary.
    pub(super) fn expandable_row(&self, entry: EntryId) -> Option<u16> {
        self.expandables
            .iter()
            .find(|hit| hit.entry == entry)
            .map(|hit| hit.row.saturating_sub(self.area.y))
    }

    #[cfg(test)]
    pub(super) fn last_expandable(&self) -> Option<EntryId> {
        self.expandables.last().map(|hit| hit.entry)
    }

    pub(super) fn link_at(&self, position: Position) -> Option<Arc<str>> {
        self.links
            .iter()
            .find(|hit| hit.row == position.y && hit.columns.contains(&position.x))
            .map(|hit| Arc::clone(&hit.destination))
    }

    #[cfg(test)]
    pub(super) fn link_start(&self, destination: &str) -> Option<Position> {
        self.links
            .iter()
            .find(|hit| hit.destination.as_ref() == destination)
            .map(|hit| Position::new(hit.columns.start, hit.row))
    }

    #[cfg(test)]
    pub(super) fn expandable_rows(&self) -> Vec<u16> {
        self.expandables.iter().map(|hit| hit.row).collect()
    }

    pub(super) fn selection_span(
        &self,
        position: Position,
        reach: SelectionReach,
        model: &TranscriptModel,
        cache: &LayoutCache,
    ) -> Option<TextSpan> {
        let (across_entries, origin) = match reach {
            SelectionReach::Row => (false, None),
            SelectionReach::Nearest { origin } => (true, origin),
        };
        let exact_row = self
            .rows
            .iter()
            .find(|(row, _)| *row == position.y)
            .map(|(_, anchor)| *anchor);
        let exact = match exact_row {
            Some(exact) => exact,
            None if across_entries => self
                .rows
                .iter()
                .min_by_key(|(row, _)| row.abs_diff(position.y))
                .map(|(_, anchor)| *anchor)?,
            None => return None,
        };
        let exact_has_selections = !cache.selections(exact).is_empty();
        if !exact_has_selections && !across_entries {
            let entry = model.entry(exact.entry)?;
            if matches!(entry.kind, EntryKind::Tool(_)) {
                return None;
            }
            cache.selection_source(entry)?;
        }
        let column = position.x.saturating_sub(self.area.x);
        let mut best = None::<((u16, u16), (usize, usize), TextSpan)>;
        for (row, anchor) in &self.rows {
            if exact_row.is_some() && exact_has_selections && *row != position.y {
                continue;
            }
            if !exact_has_selections && !across_entries && anchor.entry != exact.entry {
                continue;
            }
            for span in cache.selections(*anchor) {
                let column_distance = if column < span.columns.start {
                    span.columns.start - column
                } else {
                    column.saturating_sub(span.columns.end.saturating_sub(1))
                };
                let distance = (row.abs_diff(position.y), column_distance);
                let candidate =
                    TextSpan::new(anchor.entry.index(), span.source.start, span.source.end);
                let semantic_distance = origin.map_or((0, 0), |origin| {
                    (
                        origin.block.abs_diff(candidate.block),
                        origin
                            .start
                            .abs_diff(candidate.start)
                            .max(origin.end.abs_diff(candidate.end)),
                    )
                });
                let replace =
                    best.as_ref()
                        .is_none_or(|(best_distance, best_semantic_distance, _)| {
                            distance < *best_distance
                                || (distance == *best_distance
                                    && semantic_distance > *best_semantic_distance)
                        });
                if replace {
                    best = Some((distance, semantic_distance, candidate));
                }
            }
        }
        best.map(|(_, _, span)| span)
    }

    pub(super) fn render_selection(
        &self,
        buffer: &mut Buffer,
        range: TextRange,
        cache: &LayoutCache,
    ) {
        let selected = Style::reset().fg(Color::Black).bg(Color::Yellow);
        for (row, anchor) in &self.rows {
            for span in cache.selections(*anchor) {
                if !range.includes(anchor.entry.index(), &span.source) {
                    continue;
                }
                for column in span.columns.clone() {
                    let column = self.area.x.saturating_add(column);
                    if let Some(cell) = buffer.cell_mut(Position::new(column, *row)) {
                        cell.set_style(selected);
                    }
                }
            }
        }
    }
}
