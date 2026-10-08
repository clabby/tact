//! Per-entry layout cache for the transcript.
//!
//! Layouts are keyed by entry and invalidated when the entry's revision, the
//! render width, or its expansion state changes. Running tools keep their cached
//! detail rows while only the summary rows are rebuilt as the live timer ticks.
//! The cache also owns the session's expansion choices and the image cache that
//! layouts draw prepared image protocols from.

use super::{
    image,
    markdown::{self, ImageState},
    render::EntryRenderer,
    viewport::Anchor,
};
use crate::{
    app::theme::Theme,
    core::transcript::{EntryId, EntryKind, TranscriptEntry},
    tui::format::duration_display_tick,
};
use ratatui::text::Line;
use ratatui_image::sliced::SlicedProtocol;
use std::{
    collections::{HashMap, hash_map::Entry},
    env,
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

pub(super) struct LayoutCache {
    entries: HashMap<EntryId, CachedEntry>,
    live_tool_durations: HashMap<EntryId, u64>,
    expansion_overrides: HashMap<EntryId, bool>,
    expand_all: Option<bool>,
    workspace: PathBuf,
    images: image::Cache,
}

/// The rendered layout of one entry together with the inputs it was built from.
pub(super) struct CachedEntry {
    revision: u64,
    width: u16,
    expanded: bool,
    pub(super) live_duration_ns: Option<u64>,
    /// Leading rows that summarize a tool; the remaining rows are its details.
    pub(super) tool_summary_lines: usize,
    pub(super) lines: Vec<Line<'static>>,
    pub(super) images: Vec<markdown::ImagePlacement>,
    links: Vec<Vec<markdown::LinkSpan>>,
    pub(super) selections: Vec<Vec<markdown::SourceSpan>>,
    envelopes: Vec<markdown::SourceEnvelope>,
    selection_source: Option<String>,
    image_state: ImageState,
}

impl Default for LayoutCache {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            live_tool_durations: HashMap::new(),
            expansion_overrides: HashMap::new(),
            expand_all: None,
            workspace: env::current_dir().unwrap_or_default(),
            images: image::Cache::default(),
        }
    }
}

impl LayoutCache {
    pub(super) fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// Resolves relative image paths against `workspace` and drops every layout
    /// and image prepared for the previous workspace.
    pub(super) fn set_workspace(&mut self, workspace: PathBuf) {
        self.workspace = workspace;
        self.entries.clear();
        self.images.clear();
    }

    pub(super) const fn images(&self) -> &image::Cache {
        &self.images
    }

    /// Invalidates image protocols after the terminal may have discarded them.
    /// Layouts that are still waiting on image preparation are rebuilt; loaded
    /// images keep their dimensions and are retransmitted when next drawn.
    pub(super) fn refresh_terminal_images(&mut self) {
        self.images.advance_terminal_generation();
        self.entries
            .retain(|_, entry| entry.image_state != ImageState::Pending);
        for entry in self.entries.values_mut() {
            for image in &mut entry.images {
                image.retransmit = true;
            }
        }
    }

    /// Collects finished image preparation and drops the layouts it affects.
    /// Returns whether visible image output changed.
    pub(super) fn poll_images(&mut self, now: Instant) -> bool {
        let result = self.images.poll(now);
        match result.layout_change {
            image::LayoutChange::Ready => {
                self.entries
                    .retain(|_, entry| entry.image_state == ImageState::None);
            }
            image::LayoutChange::Pending => {
                self.entries
                    .retain(|_, entry| entry.image_state != ImageState::Pending);
            }
            image::LayoutChange::None => {}
        }
        result.render_changed
    }

    pub(super) fn forget(&mut self, id: EntryId) {
        self.entries.remove(&id);
        self.live_tool_durations.remove(&id);
        self.expansion_overrides.remove(&id);
    }

    pub(super) fn layout(
        &mut self,
        entry: &TranscriptEntry,
        width: u16,
        theme: &Theme,
    ) -> &[Line<'static>] {
        let expanded = self.is_expanded(entry);
        let live_duration_ns = self.live_tool_durations.get(&entry.id).copied();
        let mut renderer = EntryRenderer {
            width,
            theme,
            workspace: &self.workspace,
            images: &mut self.images,
        };
        let cached = match self.entries.entry(entry.id) {
            Entry::Occupied(mut occupied) => {
                let cached = occupied.get();
                if cached.revision != entry.revision
                    || cached.width != width
                    || cached.expanded != expanded
                {
                    occupied.insert(CachedEntry::new(
                        &mut renderer,
                        entry,
                        live_duration_ns,
                        expanded,
                    ));
                } else if cached.live_duration_ns != live_duration_ns {
                    occupied
                        .get_mut()
                        .update_live_duration(&renderer, entry, live_duration_ns);
                }
                occupied.into_mut()
            }
            Entry::Vacant(vacant) => vacant.insert(CachedEntry::new(
                &mut renderer,
                entry,
                live_duration_ns,
                expanded,
            )),
        };
        &cached.lines
    }

    /// Records a running tool's elapsed time. Returns whether the displayed
    /// duration changed, which is the only case that requires a new frame.
    pub(super) fn set_live_tool_duration(&mut self, id: EntryId, duration_ns: u64) -> bool {
        let display_changed = self.live_tool_durations.get(&id).is_none_or(|previous| {
            duration_display_tick(*previous) != duration_display_tick(duration_ns)
        });
        if display_changed {
            self.live_tool_durations.insert(id, duration_ns);
        }
        display_changed
    }

    pub(super) fn retain_live_tool_durations(&mut self, mut retain: impl FnMut(EntryId) -> bool) {
        self.live_tool_durations.retain(|id, _| retain(*id));
    }

    pub(super) fn toggle(&mut self, entry: &TranscriptEntry) {
        let expanded = self.is_expanded(entry);
        self.expansion_overrides.insert(entry.id, !expanded);
        self.entries.remove(&entry.id);
    }

    /// Flips every expandable to one shared state, which also applies to
    /// entries that arrive later. Individual toggles made before are discarded.
    pub(super) fn toggle_all(&mut self) {
        self.expand_all = Some(!matches!(self.expand_all, Some(true)));
        self.expansion_overrides.clear();
        self.entries.clear();
    }

    fn is_expanded(&self, entry: &TranscriptEntry) -> bool {
        self.expansion_overrides
            .get(&entry.id)
            .copied()
            .or(self.expand_all)
            .unwrap_or_else(
                || matches!(&entry.kind, EntryKind::Tool(tool) if tool.name == "update_plan"),
            )
    }

    pub(super) fn line(&self, anchor: Anchor) -> Option<&Line<'static>> {
        self.entries
            .get(&anchor.entry)
            .and_then(|cached| cached.lines.get(anchor.line))
    }

    pub(super) fn links(&self, anchor: Anchor) -> &[markdown::LinkSpan] {
        self.entries
            .get(&anchor.entry)
            .and_then(|cached| cached.links.get(anchor.line))
            .map_or(&[], Vec::as_slice)
    }

    pub(super) fn selections(&self, anchor: Anchor) -> &[markdown::SourceSpan] {
        self.entries
            .get(&anchor.entry)
            .and_then(|cached| cached.selections.get(anchor.line))
            .map_or(&[], Vec::as_slice)
    }

    /// Returns the image covering `anchor` and the layout line it starts on,
    /// retransmitting its protocol first when the terminal discarded it.
    pub(super) fn image(&mut self, anchor: Anchor) -> Option<(usize, Arc<SlicedProtocol>)> {
        let workspace = &self.workspace;
        let images = &mut self.images;
        let cached = self.entries.get_mut(&anchor.entry)?;
        let index = cached
            .images
            .partition_point(|image| image.line <= anchor.line)
            .checked_sub(1)?;
        let image = cached.images.get_mut(index)?;
        let end = image
            .line
            .saturating_add(usize::from(image.protocol.size().height));
        if anchor.line >= end {
            return None;
        }
        if image.retransmit {
            let size = image.protocol.size();
            match images.retransmit(&image.destination, workspace, size) {
                image::LoadResult::Loaded(protocol) => {
                    image.protocol = protocol;
                    image.retransmit = false;
                }
                image::LoadResult::Failed | image::LoadResult::Unsupported => {
                    image.retransmit = false;
                }
                image::LoadResult::Deferred => {}
            }
        }
        Some((image.line, Arc::clone(&image.protocol)))
    }

    /// The text that selection offsets of `entry` index into, or `None` when
    /// the entry has no selectable text.
    pub(super) fn selection_source<'a>(&'a self, entry: &'a TranscriptEntry) -> Option<&'a str> {
        self.entries
            .get(&entry.id)
            .and_then(|cached| cached.selection_source.as_deref())
            .or_else(|| match &entry.kind {
                EntryKind::User { text, .. }
                | EntryKind::Assistant { text, .. }
                | EntryKind::Reasoning { text } => Some(text),
                _ => None,
            })
    }

    /// Widens a selected source range until it covers the markup of every
    /// construct whose rendered content it fully contains.
    pub(super) fn expand_selection(
        &self,
        entry: EntryId,
        mut selected: Range<usize>,
    ) -> Range<usize> {
        let Some(cached) = self.entries.get(&entry) else {
            return selected;
        };
        loop {
            let previous = selected.clone();
            for envelope in &cached.envelopes {
                if selected.start <= envelope.content.start && selected.end >= envelope.content.end
                {
                    selected.start = selected.start.min(envelope.source.start);
                    selected.end = selected.end.max(envelope.source.end);
                }
            }
            if selected == previous {
                return selected;
            }
        }
    }

    #[cfg(test)]
    pub(super) fn cached(&self, id: EntryId) -> Option<&CachedEntry> {
        self.entries.get(&id)
    }

    #[cfg(test)]
    pub(super) fn first_image(&self) -> Option<&Arc<SlicedProtocol>> {
        self.entries
            .values()
            .find_map(|entry| entry.images.first())
            .map(|image| &image.protocol)
    }

    #[cfg(test)]
    pub(super) fn set_expanded(&mut self, id: EntryId, expanded: bool) {
        self.expansion_overrides.insert(id, expanded);
    }

    #[cfg(test)]
    pub(super) fn set_inline_images(&mut self, inline_images: bool) {
        self.images = image::Cache::with_inline_images(inline_images);
    }
}

impl CachedEntry {
    fn new(
        renderer: &mut EntryRenderer<'_>,
        entry: &TranscriptEntry,
        live_duration_ns: Option<u64>,
        expanded: bool,
    ) -> Self {
        let layout = renderer.entry(entry, live_duration_ns, expanded);
        let tool_summary_lines = match (&entry.kind, live_duration_ns) {
            (EntryKind::Tool(tool), Some(duration_ns)) => renderer
                .live_tool_summary(entry, tool, duration_ns, expanded)
                .len(),
            _ => 0,
        };
        Self {
            revision: entry.revision,
            width: renderer.width,
            expanded,
            live_duration_ns,
            tool_summary_lines,
            lines: layout.lines,
            images: layout.images,
            links: layout.links,
            selections: layout.selections,
            envelopes: layout.envelopes,
            selection_source: layout.selection_source,
            image_state: layout.image_state,
        }
    }

    fn update_live_duration(
        &mut self,
        renderer: &EntryRenderer<'_>,
        entry: &TranscriptEntry,
        live_duration_ns: Option<u64>,
    ) {
        let (EntryKind::Tool(tool), Some(duration_ns)) = (&entry.kind, live_duration_ns) else {
            return;
        };
        let summary = renderer.live_tool_summary(entry, tool, duration_ns, self.expanded);
        let summary_len = summary.len();
        self.lines.splice(0..self.tool_summary_lines, summary);
        self.links.splice(
            0..self.tool_summary_lines,
            std::iter::repeat_with(Vec::new).take(summary_len),
        );
        self.selections.splice(
            0..self.tool_summary_lines,
            std::iter::repeat_with(Vec::new).take(summary_len),
        );
        self.live_duration_ns = live_duration_ns;
        self.tool_summary_lines = summary_len;
    }
}
