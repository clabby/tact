//! The editable text of the composer.
//!
//! [DraftBuffer] owns the draft text, the cursor, and the pasted images whose
//! `[Image #N]` markers live in the text. Editing and cursor movement work on
//! extended grapheme clusters and treat each image marker as one atomic unit:
//! the cursor never rests inside a marker, and deleting any part of a marker
//! removes the whole marker together with its image. Image ranges are kept
//! sorted, disjoint, and in bounds of the text after every edit.

use super::layout::{VisualLayout, byte_at_column};
use crate::{
    app::theme::Theme,
    core::prompt::Submission,
    tui::format::{normalize_line_endings, sanitize_terminal_text, terminal_text_width},
};
use ratatui::{
    buffer::Buffer,
    layout::{Position, Rect},
    style::{Color, Style},
};
use std::{mem, ops::Range};
use unicode_segmentation::UnicodeSegmentation;

/// The direction of a cursor movement or edit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Direction {
    Backward,
    Forward,
}

pub(super) struct DraftBuffer {
    text: String,
    images: Vec<PastedImage>,
    next_image: u64,
    cursor: usize,
    /// The display column vertical movement aims for, kept across consecutive
    /// vertical moves through shorter lines.
    preferred_column: Option<usize>,
    layout: Option<CachedLayout>,
}

/// A draft set aside while the composer edits something else.
pub(crate) struct ComposerDraft {
    text: String,
    images: Vec<PastedImage>,
    next_image: u64,
    cursor: usize,
}

impl ComposerDraft {
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    pub(crate) fn images(&self) -> impl Iterator<Item = (&str, &str)> {
        draft_images(&self.text, &self.images)
    }
}

struct PastedImage {
    range: Range<usize>,
    data_url: String,
}

/// The visual layout of the text for one width and cursor position.
struct CachedLayout {
    width: usize,
    cursor: usize,
    value: VisualLayout,
}

fn draft_images<'a>(
    text: &'a str,
    images: &'a [PastedImage],
) -> impl Iterator<Item = (&'a str, &'a str)> {
    images
        .iter()
        .map(|image| (&text[image.range.clone()], image.data_url.as_str()))
}

impl Default for DraftBuffer {
    fn default() -> Self {
        Self {
            text: String::new(),
            images: Vec::new(),
            next_image: 1,
            cursor: 0,
            preferred_column: None,
            layout: None,
        }
    }
}

impl DraftBuffer {
    pub(super) fn text(&self) -> &str {
        &self.text
    }

    pub(super) const fn cursor(&self) -> usize {
        self.cursor
    }

    pub(super) fn has_images(&self) -> bool {
        !self.images.is_empty()
    }

    /// The images as (marker, data URL) pairs in text order.
    pub(super) fn images(&self) -> impl Iterator<Item = (&str, &str)> {
        draft_images(&self.text, &self.images)
    }

    pub(super) fn cursor_is_at_token_boundary(&self) -> bool {
        self.text[..self.cursor]
            .chars()
            .next_back()
            .is_none_or(char::is_whitespace)
    }

    /// Replaces the text and moves the cursor to its end. An image survives
    /// while its marker still occurs in the new text, so an edit made elsewhere
    /// keeps the images whose markers it left alone.
    pub(super) fn replace(&mut self, text: String) {
        let text = if text.contains('\r') {
            normalize_line_endings(&text).into_owned()
        } else {
            text
        };
        let previous = mem::replace(&mut self.text, text);
        let mut search_from = 0;
        self.images.retain_mut(|image| {
            let marker = &previous[image.range.clone()];
            let Some(offset) = self.text[search_from..].find(marker) else {
                return false;
            };
            let start = search_from + offset;
            image.range = start..start + marker.len();
            search_from = image.range.end;
            true
        });
        if self.images.is_empty() {
            self.next_image = 1;
        }
        self.cursor = self.text.len();
        self.preferred_column = None;
        self.layout = None;
    }

    /// Takes the trimmed draft as a submission and clears the buffer. Images
    /// whose markers fall in the trimmed whitespace are discarded.
    pub(super) fn take_submission(&mut self) -> Option<Submission> {
        let trimmed = self.text.trim();
        if trimmed.is_empty() {
            return None;
        }

        let start = self.text.len() - self.text.trim_start().len();
        let end = start + trimmed.len();
        let images = self
            .images
            .iter()
            .filter(|image| image.range.start >= start && image.range.end <= end)
            .map(|image| {
                (
                    image.range.start - start..image.range.end - start,
                    image.data_url.clone(),
                )
            });
        let prompt = Submission::multimodal(trimmed.to_owned(), images);
        self.replace(String::new());
        Some(prompt)
    }

    pub(super) fn take(&mut self) -> Option<ComposerDraft> {
        if self.text.is_empty() {
            return None;
        }
        let draft = ComposerDraft {
            text: mem::take(&mut self.text),
            images: mem::take(&mut self.images),
            next_image: mem::replace(&mut self.next_image, 1),
            cursor: mem::take(&mut self.cursor),
        };
        self.preferred_column = None;
        self.layout = None;
        Some(draft)
    }

    pub(super) fn restore(&mut self, draft: ComposerDraft) {
        self.text = draft.text;
        self.images = draft.images;
        self.next_image = draft.next_image;
        self.cursor = draft.cursor;
        self.preferred_column = None;
        self.layout = None;
    }

    /// Inserts text at the cursor, after the marker when the cursor is inside
    /// one.
    pub(super) fn insert(&mut self, text: &str) {
        let text = normalize_line_endings(text);
        self.move_cursor_out_of_image();
        for image in &mut self.images {
            if image.range.start >= self.cursor {
                image.range.start += text.len();
                image.range.end += text.len();
            }
        }
        self.text.insert_str(self.cursor, &text);
        self.cursor += text.len();
        self.preferred_column = None;
        self.layout = None;
    }

    /// Inserts a numbered marker for a pasted image at the cursor.
    pub(super) fn insert_image(&mut self, data_url: String) {
        self.move_cursor_out_of_image();
        let marker = format!("[Image #{}]", self.next_image);
        let start = self.cursor;
        self.insert(&marker);
        self.images.push(PastedImage {
            range: start..self.cursor,
            data_url,
        });
        self.images.sort_by_key(|image| image.range.start);
        self.next_image = self.next_image.saturating_add(1);
    }

    pub(super) fn append_image(&mut self, data_url: String) {
        self.cursor = self.text.len();
        self.insert_image(data_url);
    }

    /// Removes `range` and leaves the cursor at its start. Images overlapping
    /// the range must already have been removed by the caller.
    pub(super) fn remove_range(&mut self, range: Range<usize>) {
        let removed = range.len();
        self.text.drain(range.clone());
        for image in &mut self.images {
            if image.range.start >= range.end {
                image.range.start -= removed;
                image.range.end -= removed;
            }
        }
        self.cursor = range.start;
        self.preferred_column = None;
        self.layout = None;
    }

    /// Moves one grapheme, stepping over a whole image marker.
    pub(super) fn move_grapheme(&mut self, direction: Direction) -> bool {
        let target = match direction {
            Direction::Backward => {
                let Some((previous, _)) =
                    self.text[..self.cursor].grapheme_indices(true).next_back()
                else {
                    return false;
                };
                self.images
                    .iter()
                    .find(|image| image.range.contains(&previous))
                    .map_or(previous, |image| image.range.start)
            }
            Direction::Forward => {
                let Some(next) = self.text[self.cursor..].graphemes(true).next() else {
                    return false;
                };
                let target = self.cursor + next.len();
                self.snap_out_of_image(target, Direction::Forward)
            }
        };
        self.cursor = target;
        self.preferred_column = None;
        true
    }

    /// Moves to the start (backward) or end (forward) of the next run of
    /// alphanumeric graphemes, skipping any delimiters before it.
    pub(super) fn move_word(&mut self, direction: Direction) -> bool {
        let is_word = |grapheme: &str| grapheme.chars().any(char::is_alphanumeric);
        let mut found_word = false;
        let target = match direction {
            Direction::Forward => {
                let mut target = self.text.len();
                for (offset, grapheme) in self.text[self.cursor..].grapheme_indices(true) {
                    if is_word(grapheme) {
                        found_word = true;
                        target = self.cursor + offset + grapheme.len();
                    } else if found_word {
                        break;
                    }
                }
                target
            }
            Direction::Backward => {
                let mut target = 0;
                for (offset, grapheme) in self.text[..self.cursor].grapheme_indices(true).rev() {
                    if is_word(grapheme) {
                        found_word = true;
                        target = offset;
                    } else if found_word {
                        break;
                    }
                }
                target
            }
        };
        let target = self.snap_out_of_image(target, direction);
        if target == self.cursor {
            return false;
        }
        self.cursor = target;
        self.preferred_column = None;
        true
    }

    /// Moves to the neighbouring visual line at `width`, aiming for the
    /// preferred column.
    pub(super) fn move_vertical(&mut self, direction: Direction, width: usize) -> bool {
        let layout = self.visual_layout(width);
        let cursor_row = layout.cursor_row;
        let cursor_column = layout.cursor_column;
        let target_row = match direction {
            Direction::Backward => cursor_row.checked_sub(1),
            Direction::Forward => Some(cursor_row + 1).filter(|row| *row < layout.lines.len()),
        };
        let Some(target_row) = target_row else {
            return false;
        };
        let line = layout.lines[target_row].clone();
        let desired = *self.preferred_column.get_or_insert(cursor_column);
        let target = byte_at_column(&self.text, &line, desired);
        self.cursor = self.snap_out_of_image(target, direction);
        true
    }

    /// Moves to the start (backward) or end (forward) of the visual line at
    /// `width`.
    pub(super) fn move_to_visual_edge(&mut self, direction: Direction, width: usize) -> bool {
        let layout = self.visual_layout(width);
        let line = &layout.lines[layout.cursor_row];
        let target = match direction {
            Direction::Backward => line.start,
            Direction::Forward => line.end,
        };
        self.move_to(target)
    }

    /// Moves to the start (backward) or end (forward) of the logical line.
    pub(super) fn move_to_logical_edge(&mut self, direction: Direction) -> bool {
        let target = match direction {
            Direction::Forward => self.logical_line_end(),
            Direction::Backward => self.text[..self.cursor]
                .rfind('\n')
                .map_or(0, |offset| offset + '\n'.len_utf8()),
        };
        self.move_to(target)
    }

    pub(super) fn backspace(&mut self) -> bool {
        if let Some(index) = self
            .images
            .iter()
            .position(|image| image.range.start < self.cursor && self.cursor <= image.range.end)
        {
            let range = self.images.remove(index).range;
            self.remove_range(range);
            return true;
        }
        let Some((previous, _)) = self.text[..self.cursor].grapheme_indices(true).next_back()
        else {
            return false;
        };
        self.remove_range(previous..self.cursor);
        true
    }

    pub(super) fn delete(&mut self) -> bool {
        if let Some(index) = self
            .images
            .iter()
            .position(|image| image.range.start <= self.cursor && self.cursor < image.range.end)
        {
            let range = self.images.remove(index).range;
            self.remove_range(range);
            return true;
        }
        let Some(next) = self.text[self.cursor..].graphemes(true).next() else {
            return false;
        };
        self.remove_range(self.cursor..self.cursor + next.len());
        true
    }

    /// Deletes the whitespace-delimited word before the cursor and any
    /// whitespace between it and the cursor.
    pub(super) fn delete_word_before_cursor(&mut self) -> bool {
        let mut start = self.cursor;
        while let Some((index, character)) = self.text[..start].char_indices().next_back() {
            if !character.is_whitespace() {
                break;
            }
            start = index;
        }
        while let Some((index, character)) = self.text[..start].char_indices().next_back() {
            if character.is_whitespace() {
                break;
            }
            start = index;
        }
        let end = self.cursor;
        if let Some(image_start) = self
            .images
            .iter()
            .filter(|image| image.range.start < end && start < image.range.end)
            .map(|image| image.range.start)
            .min()
        {
            start = image_start;
        }
        self.remove_with_images(start..end)
    }

    /// Deletes to the end of the logical line, or the line break itself when
    /// the cursor is already there.
    pub(super) fn delete_to_logical_line_end(&mut self) -> bool {
        let end = self.logical_line_end();
        let end = if end == self.cursor && end < self.text.len() {
            end + '\n'.len_utf8()
        } else {
            end
        };
        self.remove_with_images(self.cursor..end)
    }

    /// The visual layout at `width`, cached until the text, cursor, or width
    /// changes.
    pub(super) fn visual_layout(&mut self, width: usize) -> &VisualLayout {
        let width = width.max(1);
        let stale = self
            .layout
            .as_ref()
            .is_some_and(|cached| cached.width != width || cached.cursor != self.cursor);
        if stale {
            self.layout = None;
        }
        let cursor = self.cursor;
        let text = &self.text;
        &self
            .layout
            .get_or_insert_with(|| CachedLayout {
                width,
                cursor,
                value: VisualLayout::new(text, cursor, width),
            })
            .value
    }

    /// The layout computed by the last [Self::visual_layout] call.
    pub(super) fn cached_layout(&self) -> Option<&VisualLayout> {
        self.layout.as_ref().map(|cached| &cached.value)
    }

    /// Draws the text in `range` at `position`, with image markers in blue.
    pub(super) fn render_line(
        &self,
        buffer: &mut Buffer,
        position: Position,
        range: Range<usize>,
        width: usize,
        theme: &Theme,
    ) {
        let rendered = sanitize_terminal_text(&self.text[range.clone()]);
        buffer.set_stringn(
            position.x,
            position.y,
            rendered,
            width,
            Style::default().fg(theme.text()),
        );
        for image in &self.images {
            let start = image.range.start.max(range.start);
            let end = image.range.end.min(range.end);
            if start >= end {
                continue;
            }
            let offset = terminal_text_width(&self.text[range.start..start]);
            buffer.set_stringn(
                position
                    .x
                    .saturating_add(u16::try_from(offset).unwrap_or(u16::MAX)),
                position.y,
                &self.text[start..end],
                width.saturating_sub(offset),
                Style::default().fg(Color::Blue),
            );
        }
    }

    /// Highlights the part of `selected` that falls on the visual line `line`.
    pub(super) fn render_selection(
        &self,
        buffer: &mut Buffer,
        position: Position,
        line: Range<usize>,
        selected: Range<usize>,
        width: usize,
    ) {
        let start = selected.start.max(line.start);
        let end = selected.end.min(line.end);
        if start >= end {
            return;
        }
        let (Some(prefix), Some(text)) =
            (self.text.get(line.start..start), self.text.get(start..end))
        else {
            return;
        };
        let offset = terminal_text_width(prefix);
        let selected_width = terminal_text_width(text).min(width.saturating_sub(offset));
        if selected_width == 0 {
            return;
        }
        let x = position
            .x
            .saturating_add(u16::try_from(offset).unwrap_or(u16::MAX));
        let width = u16::try_from(selected_width).unwrap_or(u16::MAX);
        buffer.set_style(
            Rect::new(x, position.y, width, 1),
            Style::reset().fg(Color::Black).bg(Color::Yellow),
        );
    }

    fn logical_line_end(&self) -> usize {
        self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |offset| self.cursor + offset)
    }

    fn move_to(&mut self, target: usize) -> bool {
        if target == self.cursor {
            return false;
        }
        self.cursor = target;
        self.preferred_column = None;
        true
    }

    /// Removes `range` together with every image it overlaps.
    fn remove_with_images(&mut self, range: Range<usize>) -> bool {
        if range.is_empty() {
            return false;
        }
        self.images
            .retain(|image| image.range.end <= range.start || image.range.start >= range.end);
        self.remove_range(range);
        true
    }

    /// Moves a position strictly inside a marker to the marker's edge in the
    /// direction of travel.
    fn snap_out_of_image(&self, position: usize, direction: Direction) -> usize {
        self.images
            .iter()
            .find(|image| image.range.start < position && position < image.range.end)
            .map_or(position, |image| match direction {
                Direction::Backward => image.range.start,
                Direction::Forward => image.range.end,
            })
    }

    fn move_cursor_out_of_image(&mut self) {
        self.cursor = self.snap_out_of_image(self.cursor, Direction::Forward);
    }

    #[cfg(test)]
    pub(super) fn set_cursor(&mut self, cursor: usize) {
        self.cursor = cursor;
    }

    #[cfg(test)]
    pub(super) fn image_ranges(&self) -> Vec<Range<usize>> {
        self.images
            .iter()
            .map(|image| image.range.clone())
            .collect()
    }
}
