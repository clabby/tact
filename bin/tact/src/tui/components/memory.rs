//! Searchable, read-only inspection and explicit deletion of stored memories.
//!
//! The browser receives terminal input plus backend results (`Loaded`, `LoadFailed`,
//! `Deleted`, `DeleteFailed`) as [`MemoryBrowserEvent`]s and asks its owner to dismiss it,
//! reload, or delete one exact key through [`MemoryBrowserEffect`]s. It never touches the
//! store itself.
//!
//! Selection follows a memory key rather than a row position, so filtering, re-sorting, and
//! reloading keep the same memory selected while it remains visible. Deletion always needs a
//! confirming second press, is offered only for keys the current access may delete, and a
//! conflicting delete reloads instead of retrying a stale version.

use super::{
    clock::unix_time_ms,
    fit::ellipsize,
    floating::Floating,
    node::{Component, ComponentUpdate, RenderRequest},
};
use crate::{app::theme::Theme, tui::format::format_age};
use chrono::{DateTime, Utc};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph, Wrap},
};
use tact_memory::{MemoryAccess, MemoryKey, MemoryRecord, MemorySource};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const LIST_KEYS: [(&str, &str); 6] = [
    ("↑↓", "move"),
    ("enter", "inspect"),
    ("ctrl+s", "sort"),
    ("ctrl+d", "remove"),
    ("ctrl+r", "refresh"),
    ("esc", "close"),
];
const REMOTE_LIST_KEYS: [(&str, &str); 6] = [
    ("↑↓", "move"),
    ("enter", "inspect"),
    ("ctrl+s", "sort"),
    ("ctrl+n", "namespaces"),
    ("ctrl+r", "refresh"),
    ("esc", "close"),
];
const REMOTE_WRITABLE_LIST_KEYS: [(&str, &str); 7] = [
    ("↑↓", "move"),
    ("enter", "inspect"),
    ("ctrl+s", "sort"),
    ("ctrl+n", "namespaces"),
    ("ctrl+d", "remove"),
    ("ctrl+r", "refresh"),
    ("esc", "close"),
];
const DETAIL_KEYS: [(&str, &str); 4] = [
    ("↑↓/pgup/pgdn", "scroll"),
    ("d", "delete"),
    ("r", "refresh"),
    ("esc", "back"),
];
const REMOTE_DETAIL_KEYS: [(&str, &str); 3] = [
    ("↑↓/pgup/pgdn", "scroll"),
    ("r", "refresh"),
    ("esc", "back"),
];
const CONFIRM_KEYS: [(&str, &str); 2] = [("d/delete", "confirm"), ("esc", "cancel")];
const DELETING_KEYS: [(&str, &str); 1] = [("", "deleting…")];
const LOAD_ERROR_KEYS: [(&str, &str); 2] = [("r", "retry"), ("esc", "close")];
const DELETE_ERROR_KEYS: [(&str, &str); 3] =
    [("d/delete", "retry"), ("r", "refresh"), ("esc", "back")];
const LOADING_KEYS: [(&str, &str); 2] = [("r", "retry"), ("esc", "close")];
const FILTER_LABEL: &str = " Filter: ";
const MAX_PREVIEW_GRAPHEMES: usize = 160;
const MAX_ERROR_WIDTH: usize = 240;

pub(super) enum MemoryBrowserEvent {
    Terminal(Event),
    Loaded {
        access: MemoryAccess,
        records: Vec<MemoryRecord>,
    },
    LoadFailed {
        source: MemorySource,
        access: Option<MemoryAccess>,
        error: String,
    },
    Deleted {
        key: MemoryKey,
    },
    DeleteFailed {
        error: String,
        conflict: bool,
    },
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum MemoryBrowserEffect {
    Dismiss,
    Refresh,
    Delete(MemoryKey),
}

pub(super) struct MemoryBrowser {
    access: Option<MemoryAccess>,
    source: MemorySource,
    records: Vec<MemoryRecord>,
    query: String,
    matches: Vec<usize>,
    selected_key: Option<MemoryKey>,
    sort: SortMode,
    namespace_scope: NamespaceScope,
    state: BrowserState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SortMode {
    MostUseful,
    Newest,
    Oldest,
    LeastUseful,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NamespaceScope {
    All,
    Own,
}

impl NamespaceScope {
    const fn next(self) -> Self {
        match self {
            Self::All => Self::Own,
            Self::Own => Self::All,
        }
    }
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
    Detail {
        key: MemoryKey,
        scroll: u16,
    },
    ConfirmDelete {
        key: MemoryKey,
        return_to: ReturnView,
    },
    Deleting {
        key: MemoryKey,
        return_to: ReturnView,
    },
}

#[derive(Clone)]
struct BrowserError {
    message: String,
    action: ErrorAction,
}

#[derive(Clone)]
enum ErrorAction {
    Load,
    Delete {
        key: MemoryKey,
        return_to: ReturnView,
    },
}

#[derive(Clone, Copy)]
enum ReturnView {
    List,
    Detail { scroll: u16 },
}

struct DetailView {
    key: MemoryKey,
    scroll: u16,
    status: Option<String>,
    update_scroll: bool,
}

/// Why the list body shows a message instead of memory rows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ListPlaceholder {
    /// The backend returned no memories at all.
    EmptyStore,
    /// Memories exist, but none belong to the selected namespace scope.
    EmptyScope,
    /// Memories exist in scope, but none match the filter text.
    NoQueryMatches,
}

impl MemoryBrowser {
    pub(super) const fn new() -> Self {
        Self {
            access: None,
            source: MemorySource::Local,
            records: Vec::new(),
            query: String::new(),
            matches: Vec::new(),
            selected_key: None,
            sort: SortMode::MostUseful,
            namespace_scope: NamespaceScope::All,
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
            BrowserState::Detail {
                key: memory_key,
                scroll,
            } => self.update_detail(key, memory_key, scroll),
            BrowserState::ConfirmDelete {
                key: memory_key,
                return_to,
            } => self.update_confirmation(key, memory_key, return_to),
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
            (
                ErrorAction::Delete {
                    key: memory_key,
                    return_to,
                },
                KeyCode::Char('d') | KeyCode::Delete,
            ) if key.kind == KeyEventKind::Press => self.delete(memory_key, return_to),
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
            KeyCode::Char('s') if key.modifiers == KeyModifiers::CONTROL => self.cycle_sort(),
            KeyCode::Char('n') if key.modifiers == KeyModifiers::CONTROL => {
                self.cycle_namespace_scope()
            }
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
        memory_key: MemoryKey,
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
            self.state = BrowserState::Detail {
                key: memory_key,
                scroll,
            };
            return ComponentUpdate::render(RenderRequest::Immediate);
        }

        match key.code {
            KeyCode::Esc => {
                self.state = BrowserState::List;
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            KeyCode::Char('r') if key.modifiers == KeyModifiers::NONE => self.refresh(),
            KeyCode::Char('d') | KeyCode::Delete if key.kind == KeyEventKind::Press => {
                if !self.can_delete(&memory_key) {
                    return ComponentUpdate::none();
                }
                self.state = BrowserState::ConfirmDelete {
                    key: memory_key,
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
        memory_key: MemoryKey,
        return_to: ReturnView,
    ) -> ComponentUpdate<MemoryBrowserEffect> {
        match key.code {
            KeyCode::Esc => {
                self.restore(return_to);
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            KeyCode::Char('d') | KeyCode::Delete if key.kind == KeyEventKind::Press => {
                self.delete(memory_key, return_to)
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
        let Some(key) = self.selected_key.clone() else {
            return ComponentUpdate::none();
        };
        self.state = BrowserState::Detail { key, scroll: 0 };
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn confirm_selected(&mut self, return_to: ReturnView) -> ComponentUpdate<MemoryBrowserEffect> {
        let Some(key) = self.selected_key.clone() else {
            return ComponentUpdate::none();
        };
        if !self.can_delete(&key) {
            return ComponentUpdate::none();
        }
        self.state = BrowserState::ConfirmDelete { key, return_to };
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn delete(
        &mut self,
        key: MemoryKey,
        return_to: ReturnView,
    ) -> ComponentUpdate<MemoryBrowserEffect> {
        let Some(record) = self.records.iter().find(|record| record.key == key) else {
            self.state = BrowserState::List;
            self.refresh_matches();
            return ComponentUpdate::render(RenderRequest::Immediate);
        };
        let key = record.key.clone();
        self.state = BrowserState::Deleting {
            key: key.clone(),
            return_to,
        };
        Self::effect(MemoryBrowserEffect::Delete(key))
    }

    fn restore(&mut self, return_to: ReturnView) {
        self.state = match return_to {
            ReturnView::List => BrowserState::List,
            ReturnView::Detail { scroll } => {
                let Some(key) = self.selected_key.clone() else {
                    return self.state = BrowserState::List;
                };
                BrowserState::Detail { key, scroll }
            }
        };
    }

    fn replace_records(&mut self, access: MemoryAccess, records: Vec<MemoryRecord>) {
        let fallback = self.selected_match_index().unwrap_or_default();
        self.source = access.source();
        self.access = Some(access);
        self.records = records;
        self.rebuild_matches(fallback);
        self.state = BrowserState::List;
    }

    fn remove_record(&mut self, key: &MemoryKey) {
        let fallback = self.selected_match_index().unwrap_or_default();
        self.records.retain(|record| record.key != *key);
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

    fn cycle_namespace_scope(&mut self) -> ComponentUpdate<MemoryBrowserEffect> {
        if !self.is_remote() {
            return ComponentUpdate::none();
        }
        let fallback = self.selected_match_index().unwrap_or_default();
        self.namespace_scope = self.namespace_scope.next();
        self.rebuild_matches(fallback);
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn rebuild_matches(&mut self, fallback: usize) {
        let query = self.query.to_lowercase();
        self.matches = self
            .records
            .iter()
            .enumerate()
            .filter(|(_, record)| self.namespace_matches(record))
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
        self.selected_key = self
            .matches
            .get(fallback.min(self.matches.len().saturating_sub(1)))
            .map(|index| self.records[*index].key.clone());
    }

    fn selected_match_index(&self) -> Option<usize> {
        let selected_key = self.selected_key.as_ref()?;
        self.matches
            .iter()
            .position(|index| self.records[*index].key == *selected_key)
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
        self.selected_key = Some(self.records[self.matches[next]].key.clone());
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn effect(effect: MemoryBrowserEffect) -> ComponentUpdate<MemoryBrowserEffect> {
        ComponentUpdate {
            effects: vec![effect],
            render: RenderRequest::Immediate,
        }
    }

    fn can_delete(&self, key: &MemoryKey) -> bool {
        self.access
            .as_ref()
            .is_some_and(|access| access.can_delete(key))
    }

    fn is_remote(&self) -> bool {
        self.access
            .as_ref()
            .is_some_and(|access| access.source() == MemorySource::Remote)
            || self.source == MemorySource::Remote
    }

    fn namespace_matches(&self, record: &MemoryRecord) -> bool {
        self.namespace_scope == NamespaceScope::All
            || !self.is_remote()
            || self
                .access
                .as_ref()
                .and_then(MemoryAccess::namespace)
                .is_some_and(|namespace| record.key.namespace.as_deref() == Some(namespace))
    }

    fn namespace_scope_label(&self) -> Option<String> {
        if !self.is_remote() {
            return None;
        }
        Some(match self.namespace_scope {
            NamespaceScope::All => "All namespaces".to_owned(),
            NamespaceScope::Own => self
                .access
                .as_ref()
                .and_then(MemoryAccess::namespace)
                .map_or_else(|| "Our namespace".to_owned(), str::to_owned),
        })
    }

    fn context_label(&self) -> String {
        match self.access.as_ref() {
            Some(MemoryAccess::Remote { namespace, .. }) => format!("Remote memory · {namespace}"),
            _ if self.source == MemorySource::Remote => "Remote memory".to_owned(),
            _ => "Local memory".to_owned(),
        }
    }

    fn list_placeholder(&self) -> Option<ListPlaceholder> {
        if self.records.is_empty() {
            return Some(ListPlaceholder::EmptyStore);
        }
        if !self.matches.is_empty() {
            return None;
        }
        Some(match self.query.is_empty() {
            true => ListPlaceholder::EmptyScope,
            false => ListPlaceholder::NoQueryMatches,
        })
    }

    fn footer(&self) -> &'static [(&'static str, &'static str)] {
        match &self.state {
            BrowserState::Loading => &LOADING_KEYS,
            BrowserState::Error(error) => match error.action {
                ErrorAction::Load => &LOAD_ERROR_KEYS,
                ErrorAction::Delete { .. } => &DELETE_ERROR_KEYS,
            },
            BrowserState::List if self.is_remote() => match self
                .selected_key
                .as_ref()
                .is_some_and(|key| self.can_delete(key))
            {
                true => &REMOTE_WRITABLE_LIST_KEYS,
                false => &REMOTE_LIST_KEYS,
            },
            BrowserState::List => &LIST_KEYS,
            BrowserState::Detail { key, .. } if self.can_delete(key) => &DETAIL_KEYS,
            BrowserState::Detail { .. } => &REMOTE_DETAIL_KEYS,
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
                Paragraph::new(ellipsize(
                    status.as_str().into(),
                    usize::from(status_area.width),
                ))
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
        let scope = self
            .namespace_scope_label()
            .map_or_else(String::new, |scope| format!("  Namespaces: {scope}"));
        let sort = format!("  Sort: {}", self.sort.label());
        let query_width = usize::from(area.width)
            .saturating_sub(FILTER_LABEL.width())
            .saturating_sub(scope.width())
            .saturating_sub(sort.width());
        let query = visible_tail(&self.query, query_width);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(FILTER_LABEL, Style::default().fg(theme.muted())),
                Span::styled(query, Style::default().fg(theme.text())),
                Span::styled(scope, Style::default().fg(theme.muted())),
                Span::styled(sort, Style::default().fg(theme.muted())),
            ])),
            area,
        );
    }

    fn render_records(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        if area.is_empty() {
            return;
        }
        if let Some(placeholder) = self.list_placeholder() {
            let width = usize::from(area.width);
            let message = match placeholder {
                ListPlaceholder::EmptyStore => {
                    format!(" {} is empty. Press r to refresh.", self.context_label())
                }
                ListPlaceholder::EmptyScope => ellipsize(
                    format!(
                        " No memories in {}.",
                        self.namespace_scope_label().unwrap_or_default()
                    )
                    .into(),
                    width,
                )
                .into_owned(),
                ListPlaceholder::NoQueryMatches => ellipsize(
                    format!(" No memories match “{}”.", self.query).into(),
                    width,
                )
                .into_owned(),
            };
            frame.render_widget(
                Paragraph::new(message).style(Style::default().fg(theme.muted())),
                area,
            );
            return;
        }

        let width = usize::from(area.width).saturating_sub(2);
        let items = self.matches.iter().map(|index| {
            let record = &self.records[*index];
            let preview = bounded_preview(&record.content, width);
            let metadata = ellipsize(list_metadata(record).into(), width).into_owned();
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
            key,
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
                Paragraph::new(ellipsize(
                    status.as_str().into(),
                    usize::from(status_area.width),
                ))
                .style(Style::default().fg(theme.accent())),
                status_area,
            );
            area.y = area.y.saturating_add(1);
            area.height = area.height.saturating_sub(1);
        }
        if area.is_empty() {
            return;
        }

        let Some(record) = self.records.iter().find(|record| record.key == key) else {
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
            self.state = BrowserState::Detail { key, scroll };
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
            ErrorAction::Delete { ref key, .. } => format!(
                "Could not delete {}: {message}\n\nPress d/Delete to retry, r to reload, or Esc to return.",
                memory_label(key)
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
            MemoryBrowserEvent::Loaded { access, records } => {
                self.replace_records(access, records);
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            MemoryBrowserEvent::LoadFailed {
                source,
                access,
                error,
            } => {
                self.source = source;
                self.access = access;
                self.state = BrowserState::Error(BrowserError {
                    message: error,
                    action: ErrorAction::Load,
                });
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            MemoryBrowserEvent::Deleted { key } => {
                self.remove_record(&key);
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            MemoryBrowserEvent::DeleteFailed { error, conflict } => {
                let BrowserState::Deleting { key, return_to } = self.state.clone() else {
                    return ComponentUpdate::none();
                };
                if conflict {
                    return self.refresh();
                }
                self.state = BrowserState::Error(BrowserError {
                    message: error,
                    action: ErrorAction::Delete { key, return_to },
                });
                ComponentUpdate::render(RenderRequest::Immediate)
            }
        }
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let state = self.state.clone();
        let title = self.context_label();
        let layout = Floating::new(&title, 88, 28, self.footer()).render(frame, area, theme);
        if layout.body.is_empty() {
            return;
        }

        match state {
            BrowserState::Loading => frame.render_widget(
                Paragraph::new(format!(
                    "Loading {}…\n\nPress r to retry or Esc to close.",
                    self.context_label().to_lowercase()
                ))
                .style(Style::default().fg(theme.muted()))
                .wrap(Wrap { trim: false }),
                layout.body,
            ),
            BrowserState::Error(error) => {
                self.render_error(frame, layout.body, theme, &error);
            }
            BrowserState::List => self.render_list(frame, layout.body, theme, None),
            BrowserState::Detail { key, scroll } => {
                self.render_detail(
                    frame,
                    layout.body,
                    theme,
                    DetailView {
                        key,
                        scroll,
                        status: None,
                        update_scroll: true,
                    },
                );
            }
            BrowserState::ConfirmDelete { key, return_to } => {
                let status = format!(
                    " Delete {}? Press d/Delete again to confirm.",
                    memory_label(&key)
                );
                match return_to {
                    ReturnView::List => {
                        self.render_list(frame, layout.body, theme, Some(status));
                    }
                    ReturnView::Detail { scroll } => self.render_detail(
                        frame,
                        layout.body,
                        theme,
                        DetailView {
                            key,
                            scroll,
                            status: Some(status),
                            update_scroll: false,
                        },
                    ),
                }
            }
            BrowserState::Deleting { key, return_to } => {
                let status = format!(" Deleting {}…", memory_label(&key));
                match return_to {
                    ReturnView::List => {
                        self.render_list(frame, layout.body, theme, Some(status));
                    }
                    ReturnView::Detail { scroll } => self.render_detail(
                        frame,
                        layout.body,
                        theme,
                        DetailView {
                            key,
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
        || record
            .key
            .namespace
            .as_deref()
            .is_some_and(|namespace| namespace.to_lowercase().contains(query))
        || record.content.to_lowercase().contains(query)
}

fn memory_label(key: &MemoryKey) -> String {
    key.namespace.as_ref().map_or_else(
        || format!("memory #{}", key.id),
        |namespace| format!("memory #{}, shared by `{namespace}`", key.id),
    )
}

fn compare_newest(left: &MemoryRecord, right: &MemoryRecord) -> std::cmp::Ordering {
    right
        .updated_at_ms
        .cmp(&left.updated_at_ms)
        .then_with(|| left.key.namespace.cmp(&right.key.namespace))
        .then_with(|| right.key.id.cmp(&left.key.id))
}

fn compare_oldest(left: &MemoryRecord, right: &MemoryRecord) -> std::cmp::Ordering {
    left.updated_at_ms
        .cmp(&right.updated_at_ms)
        .then_with(|| left.key.namespace.cmp(&right.key.namespace))
        .then_with(|| left.key.id.cmp(&right.key.id))
}

fn list_metadata(record: &MemoryRecord) -> String {
    let identity = record.key.namespace.as_ref().map_or_else(
        || format!("local#{}", record.key.id),
        |namespace| format!("{namespace}#{}", record.key.id),
    );
    format!(
        "{} · v{} · updated {} · used {}× · {}",
        identity,
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
    let mut lines = vec![Line::styled(" Memory metadata", heading)];
    lines.extend(
        detail_facts(record)
            .into_iter()
            .map(|(label_text, value_text)| {
                Line::from(vec![
                    Span::styled(format!(" {label_text:<17}"), label),
                    Span::styled(value_text, value),
                ])
            }),
    );
    lines.extend([Line::default(), Line::styled(" Content", heading)]);
    lines.extend(
        sanitize_detail(&record.content)
            .split('\n')
            .map(|line| Line::styled(line.to_owned(), value)),
    );
    lines
}

/// The labelled metadata rows shown above a memory's content, in display order.
fn detail_facts(record: &MemoryRecord) -> [(&'static str, String); 10] {
    [
        ("ID", record.key.id.to_string()),
        (
            "Namespace",
            record
                .key
                .namespace
                .clone()
                .unwrap_or_else(|| "local".to_owned()),
        ),
        ("Version", record.key.version.to_string()),
        ("Created", format_timestamp(record.created_at_ms)),
        (
            "Updated",
            format!(
                "{} ({})",
                format_timestamp(record.updated_at_ms),
                timestamp_age(record.updated_at_ms)
            ),
        ),
        (
            "Last scanned",
            optional_timestamp(record.last_scanned_at_ms),
        ),
        ("Scan count", record.scan_count.to_string()),
        ("Last used", optional_timestamp(record.last_used_at_ms)),
        ("Use count", record.use_count.to_string()),
        (
            "Probation until",
            record
                .probation_until_ms
                .map_or_else(|| "none".to_owned(), format_timestamp),
        ),
    ]
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
    let remaining_ms = until_ms.saturating_sub(i64::try_from(unix_time_ms()).unwrap_or(i64::MAX));
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
    ellipsize(collapsed.into(), width).into_owned()
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
    ellipsize(collapsed.into(), width).into_owned()
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
        BrowserState, Component, DETAIL_KEYS, ErrorAction, LIST_KEYS, ListPlaceholder,
        MemoryBrowser, MemoryBrowserEffect, MemoryBrowserEvent, NamespaceScope, REMOTE_DETAIL_KEYS,
        REMOTE_LIST_KEYS, REMOTE_WRITABLE_LIST_KEYS, ReturnView, SortMode, detail_facts,
        list_metadata, unix_time_ms,
    };
    use crate::app::theme::Theme;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend};
    use tact_memory::{MemoryAccess, MemoryKey, MemoryRecord, MemorySource, RemoteRole};

    fn remote_access(namespace: &str, role: RemoteRole) -> MemoryAccess {
        MemoryAccess::Remote {
            namespace: namespace.to_owned(),
            role,
        }
    }

    fn record(id: i64, version: u64, content: &str) -> MemoryRecord {
        MemoryRecord {
            key: MemoryKey::local(id, version),
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

    /// Marks every record as updated at the same current instant, so relative ages read "now".
    fn updated_now(mut records: Vec<MemoryRecord>) -> Vec<MemoryRecord> {
        let now_ms = i64::try_from(unix_time_ms()).unwrap();
        for record in &mut records {
            record.updated_at_ms = now_ms;
        }
        records
    }

    fn remote_record(namespace: &str, id: i64, content: &str) -> MemoryRecord {
        let mut record = record(id, 1, content);
        record.key = MemoryKey::remote(namespace.to_owned(), id, 1);
        record
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

    fn selected_record(browser: &MemoryBrowser) -> &MemoryRecord {
        let key = browser.selected_key.as_ref().unwrap();
        browser
            .records
            .iter()
            .find(|record| record.key == *key)
            .unwrap()
    }

    fn fact<'a>(facts: &'a [(&'static str, String)], label: &str) -> &'a str {
        facts
            .iter()
            .find(|(fact_label, _)| *fact_label == label)
            .map(|(_, value)| value.as_str())
            .unwrap()
    }

    fn key(code: KeyCode) -> MemoryBrowserEvent {
        modified_key(code, KeyModifiers::NONE)
    }

    fn modified_key(code: KeyCode, modifiers: KeyModifiers) -> MemoryBrowserEvent {
        MemoryBrowserEvent::Terminal(Event::Key(KeyEvent::new(code, modifiers)))
    }

    fn ctrl(character: char) -> MemoryBrowserEvent {
        modified_key(KeyCode::Char(character), KeyModifiers::CONTROL)
    }

    fn paste(text: &str) -> MemoryBrowserEvent {
        MemoryBrowserEvent::Terminal(Event::Paste(text.to_owned()))
    }

    fn repeat_key(code: KeyCode) -> MemoryBrowserEvent {
        MemoryBrowserEvent::Terminal(Event::Key(KeyEvent::new_with_kind(
            code,
            KeyModifiers::NONE,
            KeyEventKind::Repeat,
        )))
    }

    fn loaded(records: Vec<MemoryRecord>) -> MemoryBrowser {
        loaded_with_access(MemoryAccess::Local, records)
    }

    fn loaded_with_access(access: MemoryAccess, records: Vec<MemoryRecord>) -> MemoryBrowser {
        let mut browser = MemoryBrowser::new();
        browser.update(MemoryBrowserEvent::Loaded { access, records });
        browser
    }

    /// Renders the browser and returns each terminal row as text.
    fn render(browser: &mut MemoryBrowser, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| browser.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .chunks(usize::from(width))
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect()
    }

    #[test]
    fn filtering_and_reloads_preserve_selection_by_id() {
        let mut browser = loaded(vec![record(1, 1, "cafe moon"), record(42, 2, "cafe sun")]);
        browser.update(key(KeyCode::Down));
        browser.update(paste("cafe"));
        assert_eq!(browser.selected_key, Some(MemoryKey::local(1, 1)));

        browser.update(MemoryBrowserEvent::Loaded {
            access: MemoryAccess::Local,
            records: vec![record(42, 2, "cafe sun"), record(1, 1, "cafe moon")],
        });
        assert_eq!(browser.selected_key, Some(MemoryKey::local(1, 1)));

        browser.query.clear();
        browser.refresh_matches();
        browser.update(MemoryBrowserEvent::Deleted {
            key: MemoryKey::local(42, 2),
        });
        assert_eq!(browser.selected_key, Some(MemoryKey::local(1, 1)));
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

        browser.update(ctrl('s'));
        assert_eq!(browser.sort, SortMode::Newest);
        assert_eq!(ordered_ids(&browser), [2, 3, 1, 4]);
        browser.update(paste("memory"));
        browser.update(MemoryBrowserEvent::Loaded {
            access: MemoryAccess::Local,
            records: vec![
                record_with_stats(4, 50, 0),
                record_with_stats(3, 200, 5),
                record_with_stats(2, 300, 1),
                record_with_stats(1, 100, 5),
            ],
        });
        assert_eq!(browser.sort, SortMode::Newest);
        assert_eq!(ordered_ids(&browser), [2, 3, 1, 4]);

        browser.update(ctrl('s'));
        assert_eq!(browser.sort, SortMode::Oldest);
        assert_eq!(ordered_ids(&browser), [4, 1, 3, 2]);

        browser.update(ctrl('s'));
        assert_eq!(browser.sort, SortMode::LeastUseful);
        assert_eq!(ordered_ids(&browser), [4, 2, 1, 3]);

        browser.update(ctrl('s'));
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

        browser.update(paste("42"));
        assert_eq!(browser.matches, [0]);
        assert_eq!(browser.selected_key, Some(MemoryKey::local(42, 1)));
    }

    #[test]
    fn lowercase_shortcut_letters_remain_available_to_the_filter() {
        let mut browser = loaded(vec![record(1, 1, "functional preference")]);

        for character in "functional".chars() {
            browser.update(key(KeyCode::Char(character)));
        }

        assert_eq!(browser.query, "functional");
        assert_eq!(browser.matches, [0]);
        assert_eq!(
            browser.update(ctrl('r')).effects,
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
                key,
                return_to: ReturnView::List
            } if *key == MemoryKey::local(7, 3)
        ));
        assert!(
            browser
                .update(repeat_key(KeyCode::Delete))
                .effects
                .is_empty()
        );
        assert_eq!(
            browser.update(key(KeyCode::Delete)).effects,
            [MemoryBrowserEffect::Delete(MemoryKey::local(7, 3))]
        );
        assert!(browser.update(key(KeyCode::Delete)).effects.is_empty());
    }

    #[test]
    fn shared_memories_show_the_author_namespace_and_cannot_be_deleted() {
        let mut browser = loaded_with_access(
            remote_access("bob", RemoteRole::Writer),
            updated_now(vec![remote_record("alice", 7, "shared invariant")]),
        );

        assert_eq!(
            list_metadata(selected_record(&browser)),
            "alice#7 · v1 · updated now · used 3× · no probation"
        );
        assert_eq!(browser.footer(), REMOTE_LIST_KEYS);
        assert!(browser.update(key(KeyCode::Delete)).effects.is_empty());

        browser.update(key(KeyCode::Enter));
        assert!(matches!(browser.state, BrowserState::Detail { .. }));
        assert_eq!(
            fact(&detail_facts(selected_record(&browser)), "Namespace"),
            "alice"
        );
        assert_eq!(browser.footer(), REMOTE_DETAIL_KEYS);
        assert!(browser.update(key(KeyCode::Char('d'))).effects.is_empty());
    }

    #[test]
    fn remote_namespace_scope_toggles_between_all_and_authenticated_namespace() {
        let mut browser = loaded_with_access(
            remote_access("alice", RemoteRole::Writer),
            vec![
                remote_record("alice", 1, "owned invariant"),
                remote_record("bob", 2, "shared convention"),
            ],
        );

        assert_eq!(browser.namespace_scope, NamespaceScope::All);
        assert_eq!(
            browser.namespace_scope_label().as_deref(),
            Some("All namespaces")
        );
        assert_eq!(ordered_ids(&browser), [1, 2]);

        browser.update(ctrl('n'));

        assert_eq!(browser.namespace_scope, NamespaceScope::Own);
        assert_eq!(browser.namespace_scope_label().as_deref(), Some("alice"));
        assert_eq!(ordered_ids(&browser), [1]);
        assert_eq!(
            browser.records[browser.matches[0]].key.namespace.as_deref(),
            Some("alice")
        );

        browser.update(ctrl('n'));
        assert_eq!(browser.namespace_scope, NamespaceScope::All);
        assert_eq!(ordered_ids(&browser), [1, 2]);
    }

    #[test]
    fn remote_writer_can_delete_only_authenticated_namespace_and_reader_is_read_only() {
        let own_key = MemoryKey::remote("alice".to_owned(), 7, 3);
        let mut own = record(7, 3, "owned");
        own.key = own_key.clone();
        let mut writer = loaded_with_access(remote_access("alice", RemoteRole::Writer), vec![own]);
        assert_eq!(writer.context_label(), "Remote memory · alice");
        assert_eq!(writer.footer(), REMOTE_WRITABLE_LIST_KEYS);
        writer.update(key(KeyCode::Delete));
        assert_eq!(
            writer.update(key(KeyCode::Delete)).effects,
            [MemoryBrowserEffect::Delete(own_key)]
        );

        let mut reader = loaded_with_access(
            remote_access("alice", RemoteRole::Reader),
            vec![remote_record("alice", 7, "read only")],
        );
        assert_eq!(reader.footer(), REMOTE_LIST_KEYS);
        assert!(reader.update(key(KeyCode::Delete)).effects.is_empty());
    }

    #[test]
    fn deletion_removes_only_the_exact_key() {
        let old = record(7, 1, "old");
        let current = record(7, 2, "current");
        let mut browser = loaded(vec![old, current]);

        browser.update(MemoryBrowserEvent::Deleted {
            key: MemoryKey::local(7, 1),
        });

        assert_eq!(browser.records.len(), 1);
        assert_eq!(browser.records[0].key, MemoryKey::local(7, 2));
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
    fn list_placeholder_distinguishes_empty_store_scope_and_filter() {
        assert_eq!(
            loaded(Vec::new()).list_placeholder(),
            Some(ListPlaceholder::EmptyStore)
        );

        let mut filtered = loaded(vec![record(1, 1, "alpha")]);
        assert_eq!(filtered.list_placeholder(), None);
        filtered.update(paste("missing"));
        assert_eq!(
            filtered.list_placeholder(),
            Some(ListPlaceholder::NoQueryMatches)
        );

        let mut scoped = loaded_with_access(
            remote_access("alice", RemoteRole::Reader),
            vec![remote_record("bob", 1, "shared")],
        );
        scoped.update(ctrl('n'));
        assert_eq!(scoped.list_placeholder(), Some(ListPlaceholder::EmptyScope));
    }

    #[test]
    fn load_errors_keep_remote_backend_context() {
        let mut browser = MemoryBrowser::new();
        browser.update(MemoryBrowserEvent::LoadFailed {
            source: MemorySource::Remote,
            access: Some(remote_access("alice", RemoteRole::Reader)),
            error: "unavailable".to_owned(),
        });

        assert_eq!(browser.context_label(), "Remote memory · alice");
        assert!(matches!(
            &browser.state,
            BrowserState::Error(error)
                if error.message == "unavailable" && matches!(error.action, ErrorAction::Load)
        ));
    }

    #[test]
    fn local_detail_offers_deletion_and_lists_every_metadata_fact() {
        let mut browser = loaded(vec![record(9, 4, "first line\nsecond line")]);
        assert!(browser.update(key(KeyCode::Tab)).effects.is_empty());

        assert!(matches!(
            &browser.state,
            BrowserState::Detail { key, scroll: 0 } if *key == MemoryKey::local(9, 4)
        ));
        assert_eq!(browser.footer(), DETAIL_KEYS);
        let facts = detail_facts(selected_record(&browser));
        assert_eq!(
            facts.iter().map(|(label, _)| *label).collect::<Vec<_>>(),
            [
                "ID",
                "Namespace",
                "Version",
                "Created",
                "Updated",
                "Last scanned",
                "Scan count",
                "Last used",
                "Use count",
                "Probation until",
            ]
        );
        assert_eq!(fact(&facts, "ID"), "9");
        assert_eq!(fact(&facts, "Namespace"), "local");
        assert_eq!(fact(&facts, "Version"), "4");
        assert_eq!(fact(&facts, "Last used"), "never");
        assert_eq!(fact(&facts, "Probation until"), "none");
    }

    #[test]
    fn narrow_render_does_not_panic() {
        let mut browser = loaded(vec![record(1, 1, "content")]);
        assert_eq!(render(&mut browser, 3, 2).len(), 2);
    }

    #[test]
    fn list_layout_golden() {
        let mut browser = loaded(updated_now(vec![
            record(1, 1, "safe\u{1b}[31m\nnext"),
            record(2, 1, "second memory"),
        ]));
        assert_eq!(browser.footer(), LIST_KEYS);

        assert_eq!(
            render(&mut browser, 64, 9),
            [
                "╭──────────────────────── Local memory ────────────────────────╮",
                "│ Filter:   Sort: Most useful                                  │",
                "│› second memory                                               │",
                "│  local#2 · v1 · updated now · used 3× · no probation         │",
                "│  safe [31m next                                              │",
                "│  local#1 · v1 · updated now · used 3× · no probation         │",
                "│     ↑↓ move · enter inspect · ctrl+s sort · ctrl+d remove    │",
                "│                  ctrl+r refresh · esc close                  │",
                "╰──────────────────────────────────────────────────────────────╯",
            ]
        );
    }

    #[test]
    fn detail_content_layout_golden() {
        let mut browser = loaded(vec![record(9, 4, "first line\n\tsecond\u{7} line")]);
        browser.update(key(KeyCode::Enter));

        let rows = render(&mut browser, 64, 20);
        assert_eq!(
            rows[13..16],
            [
                "│ Content                                                      │",
                "│first line                                                    │",
                "│    second\u{fffd} line                                              │",
            ]
        );
    }

    #[test]
    fn load_error_layout_golden() {
        let mut browser = MemoryBrowser::new();
        browser.update(MemoryBrowserEvent::LoadFailed {
            source: MemorySource::Remote,
            access: Some(remote_access("alice", RemoteRole::Reader)),
            error: "unavailable\nretry later".to_owned(),
        });

        assert_eq!(
            render(&mut browser, 64, 7),
            [
                "╭─────────────────── Remote memory · alice ────────────────────╮",
                "│Could not load memories: unavailable retry later              │",
                "│                                                              │",
                "│Press r to retry or Esc to close.                             │",
                "│                                                              │",
                "│                      r retry · esc close                     │",
                "╰──────────────────────────────────────────────────────────────╯",
            ]
        );
    }
}
