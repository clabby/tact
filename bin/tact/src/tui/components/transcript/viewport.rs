//! Scroll position of the transcript viewport.
//!
//! The viewport is positioned by a layout anchor (an entry and one of its
//! rendered lines) rather than by a row offset, so entries can grow, rewrap, or
//! disappear without moving the content a reader is looking at. In
//! [ScrollState::Follow] the newest line stays at the bottom; in
//! [ScrollState::Detached] the anchor stays at the top. Scroll and reveal
//! requests are queued by events and resolved during the next render, once the
//! viewport width is known and layouts can be measured.

use super::cache::LayoutCache;
use crate::{
    app::theme::Theme,
    core::transcript::{EntryId, TranscriptModel},
};
use std::mem;

/// One rendered line of one transcript entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Anchor {
    pub(super) entry: EntryId,
    pub(super) line: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ScrollState {
    Follow,
    Detached(Anchor),
}

#[derive(Clone, Copy, Default)]
pub(in crate::tui::components) enum ScrollCommand {
    #[default]
    None,
    Rows(i32),
    PinnedPromptRows(i32),
    Home,
    End,
}

/// A request to move the viewport so an expandable entry stays in view.
#[derive(Clone, Copy)]
pub(super) enum Reveal {
    /// Place the entry's first line at the top of the viewport.
    Entry(EntryId),
    /// Keep the entry's first line on the viewport row it occupied before its
    /// layout changed.
    Preserve { entry: EntryId, row: u16 },
}

impl Reveal {
    const fn entry(self) -> EntryId {
        match self {
            Self::Entry(entry) | Self::Preserve { entry, .. } => entry,
        }
    }
}

/// The lines to draw for one frame, top to bottom, below `top_padding` blank
/// rows that keep a short transcript aligned to the bottom of the viewport.
#[derive(Default)]
pub(super) struct RenderPlan {
    pub(super) top_padding: u16,
    pub(super) anchors: Vec<Anchor>,
}

pub(super) struct Viewport {
    state: ScrollState,
    pending_scroll: ScrollCommand,
    pending_reveal: Option<Reveal>,
    /// The top anchor of the last rendered frame.
    top: Option<Anchor>,
    height: u16,
    /// Changes that arrived while detached, advertised so the reader can return.
    new_updates: u64,
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            state: ScrollState::Follow,
            pending_scroll: ScrollCommand::None,
            pending_reveal: None,
            top: None,
            height: 0,
            new_updates: 0,
        }
    }
}

impl Viewport {
    #[cfg(test)]
    pub(super) const fn state(&self) -> ScrollState {
        self.state
    }

    pub(super) const fn is_detached(&self) -> bool {
        matches!(self.state, ScrollState::Detached(_))
    }

    #[cfg(test)]
    pub(super) const fn top(&self) -> Option<Anchor> {
        self.top
    }

    pub(super) const fn new_updates(&self) -> u64 {
        self.new_updates
    }

    pub(super) const fn set_height(&mut self, height: u16) {
        self.height = height;
    }

    /// Rows moved by one page, keeping two rows of overlap for context.
    pub(super) fn page_size(&self) -> i32 {
        i32::from(self.height.saturating_sub(2).max(1))
    }

    /// Pins `anchor` to the top of the viewport and drops any queued scroll.
    pub(super) const fn detach_at(&mut self, anchor: Anchor) {
        self.state = ScrollState::Detached(anchor);
        self.pending_scroll = ScrollCommand::None;
    }

    /// Returns to the tail. Returns whether the viewport was detached.
    pub(super) const fn follow(&mut self) -> bool {
        let was_detached = self.is_detached();
        self.state = ScrollState::Follow;
        self.pending_scroll = ScrollCommand::None;
        self.new_updates = 0;
        was_detached
    }

    pub(super) const fn request_scroll(&mut self, command: ScrollCommand) {
        self.pending_scroll = command;
    }

    pub(super) const fn request_reveal(&mut self, reveal: Reveal) {
        self.pending_reveal = Some(reveal);
    }

    /// Counts a transcript change the reader cannot see from a detached viewport.
    pub(super) const fn record_update(&mut self) {
        if self.is_detached() {
            self.new_updates = self.new_updates.saturating_add(1);
        }
    }

    /// Drops every anchor and request that refers to a removed entry.
    pub(super) fn forget(&mut self, id: EntryId) {
        if self.top.is_some_and(|anchor| anchor.entry == id) {
            self.top = None;
        }
        if matches!(self.state, ScrollState::Detached(anchor) if anchor.entry == id) {
            self.state = ScrollState::Follow;
        }
        if self
            .pending_reveal
            .is_some_and(|reveal| reveal.entry() == id)
        {
            self.pending_reveal = None;
        }
    }

    /// Resolves queued requests and selects the lines for a viewport of
    /// `height` rows. A detached viewport that reaches the last line resumes
    /// following.
    pub(super) fn plan(&mut self, layouts: &mut Layouts<'_>, height: u16) -> RenderPlan {
        if layouts.width == 0 || height == 0 {
            return RenderPlan::default();
        }
        self.apply_pending_reveal(layouts);
        self.apply_pending_scroll(layouts, height);
        let top = match self.state {
            ScrollState::Follow => layouts.tail_top(height),
            ScrollState::Detached(anchor) => {
                let top = layouts
                    .resolve(anchor)
                    .map(|top| layouts.fill_from(top, height));
                if let Some(top) = top {
                    self.state = ScrollState::Detached(top);
                }
                top
            }
        };
        self.top = top;

        let anchors = top.map_or_else(Vec::new, |anchor| {
            layouts.collect_forward(anchor, usize::from(height))
        });
        if self.is_detached() && anchors.last().copied() == layouts.last() {
            self.state = ScrollState::Follow;
            self.new_updates = 0;
        }
        let occupied = u16::try_from(anchors.len()).unwrap_or(u16::MAX);
        layouts.warm_overscan(top, height);
        RenderPlan {
            top_padding: height.saturating_sub(occupied),
            anchors,
        }
    }

    fn apply_pending_reveal(&mut self, layouts: &mut Layouts<'_>) {
        let Some(reveal) = self.pending_reveal.take() else {
            return;
        };
        let (entry, row) = match reveal {
            Reveal::Entry(entry) => (entry, 0),
            Reveal::Preserve { entry, row } => (entry, row),
        };
        let (top, _) = layouts.move_by(Anchor { entry, line: 0 }, -i32::from(row));
        self.state = ScrollState::Detached(top);
    }

    fn apply_pending_scroll(&mut self, layouts: &mut Layouts<'_>, height: u16) {
        match mem::take(&mut self.pending_scroll) {
            ScrollCommand::None | ScrollCommand::PinnedPromptRows(_) => {}
            ScrollCommand::End => {
                self.state = ScrollState::Follow;
                self.new_updates = 0;
            }
            ScrollCommand::Home => {
                if let Some(anchor) = layouts.first() {
                    self.state = ScrollState::Detached(anchor);
                }
            }
            ScrollCommand::Rows(rows) if rows < 0 => {
                let start = match self.state {
                    ScrollState::Follow => self.top.or_else(|| layouts.tail_top(height)),
                    ScrollState::Detached(anchor) => Some(anchor),
                };
                if let Some(start) = start {
                    self.state = ScrollState::Detached(layouts.move_by(start, rows).0);
                }
            }
            ScrollCommand::Rows(rows) => {
                let ScrollState::Detached(start) = self.state else {
                    return;
                };
                let (anchor, reached_end) = layouts.move_by(start, rows);
                if reached_end {
                    self.state = ScrollState::Follow;
                    self.new_updates = 0;
                } else {
                    self.state = ScrollState::Detached(anchor);
                }
            }
        }
    }

    #[cfg(test)]
    pub(super) const fn set_state(&mut self, state: ScrollState) {
        self.state = state;
    }
}

/// Line-by-line navigation over the visible entries of a transcript at one
/// width. Layouts are built through the cache on demand, so walking far from
/// the viewport renders entries that have not been drawn yet.
pub(super) struct Layouts<'a> {
    pub(super) model: &'a TranscriptModel,
    pub(super) cache: &'a mut LayoutCache,
    pub(super) width: u16,
    pub(super) theme: &'a Theme,
}

impl Layouts<'_> {
    fn line_count(&mut self, index: usize) -> Option<usize> {
        let entry = &self.model.entries()[index];
        if entry.hidden {
            return None;
        }
        let len = self.cache.layout(entry, self.width, self.theme).len();
        (len > 0).then_some(len)
    }

    fn first(&mut self) -> Option<Anchor> {
        (0..self.model.entries().len()).find_map(|index| {
            self.line_count(index).map(|_| Anchor {
                entry: self.model.entries()[index].id,
                line: 0,
            })
        })
    }

    fn last(&mut self) -> Option<Anchor> {
        (0..self.model.entries().len()).rev().find_map(|index| {
            self.line_count(index).map(|len| Anchor {
                entry: self.model.entries()[index].id,
                line: len - 1,
            })
        })
    }

    /// Clamps `anchor` to its entry's current layout, moving to the next visible
    /// entry when the anchored entry has been hidden.
    fn resolve(&mut self, anchor: Anchor) -> Option<Anchor> {
        let index = self.model.index_of(anchor.entry)?;
        if self.model.entries()[index].hidden {
            return self.next_visible_entry(index);
        }
        let len = self.line_count(index)?;
        Some(Anchor {
            entry: anchor.entry,
            line: anchor.line.min(len - 1),
        })
    }

    fn previous(&mut self, anchor: Anchor) -> Option<Anchor> {
        if anchor.line > 0 {
            return Some(Anchor {
                line: anchor.line - 1,
                ..anchor
            });
        }
        let index = self.model.index_of(anchor.entry)?;
        (0..index).rev().find_map(|previous| {
            self.line_count(previous).map(|len| Anchor {
                entry: self.model.entries()[previous].id,
                line: len - 1,
            })
        })
    }

    fn next(&mut self, anchor: Anchor) -> Option<Anchor> {
        let index = self.model.index_of(anchor.entry)?;
        let entry = &self.model.entries()[index];
        let len = self.cache.layout(entry, self.width, self.theme).len();
        if anchor.line + 1 < len {
            return Some(Anchor {
                line: anchor.line + 1,
                ..anchor
            });
        }
        self.next_visible_entry(index)
    }

    fn next_visible_entry(&mut self, index: usize) -> Option<Anchor> {
        (index + 1..self.model.entries().len()).find_map(|next| {
            self.line_count(next).map(|_| Anchor {
                entry: self.model.entries()[next].id,
                line: 0,
            })
        })
    }

    /// Moves `rows` lines up (negative) or down. Returns the reached anchor and
    /// whether a downward move ran past the last line.
    fn move_by(&mut self, mut anchor: Anchor, rows: i32) -> (Anchor, bool) {
        if rows < 0 {
            for _ in 0..rows.unsigned_abs() {
                let Some(previous) = self.previous(anchor) else {
                    break;
                };
                anchor = previous;
            }
            return (anchor, false);
        }
        for _ in 0..rows.unsigned_abs() {
            let Some(next) = self.next(anchor) else {
                return (anchor, true);
            };
            anchor = next;
        }
        (anchor, false)
    }

    /// The top anchor of a viewport of `height` rows that ends on the last line.
    fn tail_top(&mut self, height: u16) -> Option<Anchor> {
        let mut anchor = self.last()?;
        for _ in 1..height {
            let Some(previous) = self.previous(anchor) else {
                break;
            };
            anchor = previous;
        }
        Some(anchor)
    }

    /// Moves a top anchor up just far enough that the viewport has no blank
    /// rows below the last line.
    fn fill_from(&mut self, anchor: Anchor, height: u16) -> Anchor {
        let mut last = anchor;
        let mut available = 1_u16;
        while available < height {
            let Some(next) = self.next(last) else {
                break;
            };
            last = next;
            available = available.saturating_add(1);
        }

        let mut top = anchor;
        for _ in available..height {
            let Some(previous) = self.previous(top) else {
                break;
            };
            top = previous;
        }
        top
    }

    fn collect_forward(&mut self, mut anchor: Anchor, height: usize) -> Vec<Anchor> {
        let mut anchors = Vec::with_capacity(height);
        while anchors.len() < height {
            let Some(entry) = self.model.entry(anchor.entry) else {
                break;
            };
            if self
                .cache
                .layout(entry, self.width, self.theme)
                .get(anchor.line)
                .is_some()
            {
                anchors.push(anchor);
            }
            let Some(next) = self.next(anchor) else {
                break;
            };
            anchor = next;
        }
        anchors
    }

    /// Lays out one viewport above and two below the visible lines so that
    /// scrolling does not stall on rendering entries that just came into view.
    fn warm_overscan(&mut self, top: Option<Anchor>, height: u16) {
        let Some(top) = top else {
            return;
        };
        let _ = self.move_by(top, -i32::from(height));
        let _ = self.move_by(top, i32::from(height.saturating_mul(2)));
    }
}
