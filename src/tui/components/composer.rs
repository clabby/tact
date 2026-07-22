//! Multiline prompt editing and Pi-style composer rendering.

mod history;
mod layout;

use super::{
    node::{Component, ComponentUpdate, RenderRequest},
    waved_text::WavedText,
};
use crate::{
    config::ReasoningEffort,
    tui::{format::shorten_home, prompt::Submission, theme::Theme},
};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use history::PromptHistory;
use layout::{VisualLayout, byte_at_column};
use nanocodex::MODEL;
use ratatui::{
    Frame,
    buffer::Buffer,
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
};
use std::{ops::Range, path::Path, time::Instant};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const MODEL_WINDOW_TOKENS: u64 = 272_000;
const MIN_CONTENT_ROWS: usize = 3;
const MAX_CONTENT_ROWS: usize = 6;
const ENTRY_HINT: &str = " / actions · @ files ";

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ComposerEffect {
    Submit(Submission),
    RunShell(String),
    OpenDraftEditor,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ComposerChromeTarget {
    Effort,
    Subagents,
}

pub(crate) enum ComposerEvent {
    Terminal(Event),
    PasteImage(String),
    ContextTokens(u64),
    Insert(String),
    ReplaceDraft(String),
    SetEffort(ReasoningEffort),
    Activity {
        active: bool,
        status: Option<String>,
        now: Instant,
    },
    ActiveSubagents {
        count: usize,
        now: Instant,
    },
    AnimationFrame(Instant),
}

pub(crate) struct Composer {
    draft: String,
    images: Vec<PastedImage>,
    next_image: u64,
    cursor: usize,
    preferred_column: Option<usize>,
    scroll: usize,
    last_width: usize,
    context_tokens: u64,
    workspace: String,
    thinking: ReasoningEffort,
    activity_wave: Option<WavedText>,
    activity_status: Option<String>,
    active_subagents: usize,
    subagent_wave: Option<WavedText>,
    effort_hit_area: Option<Rect>,
    subagent_hit_area: Option<Rect>,
    layout: Option<CachedLayout>,
    history: PromptHistory,
}

struct PastedImage {
    range: Range<usize>,
    data_url: String,
}

struct CachedLayout {
    width: usize,
    cursor: usize,
    value: VisualLayout,
}

pub(crate) struct ComposerUpdate {
    pub(crate) effect: Option<ComposerEffect>,
    pub(crate) changed: bool,
}

impl Composer {
    pub(crate) fn new(workspace: &Path, thinking: ReasoningEffort) -> Self {
        Self {
            draft: String::new(),
            images: Vec::new(),
            next_image: 1,
            cursor: 0,
            preferred_column: None,
            scroll: 0,
            last_width: 78,
            context_tokens: 0,
            workspace: shorten_home(workspace),
            thinking,
            activity_wave: None,
            activity_status: None,
            active_subagents: 0,
            subagent_wave: None,
            effort_hit_area: None,
            subagent_hit_area: None,
            layout: None,
            history: PromptHistory::default(),
        }
    }

    pub(crate) fn update(&mut self, event: ComposerEvent) -> ComposerUpdate {
        match event {
            ComposerEvent::Terminal(Event::Key(key)) => self.handle_key(key),
            ComposerEvent::Terminal(Event::Paste(text)) => {
                self.history.detach();
                self.insert(&text);
                ComposerUpdate::changed()
            }
            ComposerEvent::Terminal(_) => ComposerUpdate::unchanged(),
            ComposerEvent::PasteImage(data_url) => {
                self.history.detach();
                self.insert_image(data_url);
                ComposerUpdate::changed()
            }
            ComposerEvent::ContextTokens(tokens) => {
                if self.context_tokens == tokens {
                    return ComposerUpdate::unchanged();
                }
                self.context_tokens = tokens;
                ComposerUpdate::changed()
            }
            ComposerEvent::Insert(text) => {
                self.history.detach();
                self.insert(&text);
                ComposerUpdate::changed()
            }
            ComposerEvent::ReplaceDraft(draft) => {
                self.history.detach();
                self.replace_draft(draft);
                ComposerUpdate::changed()
            }
            ComposerEvent::SetEffort(effort) => {
                if self.thinking == effort {
                    return ComposerUpdate::unchanged();
                }
                self.thinking = effort;
                ComposerUpdate::changed()
            }
            ComposerEvent::Activity {
                active,
                status,
                now,
            } => {
                let status = if active { status } else { None };
                if self.activity_status == status {
                    return ComposerUpdate::unchanged();
                }
                self.activity_wave = status.as_ref().map(|status| {
                    let mut wave = WavedText::new(status, Color::Cyan);
                    wave.set_active(true, now);
                    wave
                });
                self.activity_status = status;
                ComposerUpdate::changed()
            }
            ComposerEvent::ActiveSubagents { count, now } => {
                if self.active_subagents == count {
                    return ComposerUpdate::unchanged();
                }
                self.active_subagents = count;
                self.subagent_wave = (count > 0).then(|| {
                    let mut wave = WavedText::new(format!("{count} subagents"), Color::Yellow);
                    wave.set_active(true, now);
                    wave
                });
                ComposerUpdate::changed()
            }
            ComposerEvent::AnimationFrame(now) => {
                let activity_changed = self
                    .activity_wave
                    .as_mut()
                    .is_some_and(|wave| wave.advance(now));
                let subagent_changed = self
                    .subagent_wave
                    .as_mut()
                    .is_some_and(|wave| wave.advance(now));
                ComposerUpdate::from_change(activity_changed || subagent_changed)
            }
        }
    }

    pub(super) fn chrome_target(&self, position: Position) -> Option<ComposerChromeTarget> {
        if self
            .subagent_hit_area
            .is_some_and(|area| area.contains(position))
        {
            return Some(ComposerChromeTarget::Subagents);
        }
        self.effort_hit_area
            .is_some_and(|area| area.contains(position))
            .then_some(ComposerChromeTarget::Effort)
    }

    pub(crate) fn animation_deadline(&self) -> Option<Instant> {
        self.activity_wave
            .as_ref()
            .and_then(WavedText::animation_deadline)
            .into_iter()
            .chain(
                self.subagent_wave
                    .as_ref()
                    .and_then(WavedText::animation_deadline),
            )
            .min()
    }

    pub(crate) fn desired_height(&mut self, width: u16) -> u16 {
        if width < 2 {
            return 1;
        }

        let content_width = usize::from(width.saturating_sub(2)).max(1);
        let rows = self
            .visual_layout(content_width)
            .lines
            .len()
            .clamp(MIN_CONTENT_ROWS, MAX_CONTENT_ROWS);
        u16::try_from(rows + 2).unwrap_or(u16::MAX)
    }

    pub(crate) fn render(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        self.render_focused(frame, area, theme, true);
    }

    pub(crate) fn render_focused(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        theme: &Theme,
        focused: bool,
    ) {
        if area.is_empty() {
            return;
        }
        if area.width < 2 || area.height < 3 {
            self.render_narrow(frame, area, theme, focused);
            return;
        }

        let content_width = usize::from(area.width - 2).max(1);
        self.last_width = content_width;
        let (cursor_row, cursor_column, line_count) = {
            let layout = self.visual_layout(content_width);
            (layout.cursor_row, layout.cursor_column, layout.lines.len())
        };
        let visible_rows = usize::from(area.height - 2);
        self.keep_cursor_visible(cursor_row, visible_rows, line_count);

        let buffer = frame.buffer_mut();
        buffer.set_style(area, Style::default().fg(theme.text()));
        self.render_chrome(buffer, area, theme);
        let border = self.border_style(theme);

        for row in 0..visible_rows {
            let y = area.y + 1 + u16::try_from(row).unwrap_or(u16::MAX);
            draw_symbol(buffer, area.x, y, "│", border);
            draw_symbol(buffer, area.right() - 1, y, "│", border);

            let Some(line) = self
                .layout
                .as_ref()
                .and_then(|cached| cached.value.lines.get(self.scroll + row))
            else {
                continue;
            };
            render_draft_line(
                buffer,
                Position::new(area.x + 1, y),
                &self.draft,
                &self.images,
                line.start..line.end,
                content_width,
                theme,
            );
        }

        let cursor_row = cursor_row.saturating_sub(self.scroll);
        let cursor_x = area.x + 1 + u16::try_from(cursor_column).unwrap_or(u16::MAX);
        let cursor_y = area.y + 1 + u16::try_from(cursor_row).unwrap_or(u16::MAX);
        let max_cursor_x = area.right().saturating_sub(2);
        if focused {
            frame.set_cursor_position(Position::new(cursor_x.min(max_cursor_x), cursor_y));
        }
    }

    pub(crate) fn draft(&self) -> &str {
        &self.draft
    }

    pub(crate) const fn effort(&self) -> ReasoningEffort {
        self.thinking
    }

    #[cfg(test)]
    const fn cursor(&self) -> usize {
        self.cursor
    }

    pub(crate) fn replace_draft(&mut self, draft: String) {
        self.draft = draft;
        self.images.clear();
        self.next_image = 1;
        self.cursor = self.draft.len();
        self.preferred_column = None;
        self.scroll = 0;
        self.layout = None;
    }

    fn handle_key(&mut self, key: KeyEvent) -> ComposerUpdate {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return ComposerUpdate::unchanged();
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('g') => {
                    return ComposerUpdate::effect(ComposerEffect::OpenDraftEditor, false);
                }
                KeyCode::Char('j') => {
                    self.history.detach();
                    self.insert("\n");
                    return ComposerUpdate::changed();
                }
                _ => return ComposerUpdate::unchanged(),
            }
        }

        let detaches_history = matches!(
            key.code,
            KeyCode::Char(_)
                | KeyCode::Left
                | KeyCode::Right
                | KeyCode::Home
                | KeyCode::End
                | KeyCode::Backspace
                | KeyCode::Delete
        ) || key.code == KeyCode::Enter
            && key.modifiers.contains(KeyModifiers::SHIFT);
        if detaches_history {
            self.history.detach();
        }

        match key.code {
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.insert("\n");
                ComposerUpdate::changed()
            }
            KeyCode::Enter => self.submit(),
            KeyCode::Char(character) => {
                self.insert(&character.to_string());
                ComposerUpdate::changed()
            }
            KeyCode::Left => ComposerUpdate::from_change(self.move_left()),
            KeyCode::Right => ComposerUpdate::from_change(self.move_right()),
            KeyCode::Up => ComposerUpdate::from_change(self.move_up()),
            KeyCode::Down => ComposerUpdate::from_change(self.move_down()),
            KeyCode::Home => ComposerUpdate::from_change(self.move_to_visual_edge(false)),
            KeyCode::End => ComposerUpdate::from_change(self.move_to_visual_edge(true)),
            KeyCode::Backspace => ComposerUpdate::from_change(self.backspace()),
            KeyCode::Delete => ComposerUpdate::from_change(self.delete()),
            _ => ComposerUpdate::unchanged(),
        }
    }

    fn submit(&mut self) -> ComposerUpdate {
        let trimmed = self.draft.trim();
        if trimmed.is_empty() {
            return ComposerUpdate::unchanged();
        }

        let start = self.draft.len() - self.draft.trim_start().len();
        let end = start + trimmed.len();

        if self.images.is_empty() && self.draft.starts_with('!') {
            let command = trimmed.trim_start_matches('!').trim().to_owned();
            if command.is_empty() {
                return ComposerUpdate::unchanged();
            }
            self.history.record(format!("!{command}"));
            self.replace_draft(String::new());
            return ComposerUpdate::effect(ComposerEffect::RunShell(command), true);
        }

        let text = trimmed.to_owned();
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
        let prompt = Submission::multimodal(text.clone(), images);
        self.history.record(text);
        self.replace_draft(String::new());
        ComposerUpdate::effect(ComposerEffect::Submit(prompt), true)
    }

    fn move_up(&mut self) -> bool {
        if !self.history.is_browsing() && self.move_vertical(-1) {
            return true;
        }

        let Some(prompt) = self.history.previous(&self.draft) else {
            return false;
        };
        self.replace_draft(prompt);
        true
    }

    fn move_down(&mut self) -> bool {
        if !self.history.is_browsing() {
            return self.move_vertical(1);
        }

        let Some(prompt) = self.history.next() else {
            return false;
        };
        self.replace_draft(prompt);
        true
    }

    fn insert(&mut self, text: &str) {
        self.move_cursor_out_of_image();
        for image in &mut self.images {
            if image.range.start >= self.cursor {
                image.range.start += text.len();
                image.range.end += text.len();
            }
        }
        self.draft.insert_str(self.cursor, text);
        self.cursor += text.len();
        self.preferred_column = None;
        self.layout = None;
    }

    fn move_left(&mut self) -> bool {
        let Some(previous) = self.draft[..self.cursor].grapheme_indices(true).next_back() else {
            return false;
        };
        self.cursor = self
            .images
            .iter()
            .find(|image| image.range.contains(&previous.0))
            .map_or(previous.0, |image| image.range.start);
        self.preferred_column = None;
        true
    }

    fn move_right(&mut self) -> bool {
        let Some(next) = self.draft[self.cursor..].graphemes(true).next() else {
            return false;
        };
        let target = self.cursor + next.len();
        self.cursor = self
            .images
            .iter()
            .find(|image| image.range.start < target && target < image.range.end)
            .map_or(target, |image| image.range.end);
        self.preferred_column = None;
        true
    }

    fn backspace(&mut self) -> bool {
        if let Some(index) = self
            .images
            .iter()
            .position(|image| image.range.start < self.cursor && self.cursor <= image.range.end)
        {
            let range = self.images.remove(index).range;
            self.remove_range(range);
            return true;
        }
        let Some(previous) = self.draft[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map(|(index, _)| index)
        else {
            return false;
        };
        self.remove_range(previous..self.cursor);
        true
    }

    fn delete(&mut self) -> bool {
        if let Some(index) = self
            .images
            .iter()
            .position(|image| image.range.start <= self.cursor && self.cursor < image.range.end)
        {
            let range = self.images.remove(index).range;
            self.remove_range(range);
            return true;
        }
        let Some(next) = self.draft[self.cursor..].graphemes(true).next() else {
            return false;
        };
        self.remove_range(self.cursor..self.cursor + next.len());
        true
    }

    fn insert_image(&mut self, data_url: String) {
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

    fn move_cursor_out_of_image(&mut self) {
        if let Some(image) = self
            .images
            .iter()
            .find(|image| image.range.start < self.cursor && self.cursor < image.range.end)
        {
            self.cursor = image.range.end;
        }
    }

    fn remove_range(&mut self, range: Range<usize>) {
        let removed = range.len();
        self.draft.drain(range.clone());
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

    fn move_vertical(&mut self, direction: isize) -> bool {
        let layout = VisualLayout::new(&self.draft, self.cursor, self.last_width.max(1));
        let target_row = layout.cursor_row.saturating_add_signed(direction);
        if target_row == layout.cursor_row || target_row >= layout.lines.len() {
            return false;
        }

        let desired = *self.preferred_column.get_or_insert(layout.cursor_column);
        self.cursor = byte_at_column(&self.draft, &layout.lines[target_row], desired);
        true
    }

    fn move_to_visual_edge(&mut self, end: bool) -> bool {
        let layout = VisualLayout::new(&self.draft, self.cursor, self.last_width.max(1));
        let line = &layout.lines[layout.cursor_row];
        let target = if end { line.end } else { line.start };
        if target == self.cursor {
            return false;
        }
        self.cursor = target;
        self.preferred_column = None;
        true
    }

    fn keep_cursor_visible(&mut self, cursor_row: usize, visible: usize, line_count: usize) {
        if visible == 0 {
            self.scroll = 0;
            return;
        }
        if cursor_row < self.scroll {
            self.scroll = cursor_row;
        } else if cursor_row >= self.scroll + visible {
            self.scroll = cursor_row + 1 - visible;
        }

        self.scroll = self.scroll.min(line_count.saturating_sub(visible));
    }

    fn visual_layout(&mut self, width: usize) -> &VisualLayout {
        let stale = self
            .layout
            .as_ref()
            .is_some_and(|cached| cached.width != width || cached.cursor != self.cursor);
        if stale {
            self.layout = None;
        }

        let cursor = self.cursor;
        let draft = &self.draft;
        let cached = self.layout.get_or_insert_with(|| CachedLayout {
            width,
            cursor,
            value: VisualLayout::new(draft, cursor, width),
        });
        &cached.value
    }

    fn render_narrow(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme, focused: bool) {
        let buffer = frame.buffer_mut();
        buffer.set_style(area, Style::default().fg(theme.text()));
        render_draft_line(
            buffer,
            Position::new(area.x, area.y),
            &self.draft,
            &self.images,
            0..self.draft.len(),
            usize::from(area.width),
            theme,
        );
        if focused {
            frame.set_cursor_position(Position::new(area.x, area.y));
        }
    }

    fn render_chrome(&mut self, buffer: &mut Buffer, area: Rect, theme: &Theme) {
        self.effort_hit_area = None;
        self.subagent_hit_area = None;
        let shell_mode = self.draft.starts_with('!');
        let border = self.border_style(theme);
        let top = area.y;
        let bottom = area.bottom() - 1;

        for x in area.x..area.right() {
            draw_symbol(buffer, x, top, "─", border);
            draw_symbol(buffer, x, bottom, "─", border);
        }
        draw_symbol(buffer, area.x, top, "╭", border);
        draw_symbol(buffer, area.right() - 1, top, "╮", border);
        draw_symbol(buffer, area.x, bottom, "╰", border);
        draw_symbol(buffer, area.right() - 1, bottom, "╯", border);

        if area.width < 4 {
            return;
        }

        let content_start = area.x + 2;
        let content_width = usize::from(area.width - 4);
        let content_end = content_start + u16::try_from(content_width).unwrap_or(u16::MAX);
        let usage_prefix = format!(" {}%/272k ", context_percent(self.context_tokens));
        let status_segment = self.activity_status.clone().unwrap_or_default();
        let subagent_segment = if self.active_subagents > 0 {
            format!(" {} subagents", self.active_subagents)
        } else {
            String::new()
        };
        let usage_before_subagents = self.activity_wave.as_ref().map_or_else(
            || usage_prefix.clone(),
            |_| format!("{usage_prefix}{status_segment} "),
        );
        let usage = if subagent_segment.is_empty() {
            usage_before_subagents.clone()
        } else {
            format!("{usage_before_subagents}{} ", subagent_segment.trim_start())
        };
        let model = format!(" {MODEL} ");
        let effort = format!(" {} ", self.thinking.as_str());
        let shell = shell_mode.then_some(" shell ");
        let right_width = model.width() + effort.width() + shell.map_or(0, UnicodeWidthStr::width);
        let right_start = content_start
            + u16::try_from(content_width.saturating_sub(right_width)).unwrap_or(u16::MAX);

        let usage_space = usize::from(right_start.saturating_sub(content_start)).saturating_sub(1);
        buffer.set_stringn(
            content_start,
            top,
            usage,
            usage_space,
            Style::default().fg(theme.muted()),
        );
        if let Some(wave) = &self.activity_wave {
            let mut x = content_start + u16::try_from(usage_prefix.width()).unwrap_or(u16::MAX);
            for span in wave.spans() {
                if x >= right_start {
                    break;
                }
                let width = u16::try_from(span.width()).unwrap_or(u16::MAX);
                buffer.set_span(x, top, &span, right_start.saturating_sub(x));
                x = x.saturating_add(width);
            }
        }
        if let Some(wave) = &self.subagent_wave {
            let wave_x =
                content_start + u16::try_from(usage_before_subagents.width()).unwrap_or(u16::MAX);
            let wave_width =
                u16::try_from(wave.spans().iter().map(|span| span.width()).sum::<usize>())
                    .unwrap_or(u16::MAX)
                    .min(right_start.saturating_sub(wave_x));
            if wave_width > 0 {
                self.subagent_hit_area = Some(Rect::new(wave_x, top, wave_width, 1));
            }
            let mut x = wave_x;
            for span in wave.spans() {
                if x >= right_start {
                    break;
                }
                let width = u16::try_from(span.width()).unwrap_or(u16::MAX);
                buffer.set_span(x, top, &span, right_start.saturating_sub(x));
                x = x.saturating_add(width);
            }
        }
        buffer.set_stringn(
            right_start,
            top,
            &model,
            usize::from(content_end.saturating_sub(right_start)),
            Style::default().fg(theme.accent()),
        );
        let effort_start = right_start + u16::try_from(model.width()).unwrap_or(u16::MAX);
        if effort_start < content_end {
            self.effort_hit_area = Some(Rect::new(
                effort_start,
                top,
                u16::try_from(effort.width())
                    .unwrap_or(u16::MAX)
                    .min(content_end.saturating_sub(effort_start)),
                1,
            ));
            buffer.set_stringn(
                effort_start,
                top,
                &effort,
                usize::from(content_end - effort_start),
                Style::default()
                    .fg(theme.effort(self.thinking))
                    .add_modifier(Modifier::BOLD),
            );
        }
        if let Some(shell) = shell {
            let shell_start = effort_start + u16::try_from(effort.width()).unwrap_or(u16::MAX);
            if shell_start < content_end {
                buffer.set_stringn(
                    shell_start,
                    top,
                    shell,
                    usize::from(content_end - shell_start),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                );
            }
        }

        let directory = format!(" {} ", self.workspace);
        let directory_width = directory.width().min(content_width);
        let directory_start =
            content_end.saturating_sub(u16::try_from(directory_width).unwrap_or(u16::MAX));
        let hint_space = usize::from(directory_start.saturating_sub(content_start));
        if self.draft.is_empty() && ENTRY_HINT.width() <= hint_space {
            buffer.set_stringn(
                content_start,
                bottom,
                ENTRY_HINT,
                hint_space,
                Style::default()
                    .fg(theme.muted())
                    .add_modifier(Modifier::DIM),
            );
        }
        buffer.set_stringn(
            directory_start,
            bottom,
            directory,
            directory_width,
            Style::default().fg(theme.muted()),
        );
    }

    fn border_style(&self, theme: &Theme) -> Style {
        Style::default().fg(if self.draft.starts_with('!') {
            Color::Yellow
        } else {
            theme.border()
        })
    }
}

impl Component for Composer {
    type Event = ComposerEvent;
    type Effect = ComposerEffect;

    fn update(&mut self, event: Self::Event) -> ComponentUpdate<Self::Effect> {
        let update = Composer::update(self, event);
        ComponentUpdate {
            effects: update.effect.into_iter().collect(),
            render: if update.changed {
                RenderRequest::Immediate
            } else {
                RenderRequest::None
            },
        }
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        Composer::render(self, frame, area, theme);
    }
}

impl ComposerUpdate {
    fn unchanged() -> Self {
        Self {
            effect: None,
            changed: false,
        }
    }

    fn changed() -> Self {
        Self {
            effect: None,
            changed: true,
        }
    }

    fn effect(effect: ComposerEffect, changed: bool) -> Self {
        Self {
            effect: Some(effect),
            changed,
        }
    }

    fn from_change(changed: bool) -> Self {
        Self {
            effect: None,
            changed,
        }
    }
}

fn context_percent(tokens: u64) -> u64 {
    tokens
        .saturating_mul(100)
        .saturating_add(MODEL_WINDOW_TOKENS / 2)
        / MODEL_WINDOW_TOKENS
}

fn draw_symbol(buffer: &mut Buffer, x: u16, y: u16, symbol: &str, style: Style) {
    buffer[(x, y)].set_symbol(symbol).set_style(style);
}

fn render_draft_line(
    buffer: &mut Buffer,
    position: Position,
    draft: &str,
    images: &[PastedImage],
    range: Range<usize>,
    width: usize,
    theme: &Theme,
) {
    buffer.set_stringn(
        position.x,
        position.y,
        &draft[range.clone()],
        width,
        Style::default().fg(theme.text()),
    );
    for image in images {
        let start = image.range.start.max(range.start);
        let end = image.range.end.min(range.end);
        if start >= end {
            continue;
        }
        let offset = draft[range.start..start].width();
        buffer.set_stringn(
            position
                .x
                .saturating_add(u16::try_from(offset).unwrap_or(u16::MAX)),
            position.y,
            &draft[start..end],
            width.saturating_sub(offset),
            Style::default().fg(Color::Blue),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{Composer, ComposerEffect, ComposerEvent, context_percent};
    use crate::{config::ReasoningEffort, tui::theme::Theme};
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use nanocodex::{PromptInput, UserInput};
    use ratatui::{Terminal, backend::TestBackend, layout::Position, style::Color};
    use std::{path::Path, time::Instant};

    fn key(code: KeyCode, modifiers: KeyModifiers) -> ComposerEvent {
        ComposerEvent::Terminal(Event::Key(KeyEvent::new(code, modifiers)))
    }

    fn render(composer: &mut Composer, width: u16, height: u16) -> Terminal<TestBackend> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| composer.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        terminal
    }

    fn rows(terminal: &Terminal<TestBackend>) -> Vec<String> {
        let buffer = terminal.backend().buffer();
        buffer
            .content
            .chunks(usize::from(buffer.area.width))
            .map(|cells| cells.iter().map(|cell| cell.symbol()).collect())
            .collect()
    }

    #[test]
    fn empty_composer_matches_the_pi_chrome() {
        let mut composer = Composer::new(Path::new("/work"), ReasoningEffort::Medium);
        let terminal = render(&mut composer, 60, 5);

        assert_eq!(
            rows(&terminal),
            [
                "╭─ 0%/272k ────────────────────────── gpt-5.6-sol  medium ─╮",
                "│                                                          │",
                "│                                                          │",
                "│                                                          │",
                "╰─ / actions · @ files ──────────────────────────── /work ─╯",
            ]
        );
    }

    #[test]
    fn entry_hint_is_only_shown_for_an_empty_draft_with_room() {
        let mut composer = Composer::new(Path::new("/work"), ReasoningEffort::Medium);
        assert!(rows(&render(&mut composer, 40, 5))[4].contains("/ actions · @ files"));

        composer.replace_draft("hello".to_owned());
        assert!(!rows(&render(&mut composer, 40, 5))[4].contains("/ actions"));

        composer.replace_draft(String::new());
        assert!(!rows(&render(&mut composer, 20, 5))[4].contains("/ actions"));
    }

    #[test]
    fn active_turn_waves_the_transient_status_after_context_usage() {
        let mut composer = Composer::new(Path::new("/work"), ReasoningEffort::Medium);
        composer.update(ComposerEvent::Activity {
            active: true,
            status: Some("Running exec command…".to_owned()),
            now: Instant::now(),
        });

        let terminal = render(&mut composer, 60, 5);

        assert!(rows(&terminal)[0].contains("0%/272k Running exec command…"));
        assert!(
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .filter(|cell| "Runningexeccommand…".contains(cell.symbol()))
                .any(|cell| cell.fg == Color::Cyan)
        );
        assert!(composer.animation_deadline().is_some());
    }

    #[test]
    fn active_subagents_wave_after_the_transient_status() {
        let mut composer = Composer::new(Path::new("/work"), ReasoningEffort::Medium);
        let now = Instant::now();
        composer.update(ComposerEvent::Activity {
            active: true,
            status: Some("Thinking…".to_owned()),
            now,
        });
        composer.update(ComposerEvent::ActiveSubagents { count: 2, now });

        let terminal = render(&mut composer, 72, 5);
        let top = rows(&terminal)[0].clone();

        assert!(top.contains("Thinking… 2 subagents"));
        assert!(
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .filter(|cell| "2subagents".contains(cell.symbol()))
                .any(|cell| cell.fg == Color::Yellow)
        );
        assert!(composer.animation_deadline().is_some());
    }

    #[test]
    fn composer_grows_from_three_through_six_rows() {
        let mut composer = Composer::new(Path::new("/work"), ReasoningEffort::Medium);
        assert_eq!(composer.desired_height(20), 5);

        composer.replace_draft("1\n2\n3\n4\n5\n6".to_owned());
        assert_eq!(composer.desired_height(20), 8);

        composer.replace_draft("1\n2\n3\n4\n5\n6\n7".to_owned());
        assert_eq!(composer.desired_height(20), 8);
    }

    #[test]
    fn overflow_scrolls_to_keep_the_cursor_visible() {
        let mut composer = Composer::new(Path::new("/work"), ReasoningEffort::Medium);
        composer.replace_draft("one\ntwo\nthree\nfour\nfive\nsix\nseven".to_owned());
        let terminal = render(&mut composer, 30, 8);
        let rows = rows(&terminal);

        assert!(rows[1].contains("two"));
        assert!(rows[6].contains("seven"));
        assert!(!rows.iter().any(|row| row.contains("one")));
    }

    #[test]
    fn resize_reflows_wrapped_text() {
        let mut composer = Composer::new(Path::new("/work"), ReasoningEffort::Medium);
        composer.replace_draft("alpha beta gamma delta".to_owned());

        render(&mut composer, 14, 5);
        assert_eq!(composer.desired_height(14), 5);
        assert_eq!(composer.last_width, 12);

        render(&mut composer, 8, 6);
        assert_eq!(composer.desired_height(8), 6);
        assert_eq!(composer.last_width, 6);
    }

    #[test]
    fn cursor_movement_respects_graphemes_and_display_width() {
        let mut composer = Composer::new(Path::new("/work"), ReasoningEffort::Medium);
        composer.replace_draft("a界e\u{301}".to_owned());
        composer.update(key(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(composer.cursor(), 4);
        composer.update(key(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(composer.cursor(), 1);

        let terminal = render(&mut composer, 20, 5);
        assert_eq!(terminal.backend().cursor_position(), Position::new(2, 1));
    }

    #[test]
    fn paste_and_editor_replacement_preserve_multiline_text() {
        let mut composer = Composer::new(Path::new("/work"), ReasoningEffort::Medium);
        composer.update(ComposerEvent::Terminal(Event::Paste("one\ntwo".to_owned())));
        assert_eq!(composer.draft(), "one\ntwo");

        composer.update(ComposerEvent::ReplaceDraft("edited\ndraft".to_owned()));
        assert_eq!(composer.draft(), "edited\ndraft");
        assert_eq!(composer.cursor(), composer.draft().len());
    }

    #[test]
    fn pasted_images_render_as_numbered_blue_tokens_and_submit_as_images() {
        let mut composer = Composer::new(Path::new("/work"), ReasoningEffort::Medium);
        composer.update(ComposerEvent::Insert("inspect ".to_owned()));
        composer.update(ComposerEvent::PasteImage(
            "data:image/png;base64,first".to_owned(),
        ));
        composer.update(ComposerEvent::PasteImage(
            "data:image/png;base64,second".to_owned(),
        ));

        assert_eq!(composer.draft(), "inspect [Image #1][Image #2]");
        let terminal = render(&mut composer, 50, 5);
        let buffer = terminal.backend().buffer();
        for x in 9..29 {
            assert_eq!(buffer[(x, 1)].fg, Color::Blue);
        }

        let update = composer.update(key(KeyCode::Enter, KeyModifiers::NONE));
        let Some(ComposerEffect::Submit(submission)) = update.effect else {
            panic!("image prompt should submit");
        };
        assert_eq!(submission.display_text(), "inspect [Image #1][Image #2]");
        let PromptInput::Content(content) = submission.agent_prompt().instruction else {
            panic!("image prompt should use multimodal content");
        };
        assert!(matches!(&content[0], UserInput::Text { text } if text == "inspect "));
        assert!(
            matches!(&content[1], UserInput::Image { image_url, .. } if image_url.ends_with("first"))
        );
        assert!(
            matches!(&content[2], UserInput::Image { image_url, .. } if image_url.ends_with("second"))
        );
    }

    #[test]
    fn deleting_an_image_token_removes_its_attachment_atomically() {
        let mut composer = Composer::new(Path::new("/work"), ReasoningEffort::Medium);
        composer.update(ComposerEvent::PasteImage(
            "data:image/png;base64,removed".to_owned(),
        ));

        composer.update(key(KeyCode::Backspace, KeyModifiers::NONE));

        assert!(composer.draft().is_empty());
        assert!(composer.images.is_empty());
    }

    #[test]
    fn submission_trims_nonempty_prompts_and_preserves_empty_drafts() {
        let mut composer = Composer::new(Path::new("/work"), ReasoningEffort::Medium);
        composer.replace_draft("  inspect this  \n".to_owned());
        let update = composer.update(key(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(
            update.effect,
            Some(ComposerEffect::Submit("inspect this".to_owned().into()))
        );
        assert!(composer.draft().is_empty());

        composer.replace_draft("   \n".to_owned());
        let update = composer.update(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(update.effect, None);
        assert_eq!(composer.draft(), "   \n");
    }

    #[test]
    fn leading_bang_uses_yellow_shell_chrome_and_submits_only_the_command() {
        let mut composer = Composer::new(Path::new("/work"), ReasoningEffort::Medium);
        composer.replace_draft("!  printf hello  ".to_owned());

        let terminal = render(&mut composer, 80, 5);
        let buffer = terminal.backend().buffer();
        for position in [(0, 0), (79, 0), (0, 2), (79, 2), (0, 4), (79, 4)] {
            assert_eq!(buffer[position].fg, Color::Yellow);
        }
        assert!(rows(&terminal)[0].contains("medium  shell"));

        let update = composer.update(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            update.effect,
            Some(ComposerEffect::RunShell("printf hello".to_owned()))
        );
        assert!(composer.draft().is_empty());
    }

    #[test]
    fn bang_without_a_command_is_not_submitted() {
        let mut composer = Composer::new(Path::new("/work"), ReasoningEffort::Medium);
        composer.replace_draft("!   ".to_owned());

        let update = composer.update(key(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(update.effect, None);
        assert_eq!(composer.draft(), "!   ");
    }

    #[test]
    fn arrows_cycle_submitted_prompts_and_restore_the_unsent_draft() {
        let mut composer = Composer::new(Path::new("/work"), ReasoningEffort::Medium);
        for prompt in ["first", "second"] {
            composer.replace_draft(prompt.to_owned());
            composer.update(key(KeyCode::Enter, KeyModifiers::NONE));
        }
        composer.replace_draft("unfinished\nline".to_owned());

        composer.update(key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(composer.draft(), "unfinished\nline");
        composer.update(key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(composer.draft(), "second");
        composer.update(key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(composer.draft(), "first");
        composer.update(key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(composer.draft(), "second");
        composer.update(key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(composer.draft(), "unfinished\nline");
    }

    #[test]
    fn editing_a_recalled_prompt_detaches_it_from_history() {
        let mut composer = Composer::new(Path::new("/work"), ReasoningEffort::Medium);
        composer.replace_draft("previous".to_owned());
        composer.update(key(KeyCode::Enter, KeyModifiers::NONE));

        composer.update(key(KeyCode::Up, KeyModifiers::NONE));
        composer.update(key(KeyCode::Char('!'), KeyModifiers::NONE));
        composer.update(key(KeyCode::Down, KeyModifiers::NONE));

        assert_eq!(composer.draft(), "previous!");
    }

    #[test]
    fn multiline_and_control_effect_keys_are_distinct() {
        let mut composer = Composer::new(Path::new("/work"), ReasoningEffort::Medium);
        composer.update(key(KeyCode::Enter, KeyModifiers::SHIFT));
        composer.update(key(KeyCode::Char('j'), KeyModifiers::CONTROL));
        assert_eq!(composer.draft(), "\n\n");

        assert_eq!(
            composer
                .update(key(KeyCode::Char('g'), KeyModifiers::CONTROL))
                .effect,
            Some(ComposerEffect::OpenDraftEditor)
        );
    }

    #[test]
    fn editing_keys_follow_visual_lines_and_grapheme_boundaries() {
        let mut composer = Composer::new(Path::new("/work"), ReasoningEffort::Medium);
        composer.replace_draft("abc\ndef".to_owned());
        render(&mut composer, 20, 5);

        composer.update(key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(composer.cursor(), 3);
        composer.update(key(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(composer.cursor(), 0);
        composer.update(key(KeyCode::Delete, KeyModifiers::NONE));
        assert_eq!(composer.draft(), "bc\ndef");
        composer.update(key(KeyCode::End, KeyModifiers::NONE));
        composer.update(key(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(composer.draft(), "b\ndef");
    }

    #[test]
    fn wrapping_prefers_words_and_hard_wraps_long_words() {
        let mut composer = Composer::new(Path::new("/work"), ReasoningEffort::Medium);
        composer.replace_draft("alpha betaabcdefgh".to_owned());
        let terminal = render(&mut composer, 8, 6);
        let rows = rows(&terminal);

        assert!(rows[1].contains("alpha"));
        assert!(rows[2].contains("betaab"));
        assert!(rows[3].contains("cdefgh"));
    }

    #[test]
    fn context_percentage_is_rounded() {
        assert_eq!(context_percent(0), 0);
        assert_eq!(context_percent(136_000), 50);
        assert_eq!(context_percent(1_400), 1);
    }

    #[test]
    fn narrow_rendering_truncates_without_panicking() {
        let mut composer = Composer::new(Path::new("/work"), ReasoningEffort::Medium);
        composer.replace_draft("abcdef".to_owned());

        let terminal = render(&mut composer, 3, 2);

        assert_eq!(rows(&terminal)[0], "abc");
    }
}
