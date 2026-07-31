//! Searchable, read-only inspection and explicit deletion of stored memories.

use super::{
    floating::Floating,
    node::{Component, ComponentUpdate, RenderRequest},
};
use crate::{
    core::extensions::memory::{MemoryKey, MemoryRecord},
    tui::{session::format_age, theme::Theme},
};
use chrono::{DateTime, Utc};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph, Wrap},
};
use std::time::{SystemTime, UNIX_EPOCH};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const LIST_KEYS: [&str; 5] = [
    "↑↓ move",
    "enter inspect",
    "f sort",
    "ctrl+d remove",
    "ctrl+r refresh · esc close",
];
const DETAIL_KEYS: [&str; 3] = ["↑↓/pgup/pgdn scroll", "d delete", "r refresh · esc back"];
const CONFIRM_KEYS: [&str; 2] = ["d/delete confirm", "esc cancel"];
const DELETING_KEYS: [&str; 1] = ["deleting…"];
const LOAD_ERROR_KEYS: [&str; 2] = ["r retry", "esc close"];
const DELETE_ERROR_KEYS: [&str; 3] = ["d/delete retry", "r refresh", "esc back"];
const LOADING_KEYS: [&str; 2] = ["r retry", "esc close"];
const FILTER_LABEL: &str = " Filter: ";
const MAX_PREVIEW_GRAPHEMES: usize = 160;
const MAX_ERROR_WIDTH: usize = 240;

pub(super) enum MemoryBrowserEvent {
    Terminal(Event),
    Loaded(Vec<MemoryRecord>),
    LoadFailed(String),
    Deleted { id: i64 },
    DeleteFailed { error: String, conflict: bool },
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum MemoryBrowserEffect {
    Dismiss,
    Refresh,
    Delete(MemoryKey),
}

pub(super) struct MemoryBrowser {
    records: Vec<MemoryRecord>,
    query: String,
    matches: Vec<usize>,
    selected_id: Option<i64>,
    sort: SortMode,
    state: BrowserState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SortMode {
    MostUseful,
    Newest,
    Oldest,
    LeastUseful,
}

impl SortMode {
    const fn next(self) -> Self {
        match self {
            Self::MostUseful => Self::Newest,
            Self::Newest => Self::Oldest,
            Self::Oldest => Self::LeastUseful,
            Self::LeastUseful => Self::MostUseful,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::MostUseful => "Most useful",
            Self::Newest => "Newest",
            Self::Oldest => "Oldest",
            Self::LeastUseful => "Least useful",
        }
    }

    fn compare(self, left: &MemoryRecord, right: &MemoryRecord) -> std::cmp::Ordering {
        match self {
            Self::MostUseful => right
                .use_count
                .cmp(&left.use_count)
                .then_with(|| compare_newest(left, right)),
            Self::Newest => compare_newest(left, right),
            Self::Oldest => compare_oldest(left, right),
            Self::LeastUseful => left
                .use_count
                .cmp(&right.use_count)
                .then_with(|| compare_oldest(left, right)),
        }
    }
}

#[derive(Clone)]
enum BrowserState {
    Loading,
    Error(BrowserError),
    List,
    Detail { id: i64, scroll: u16 },
    ConfirmDelete { id: i64, return_to: ReturnView },
    Deleting { id: i64, return_to: ReturnView },
}

#[derive(Clone)]
struct BrowserError {
    message: String,
    action: ErrorAction,
}

#[derive(Clone, Copy)]
enum ErrorAction {
    Load,
    Delete { id: i64, return_to: ReturnView },
}

#[derive(Clone, Copy)]
enum ReturnView {
    List,
    Detail { scroll: u16 },
}

struct DetailView {
    id: i64,
    scroll: u16,
    status: Option<String>,
    update_scroll: bool,
}

impl MemoryBrowser {
    pub(super) const fn new() -> Self {
        Self {
            records: Vec::new(),
            query: String::new(),
            matches: Vec::new(),
            selected_id: None,
            sort: SortMode::MostUseful,
            state: BrowserState::Loading,
        }
    }

    fn update_key(&mut self, key: KeyEvent) -> ComponentUpdate<MemoryBrowserEffect> {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return ComponentUpdate::none();
        }

        match self.state.clone() {
            BrowserState::Loading => self.update_loading(key),
            BrowserState::Error(error) => self.update_error(key, error.action),
            BrowserState::List => self.update_list(key),
            BrowserState::Detail { id, scroll } => self.update_detail(key, id, scroll),
            BrowserState::ConfirmDelete { id, return_to } => {
                self.update_confirmation(key, id, return_to)
            }
            BrowserState::Deleting { .. } => ComponentUpdate::none(),
        }
    }

    fn update_loading(&mut self, key: KeyEvent) -> ComponentUpdate<MemoryBrowserEffect> {
        match key.code {
            KeyCode::Esc => Self::effect(MemoryBrowserEffect::Dismiss),
            KeyCode::Char('r') if key.modifiers == KeyModifiers::NONE => self.refresh(),
            _ => ComponentUpdate::none(),
        }
    }

    fn update_error(
        &mut self,
        key: KeyEvent,
        action: ErrorAction,
    ) -> ComponentUpdate<MemoryBrowserEffect> {
        match (action, key.code) {
            (_, KeyCode::Char('r')) if key.modifiers == KeyModifiers::NONE => self.refresh(),
            (ErrorAction::Load, KeyCode::Esc) => Self::effect(MemoryBrowserEffect::Dismiss),
            (ErrorAction::Delete { return_to, .. }, KeyCode::Esc) => {
                self.restore(return_to);
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            (ErrorAction::Delete { id, return_to }, KeyCode::Char('d') | KeyCode::Delete)
                if key.kind == KeyEventKind::Press =>
            {
                self.delete(id, return_to)
            }
            _ => ComponentUpdate::none(),
        }
    }

    fn update_list(&mut self, key: KeyEvent) -> ComponentUpdate<MemoryBrowserEffect> {
        match key.code {
            KeyCode::Esc => Self::effect(MemoryBrowserEffect::Dismiss),
            KeyCode::Up => self.move_selection(false),
            KeyCode::Down => self.move_selection(true),
            KeyCode::Enter | KeyCode::Tab => self.inspect_selected(),
            KeyCode::Backspace if !self.query.is_empty() => {
                if let Some((index, _)) = self.query.grapheme_indices(true).next_back() {
                    self.query.truncate(index);
                    self.refresh_matches();
                }
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            KeyCode::Char('f') if key.modifiers == KeyModifiers::NONE => self.cycle_sort(),
            KeyCode::Char('r') if key.modifiers == KeyModifiers::CONTROL => self.refresh(),
            KeyCode::Char('d') if key.modifiers == KeyModifiers::CONTROL => {
                self.confirm_selected(ReturnView::List)
            }
            KeyCode::Delete if key.kind == KeyEventKind::Press => {
                self.confirm_selected(ReturnView::List)
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    && !character.is_control() =>
            {
                self.query.push(character);
                self.refresh_matches();
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            _ => ComponentUpdate::none(),
        }
    }

    fn update_detail(
        &mut self,
        key: KeyEvent,
        id: i64,
        scroll: u16,
    ) -> ComponentUpdate<MemoryBrowserEffect> {
        let next_scroll = match key.code {
            KeyCode::Up => Some(scroll.saturating_sub(1)),
            KeyCode::Down => Some(scroll.saturating_add(1)),
            KeyCode::PageUp => Some(scroll.saturating_sub(10)),
            KeyCode::PageDown => Some(scroll.saturating_add(10)),
            _ => None,
        };
        if let Some(scroll) = next_scroll {
            self.state = BrowserState::Detail { id, scroll };
            return ComponentUpdate::render(RenderRequest::Immediate);
        }

        match key.code {
            KeyCode::Esc => {
                self.state = BrowserState::List;
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            KeyCode::Char('r') if key.modifiers == KeyModifiers::NONE => self.refresh(),
            KeyCode::Char('d') | KeyCode::Delete if key.kind == KeyEventKind::Press => {
                self.state = BrowserState::ConfirmDelete {
                    id,
                    return_to: ReturnView::Detail { scroll },
                };
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            _ => ComponentUpdate::none(),
        }
    }

    fn update_confirmation(
        &mut self,
        key: KeyEvent,
        id: i64,
        return_to: ReturnView,
    ) -> ComponentUpdate<MemoryBrowserEffect> {
        match key.code {
            KeyCode::Esc => {
                self.restore(return_to);
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            KeyCode::Char('d') | KeyCode::Delete if key.kind == KeyEventKind::Press => {
                self.delete(id, return_to)
            }
            _ => ComponentUpdate::none(),
        }
    }

    fn insert_paste(&mut self, text: &str) -> ComponentUpdate<MemoryBrowserEffect> {
        if !matches!(self.state, BrowserState::List) {
            return ComponentUpdate::none();
        }
        self.query
            .extend(text.chars().filter(|character| !character.is_control()));
        self.refresh_matches();
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn refresh(&mut self) -> ComponentUpdate<MemoryBrowserEffect> {
        self.state = BrowserState::Loading;
        Self::effect(MemoryBrowserEffect::Refresh)
    }

    fn inspect_selected(&mut self) -> ComponentUpdate<MemoryBrowserEffect> {
        let Some(id) = self.selected_id else {
            return ComponentUpdate::none();
        };
        self.state = BrowserState::Detail { id, scroll: 0 };
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn confirm_selected(&mut self, return_to: ReturnView) -> ComponentUpdate<MemoryBrowserEffect> {
        let Some(id) = self.selected_id else {
            return ComponentUpdate::none();
        };
        self.state = BrowserState::ConfirmDelete { id, return_to };
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn delete(&mut self, id: i64, return_to: ReturnView) -> ComponentUpdate<MemoryBrowserEffect> {
        let Some(record) = self.records.iter().find(|record| record.key.id == id) else {
            self.state = BrowserState::List;
            self.refresh_matches();
            return ComponentUpdate::render(RenderRequest::Immediate);
        };
        let key = record.key;
        self.state = BrowserState::Deleting { id, return_to };
        Self::effect(MemoryBrowserEffect::Delete(key))
    }

    fn restore(&mut self, return_to: ReturnView) {
        self.state = match return_to {
            ReturnView::List => BrowserState::List,
            ReturnView::Detail { scroll } => {
                let Some(id) = self.selected_id else {
                    return self.state = BrowserState::List;
                };
                BrowserState::Detail { id, scroll }
            }
        };
    }

    fn replace_records(&mut self, records: Vec<MemoryRecord>) {
        let fallback = self.selected_match_index().unwrap_or_default();
        self.records = records;
        self.rebuild_matches(fallback);
        self.state = BrowserState::List;
    }

    fn remove_record(&mut self, id: i64) {
        let fallback = self.selected_match_index().unwrap_or_default();
        self.records.retain(|record| record.key.id != id);
        self.rebuild_matches(fallback);
        self.state = BrowserState::List;
    }

    fn refresh_matches(&mut self) {
        let fallback = self.selected_match_index().unwrap_or_default();
        self.rebuild_matches(fallback);
    }

    fn cycle_sort(&mut self) -> ComponentUpdate<MemoryBrowserEffect> {
        let fallback = self.selected_match_index().unwrap_or_default();
        self.sort = self.sort.next();
        self.rebuild_matches(fallback);
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn rebuild_matches(&mut self, fallback: usize) {
        let query = self.query.to_lowercase();
        self.matches = self
            .records
            .iter()
            .enumerate()
            .filter(|(_, record)| record_matches(record, &query))
            .map(|(index, _)| index)
            .collect();
        self.matches.sort_by(|left, right| {
            self.sort
                .compare(&self.records[*left], &self.records[*right])
        });

        if self.selected_match_index().is_some() {
            return;
        }
        self.selected_id = self
            .matches
            .get(fallback.min(self.matches.len().saturating_sub(1)))
            .map(|index| self.records[*index].key.id);
    }

    fn selected_match_index(&self) -> Option<usize> {
        let selected_id = self.selected_id?;
        self.matches
            .iter()
            .position(|index| self.records[*index].key.id == selected_id)
    }

    fn move_selection(&mut self, down: bool) -> ComponentUpdate<MemoryBrowserEffect> {
        if self.matches.is_empty() {
            return ComponentUpdate::none();
        }
        let current = self.selected_match_index().unwrap_or_default();
        let next = if down {
            current.saturating_add(1).min(self.matches.len() - 1)
        } else {
            current.saturating_sub(1)
        };
        self.selected_id = Some(self.records[self.matches[next]].key.id);
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn effect(effect: MemoryBrowserEffect) -> ComponentUpdate<MemoryBrowserEffect> {
        ComponentUpdate {
            effects: vec![effect],
            render: RenderRequest::Immediate,
        }
    }

    fn footer(&self) -> &'static [&'static str] {
        match &self.state {
            BrowserState::Loading => &LOADING_KEYS,
            BrowserState::Error(error) => match error.action {
                ErrorAction::Load => &LOAD_ERROR_KEYS,
                ErrorAction::Delete { .. } => &DELETE_ERROR_KEYS,
            },
            BrowserState::List => &LIST_KEYS,
            BrowserState::Detail { .. } => &DETAIL_KEYS,
            BrowserState::ConfirmDelete { .. } => &CONFIRM_KEYS,
            BrowserState::Deleting { .. } => &DELETING_KEYS,
        }
    }

    fn render_list(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        theme: &Theme,
        status: Option<String>,
    ) {
        if area.is_empty() {
            return;
        }
        let filter = Rect { height: 1, ..area };
        self.render_filter(frame, filter, theme);

        let mut list = Rect {
            y: area.y.saturating_add(1),
            height: area.height.saturating_sub(1),
            ..area
        };
        if let Some(status) = status {
            if list.is_empty() {
                return;
            }
            let status_area = Rect { height: 1, ..list };
            frame.render_widget(
                Paragraph::new(fit_width(&status, usize::from(status_area.width)))
                    .style(Style::default().fg(theme.accent())),
                status_area,
            );
            list.y = list.y.saturating_add(1);
            list.height = list.height.saturating_sub(1);
        }
        self.render_records(frame, list, theme);
    }

    fn render_filter(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        if area.is_empty() {
            return;
        }
        let sort = format!("  Sort: {}", self.sort.label());
        let query_width = usize::from(area.width)
            .saturating_sub(FILTER_LABEL.width())
            .saturating_sub(sort.width());
        let query = visible_tail(&self.query, query_width);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(FILTER_LABEL, Style::default().fg(theme.muted())),
                Span::styled(query, Style::default().fg(theme.text())),
                Span::styled(sort, Style::default().fg(theme.muted())),
            ])),
            area,
        );
    }

    fn render_records(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        if area.is_empty() {
            return;
        }
        if self.records.is_empty() {
            frame.render_widget(
                Paragraph::new(" Memory is empty. Press r to refresh.")
                    .style(Style::default().fg(theme.muted())),
                area,
            );
            return;
        }
        if self.matches.is_empty() {
            let message = format!(" No memories match “{}”.", self.query);
            frame.render_widget(
                Paragraph::new(fit_width(&message, usize::from(area.width)))
                    .style(Style::default().fg(theme.muted())),
                area,
            );
            return;
        }

        let width = usize::from(area.width).saturating_sub(2);
        let items = self.matches.iter().map(|index| {
            let record = &self.records[*index];
            let preview = bounded_preview(&record.content, width);
            let metadata = fit_width(&list_metadata(record), width);
            ListItem::new(vec![
                Line::from(Span::styled(
                    preview,
                    Style::default()
                        .fg(theme.text())
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(metadata, Style::default().fg(theme.muted()))),
            ])
        });
        let list = List::new(items)
            .highlight_symbol("› ")
            .highlight_style(Style::default().fg(theme.accent()));
        let mut state = ListState::default().with_selected(self.selected_match_index());
        frame.render_stateful_widget(list, area, &mut state);
    }

    fn render_detail(
        &mut self,
        frame: &mut Frame<'_>,
        mut area: Rect,
        theme: &Theme,
        view: DetailView,
    ) {
        let DetailView {
            id,
            scroll: requested_scroll,
            status,
            update_scroll,
        } = view;
        if area.is_empty() {
            return;
        }
        if let Some(status) = status {
            let status_area = Rect { height: 1, ..area };
            frame.render_widget(
                Paragraph::new(fit_width(&status, usize::from(status_area.width)))
                    .style(Style::default().fg(theme.accent())),
                status_area,
            );
            area.y = area.y.saturating_add(1);
            area.height = area.height.saturating_sub(1);
        }
        if area.is_empty() {
            return;
        }

        let Some(record) = self.records.iter().find(|record| record.key.id == id) else {
            self.state = BrowserState::List;
            self.render_list(frame, area, theme, None);
            return;
        };
        let lines = detail_lines(record, theme);
        let line_count = wrapped_line_count(&lines, area.width);
        let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
        let max_scroll = line_count
            .saturating_sub(usize::from(area.height))
            .min(usize::from(u16::MAX)) as u16;
        let scroll = requested_scroll.min(max_scroll);
        if update_scroll {
            self.state = BrowserState::Detail { id, scroll };
        }
        frame.render_widget(paragraph.scroll((scroll, 0)), area);
    }

    fn render_error(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme, error: &BrowserError) {
        if area.is_empty() {
            return;
        }
        let message = sanitize_single_line(&error.message, MAX_ERROR_WIDTH);
        let text = match error.action {
            ErrorAction::Load => {
                format!("Could not load memories: {message}\n\nPress r to retry or Esc to close.")
            }
            ErrorAction::Delete { id, .. } => format!(
                "Could not delete memory #{id}: {message}\n\nPress d/Delete to retry, r to reload, or Esc to return."
            ),
        };
        frame.render_widget(
            Paragraph::new(text)
                .style(Style::default().fg(theme.text()))
                .wrap(Wrap { trim: false }),
            area,
        );
    }
}

impl Component for MemoryBrowser {
    type Event = MemoryBrowserEvent;
    type Effect = MemoryBrowserEffect;

    fn update(&mut self, event: Self::Event) -> ComponentUpdate<Self::Effect> {
        match event {
            MemoryBrowserEvent::Terminal(Event::Key(key)) => self.update_key(key),
            MemoryBrowserEvent::Terminal(Event::Paste(text)) => self.insert_paste(&text),
            MemoryBrowserEvent::Terminal(_) => ComponentUpdate::none(),
            MemoryBrowserEvent::Loaded(records) => {
                self.replace_records(records);
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            MemoryBrowserEvent::LoadFailed(message) => {
                self.state = BrowserState::Error(BrowserError {
                    message,
                    action: ErrorAction::Load,
                });
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            MemoryBrowserEvent::Deleted { id } => {
                self.remove_record(id);
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            MemoryBrowserEvent::DeleteFailed { error, conflict } => {
                let BrowserState::Deleting { id, return_to } = self.state.clone() else {
                    return ComponentUpdate::none();
                };
                if conflict {
                    return self.refresh();
                }
                self.state = BrowserState::Error(BrowserError {
                    message: error,
                    action: ErrorAction::Delete { id, return_to },
                });
                ComponentUpdate::render(RenderRequest::Immediate)
            }
        }
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let state = self.state.clone();
        let layout = Floating::new("Memory", 88, 28, self.footer()).render(frame, area, theme);
        if layout.body.is_empty() {
            return;
        }

        match state {
            BrowserState::Loading => frame.render_widget(
                Paragraph::new("Loading memories…\n\nPress r to retry or Esc to close.")
                    .style(Style::default().fg(theme.muted()))
                    .wrap(Wrap { trim: false }),
                layout.body,
            ),
            BrowserState::Error(error) => {
                self.render_error(frame, layout.body, theme, &error);
            }
            BrowserState::List => self.render_list(frame, layout.body, theme, None),
            BrowserState::Detail { id, scroll } => {
                self.render_detail(
                    frame,
                    layout.body,
                    theme,
                    DetailView {
                        id,
                        scroll,
                        status: None,
                        update_scroll: true,
                    },
                );
            }
            BrowserState::ConfirmDelete { id, return_to } => {
                let status = format!(" Delete memory #{id}? Press d/Delete again to confirm.");
                match return_to {
                    ReturnView::List => {
                        self.render_list(frame, layout.body, theme, Some(status));
                    }
                    ReturnView::Detail { scroll } => self.render_detail(
                        frame,
                        layout.body,
                        theme,
                        DetailView {
                            id,
                            scroll,
                            status: Some(status),
                            update_scroll: false,
                        },
                    ),
                }
            }
            BrowserState::Deleting { id, return_to } => {
                let status = format!(" Deleting memory #{id}…");
                match return_to {
                    ReturnView::List => {
                        self.render_list(frame, layout.body, theme, Some(status));
                    }
                    ReturnView::Detail { scroll } => self.render_detail(
                        frame,
                        layout.body,
                        theme,
                        DetailView {
                            id,
                            scroll,
                            status: Some(status),
                            update_scroll: false,
                        },
                    ),
                }
            }
        }
    }
}

fn record_matches(record: &MemoryRecord, query: &str) -> bool {
    query.is_empty()
        || record.key.id.to_string().contains(query)
        || record.content.to_lowercase().contains(query)
}

fn compare_newest(left: &MemoryRecord, right: &MemoryRecord) -> std::cmp::Ordering {
    right
        .updated_at_ms
        .cmp(&left.updated_at_ms)
        .then_with(|| right.key.id.cmp(&left.key.id))
}

fn compare_oldest(left: &MemoryRecord, right: &MemoryRecord) -> std::cmp::Ordering {
    left.updated_at_ms
        .cmp(&right.updated_at_ms)
        .then_with(|| left.key.id.cmp(&right.key.id))
}

fn list_metadata(record: &MemoryRecord) -> String {
    format!(
        "#{} · v{} · updated {} · used {}× · {}",
        record.key.id,
        record.key.version,
        timestamp_age(record.updated_at_ms),
        record.use_count,
        probation_status(record.probation_until_ms),
    )
}

fn detail_lines(record: &MemoryRecord, theme: &Theme) -> Vec<Line<'static>> {
    let label = Style::default().fg(theme.muted());
    let value = Style::default().fg(theme.text());
    let heading = Style::default()
        .fg(theme.accent())
        .add_modifier(Modifier::BOLD);
    let mut lines = vec![
        Line::styled(" Memory metadata", heading),
        fact(" ID", record.key.id.to_string(), label, value),
        fact(" Version", record.key.version.to_string(), label, value),
        fact(
            " Created",
            format_timestamp(record.created_at_ms),
            label,
            value,
        ),
        fact(
            " Updated",
            format!(
                "{} ({})",
                format_timestamp(record.updated_at_ms),
                timestamp_age(record.updated_at_ms)
            ),
            label,
            value,
        ),
        fact(
            " Last scanned",
            optional_timestamp(record.last_scanned_at_ms),
            label,
            value,
        ),
        fact(" Scan count", record.scan_count.to_string(), label, value),
        fact(
            " Last used",
            optional_timestamp(record.last_used_at_ms),
            label,
            value,
        ),
        fact(" Use count", record.use_count.to_string(), label, value),
        fact(
            " Probation until",
            record
                .probation_until_ms
                .map_or_else(|| "none".to_owned(), format_timestamp),
            label,
            value,
        ),
        Line::default(),
        Line::styled(" Content", heading),
    ];
    lines.extend(
        sanitize_detail(&record.content)
            .split('\n')
            .map(|line| Line::styled(line.to_owned(), value)),
    );
    lines
}

fn fact(label_text: &'static str, value_text: String, label: Style, value: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label_text:<18}"), label),
        Span::styled(value_text, value),
    ])
}

fn optional_timestamp(timestamp_ms: Option<i64>) -> String {
    timestamp_ms.map_or_else(|| "never".to_owned(), format_timestamp)
}

fn format_timestamp(timestamp_ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(timestamp_ms).map_or_else(
        || "unknown time".to_owned(),
        |time| time.format("%Y-%m-%d %H:%M:%SZ").to_string(),
    )
}

fn timestamp_age(timestamp_ms: i64) -> String {
    format_age(u64::try_from(timestamp_ms).unwrap_or_default())
}

fn probation_status(until_ms: Option<i64>) -> String {
    let Some(until_ms) = until_ms else {
        return "no probation".to_owned();
    };
    let remaining_ms = until_ms.saturating_sub(now_unix_ms());
    if remaining_ms <= 0 {
        return "probation elapsed".to_owned();
    }
    let minutes = u64::try_from(remaining_ms).unwrap_or_default() / 60_000;
    match minutes {
        0 => "probation <1m".to_owned(),
        1..=59 => format!("probation {minutes}m"),
        60..=1_439 => format!("probation {}h", minutes / 60),
        _ => format!("probation {}d", minutes / 1_440),
    }
}

fn now_unix_ms() -> i64 {
    let milliseconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    i64::try_from(milliseconds).unwrap_or(i64::MAX)
}

fn bounded_preview(content: &str, width: usize) -> String {
    let single_line = content
        .graphemes(true)
        .take(MAX_PREVIEW_GRAPHEMES)
        .flat_map(str::chars)
        .map(|character| {
            if character.is_control() || character.is_whitespace() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let collapsed = single_line.split_whitespace().collect::<Vec<_>>().join(" ");
    fit_width(&collapsed, width)
}

fn sanitize_detail(content: &str) -> String {
    let mut sanitized = String::with_capacity(content.len());
    for character in content.chars() {
        match character {
            '\n' => sanitized.push('\n'),
            '\t' => sanitized.push_str("    "),
            character if character.is_control() => sanitized.push('�'),
            character => sanitized.push(character),
        }
    }
    sanitized
}

fn sanitize_single_line(text: &str, width: usize) -> String {
    let sanitized = text
        .chars()
        .map(|character| {
            if character.is_control() || character.is_whitespace() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let collapsed = sanitized.split_whitespace().collect::<Vec<_>>().join(" ");
    fit_width(&collapsed, width)
}

fn fit_width(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }

    let content_width = width.saturating_sub(1);
    let mut result = String::new();
    let mut used: usize = 0;
    for grapheme in text.graphemes(true) {
        let grapheme_width = grapheme.width();
        if used.saturating_add(grapheme_width) > content_width {
            break;
        }
        result.push_str(grapheme);
        used = used.saturating_add(grapheme_width);
    }
    result.push('…');
    result
}

fn visible_tail(query: &str, width: usize) -> &str {
    let mut used: usize = 0;
    for (index, grapheme) in query.grapheme_indices(true).rev() {
        used += grapheme.width();
        if used > width {
            return &query[index + grapheme.len()..];
        }
    }
    query
}

fn wrapped_line_count(lines: &[Line<'_>], width: u16) -> usize {
    let width = usize::from(width);
    if width == 0 {
        return 0;
    }

    lines
        .iter()
        .map(|line| {
            let text = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            wrapped_text_line_count(&text, width)
        })
        .sum()
}

fn wrapped_text_line_count(text: &str, width: usize) -> usize {
    let mut words = text.split_whitespace();
    let Some(first) = words.next() else {
        return 1;
    };

    let mut lines = 1;
    let mut used = 0;
    place_word(first.width(), width, &mut lines, &mut used);
    for word in words {
        let word_width = word.width();
        if used < width && used.saturating_add(1).saturating_add(word_width) <= width {
            used += 1 + word_width;
            continue;
        }

        lines += 1;
        used = 0;
        place_word(word_width, width, &mut lines, &mut used);
    }
    lines
}

fn place_word(word_width: usize, width: usize, lines: &mut usize, used: &mut usize) {
    if word_width <= width {
        *used = word_width;
        return;
    }

    *lines += (word_width - 1) / width;
    *used = word_width % width;
    if *used == 0 {
        *used = width;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BrowserState, Component, MemoryBrowser, MemoryBrowserEffect, MemoryBrowserEvent,
        ReturnView, SortMode,
    };
    use crate::{
        core::extensions::memory::{MemoryKey, MemoryRecord},
        tui::theme::Theme,
    };
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend};

    fn record(id: i64, version: u64, content: &str) -> MemoryRecord {
        MemoryRecord {
            key: MemoryKey { id, version },
            content: content.to_owned(),
            created_at_ms: 0,
            updated_at_ms: 0,
            last_scanned_at_ms: None,
            scan_count: 2,
            last_used_at_ms: None,
            use_count: 3,
            probation_until_ms: None,
        }
    }

    fn record_with_stats(id: i64, updated_at_ms: i64, use_count: u64) -> MemoryRecord {
        MemoryRecord {
            updated_at_ms,
            use_count,
            ..record(id, 1, &format!("memory {id}"))
        }
    }

    fn ordered_ids(browser: &MemoryBrowser) -> Vec<i64> {
        browser
            .matches
            .iter()
            .map(|index| browser.records[*index].key.id)
            .collect()
    }

    fn key(code: KeyCode) -> MemoryBrowserEvent {
        MemoryBrowserEvent::Terminal(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn modified_key(code: KeyCode, modifiers: KeyModifiers) -> MemoryBrowserEvent {
        MemoryBrowserEvent::Terminal(Event::Key(KeyEvent::new(code, modifiers)))
    }

    fn repeat_key(code: KeyCode) -> MemoryBrowserEvent {
        MemoryBrowserEvent::Terminal(Event::Key(KeyEvent::new_with_kind(
            code,
            KeyModifiers::NONE,
            KeyEventKind::Repeat,
        )))
    }

    fn loaded(records: Vec<MemoryRecord>) -> MemoryBrowser {
        let mut browser = MemoryBrowser::new();
        browser.update(MemoryBrowserEvent::Loaded(records));
        browser
    }

    fn render(browser: &mut MemoryBrowser, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| browser.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn filtering_and_reloads_preserve_selection_by_id() {
        let mut browser = loaded(vec![record(1, 1, "cafe moon"), record(42, 2, "cafe sun")]);
        browser.update(key(KeyCode::Down));
        browser.update(MemoryBrowserEvent::Terminal(Event::Paste(
            "cafe".to_owned(),
        )));
        assert_eq!(browser.selected_id, Some(1));

        browser.update(MemoryBrowserEvent::Loaded(vec![
            record(42, 2, "cafe sun"),
            record(1, 1, "cafe moon"),
        ]));
        assert_eq!(browser.selected_id, Some(1));

        browser.query.clear();
        browser.refresh_matches();
        browser.update(MemoryBrowserEvent::Deleted { id: 42 });
        assert_eq!(browser.selected_id, Some(1));
    }

    #[test]
    fn sort_modes_cycle_from_usefulness_through_age_and_back() {
        let mut browser = loaded(vec![
            record_with_stats(1, 100, 5),
            record_with_stats(2, 300, 1),
            record_with_stats(3, 200, 5),
            record_with_stats(4, 50, 0),
        ]);

        assert_eq!(browser.sort, SortMode::MostUseful);
        assert_eq!(ordered_ids(&browser), [3, 1, 2, 4]);
        assert!(render(&mut browser, 80, 16).contains("Sort: Most useful"));

        browser.update(key(KeyCode::Char('f')));
        assert_eq!(browser.sort, SortMode::Newest);
        assert_eq!(ordered_ids(&browser), [2, 3, 1, 4]);
        browser.update(MemoryBrowserEvent::Terminal(Event::Paste(
            "memory".to_owned(),
        )));
        browser.update(MemoryBrowserEvent::Loaded(vec![
            record_with_stats(4, 50, 0),
            record_with_stats(3, 200, 5),
            record_with_stats(2, 300, 1),
            record_with_stats(1, 100, 5),
        ]));
        assert_eq!(browser.sort, SortMode::Newest);
        assert_eq!(ordered_ids(&browser), [2, 3, 1, 4]);

        browser.update(key(KeyCode::Char('f')));
        assert_eq!(browser.sort, SortMode::Oldest);
        assert_eq!(ordered_ids(&browser), [4, 1, 3, 2]);

        browser.update(key(KeyCode::Char('f')));
        assert_eq!(browser.sort, SortMode::LeastUseful);
        assert_eq!(ordered_ids(&browser), [4, 2, 1, 3]);

        browser.update(key(KeyCode::Char('f')));
        assert_eq!(browser.sort, SortMode::MostUseful);
        assert_eq!(ordered_ids(&browser), [3, 1, 2, 4]);
    }

    #[test]
    fn backspace_removes_a_whole_unicode_grapheme_and_ids_are_searchable() {
        let mut browser = loaded(vec![record(42, 1, "unrelated")]);
        browser.update(key(KeyCode::Char('e')));
        browser.update(key(KeyCode::Char('\u{301}')));
        browser.update(key(KeyCode::Backspace));
        assert!(browser.query.is_empty());

        browser.update(MemoryBrowserEvent::Terminal(Event::Paste("42".to_owned())));
        assert_eq!(browser.matches, [0]);
        assert_eq!(browser.selected_id, Some(42));
    }

    #[test]
    fn lowercase_shortcut_letters_remain_available_to_the_filter() {
        let mut browser = loaded(vec![record(1, 1, "durable preference")]);

        for character in "durable".chars() {
            browser.update(key(KeyCode::Char(character)));
        }

        assert_eq!(browser.query, "durable");
        assert_eq!(browser.matches, [0]);
        assert_eq!(
            browser
                .update(modified_key(KeyCode::Char('r'), KeyModifiers::CONTROL))
                .effects,
            [MemoryBrowserEffect::Refresh]
        );
    }

    #[test]
    fn deletion_needs_two_physical_delete_keys_and_emits_once() {
        let mut browser = loaded(vec![record(7, 3, "forget me")]);

        assert!(browser.update(key(KeyCode::Delete)).effects.is_empty());
        assert!(matches!(
            &browser.state,
            BrowserState::ConfirmDelete {
                id: 7,
                return_to: ReturnView::List
            }
        ));
        assert!(
            browser
                .update(repeat_key(KeyCode::Delete))
                .effects
                .is_empty()
        );
        assert_eq!(
            browser.update(key(KeyCode::Delete)).effects,
            [MemoryBrowserEffect::Delete(MemoryKey { id: 7, version: 3 })]
        );
        assert!(browser.update(key(KeyCode::Delete)).effects.is_empty());
    }

    #[test]
    fn optimistic_delete_conflicts_reload_instead_of_retrying_a_stale_key() {
        let mut browser = loaded(vec![record(7, 3, "changed elsewhere")]);
        browser.update(key(KeyCode::Delete));
        browser.update(key(KeyCode::Delete));

        let update = browser.update(MemoryBrowserEvent::DeleteFailed {
            error: "memory changed since it was read".to_owned(),
            conflict: true,
        });

        assert_eq!(update.effects, [MemoryBrowserEffect::Refresh]);
        assert!(matches!(browser.state, BrowserState::Loading));
    }

    #[test]
    fn escape_returns_from_detail_then_dismisses() {
        let mut browser = loaded(vec![record(1, 1, "inspect me")]);
        browser.update(key(KeyCode::Enter));
        browser.update(key(KeyCode::Down));
        assert!(browser.update(key(KeyCode::Esc)).effects.is_empty());
        assert!(matches!(&browser.state, BrowserState::List));
        assert_eq!(
            browser.update(key(KeyCode::Esc)).effects,
            [MemoryBrowserEffect::Dismiss]
        );
    }

    #[test]
    fn list_render_distinguishes_empty_and_no_matches() {
        let mut empty = loaded(Vec::new());
        assert!(render(&mut empty, 60, 12).contains("Memory is empty"));

        let mut filtered = loaded(vec![record(1, 1, "alpha")]);
        filtered.update(MemoryBrowserEvent::Terminal(Event::Paste(
            "missing".to_owned(),
        )));
        assert!(render(&mut filtered, 60, 12).contains("No memories match"));
    }

    #[test]
    fn render_sanitizes_controls_and_is_safe_when_narrow() {
        let mut browser = loaded(vec![record(1, 1, "safe\u{1b}[31m\nnext")]);
        let rendered = render(&mut browser, 30, 8);
        assert!(rendered.contains("safe [31m next"));
        assert!(!rendered.contains('\u{1b}'));

        let rendered = render(&mut browser, 3, 2);
        assert!(!rendered.is_empty());
    }

    #[test]
    fn detail_renders_full_content_and_metadata_without_emitting_an_effect() {
        let mut browser = loaded(vec![record(9, 4, "first line\nsecond line")]);
        assert!(browser.update(key(KeyCode::Tab)).effects.is_empty());

        let rendered = render(&mut browser, 80, 28);
        assert!(rendered.contains("Memory metadata"));
        assert!(rendered.contains("first line"));
        assert!(rendered.contains("second line"));
    }
}
