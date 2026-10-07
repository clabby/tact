//! Searchable workspace file picker opened from the composer.

use super::{
    floating::Floating,
    node::{Component, ComponentUpdate, RenderRequest},
};
use crate::{
    search::{discover_paths, rank},
    tui::theme::Theme,
};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph},
};
use std::path::Path;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const KEY_BINDINGS: [(&str, &str); 3] = [("↑↓", "move"), ("enter/tab", "insert"), ("esc", "close")];
const SEARCH_LABEL: &str = "Search: ";
const FOCUS_MARKER: &str = "› ";

pub(super) enum FileFinderEvent {
    Terminal(Event),
    Query(String),
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum FileFinderEffect {
    Dismiss,
    Insert(String),
}

pub(super) struct FileFinder {
    paths: Vec<String>,
    query: String,
    selected: usize,
    matches: Vec<usize>,
}

impl FileFinder {
    pub(super) fn new(workspace: &Path) -> Self {
        let paths = discover_paths(workspace);
        let matches = (0..paths.len()).collect();
        Self {
            paths,
            query: String::new(),
            selected: 0,
            matches,
        }
    }

    fn update_key(&mut self, key: KeyEvent) -> ComponentUpdate<FileFinderEffect> {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return ComponentUpdate::none();
        }

        match key.code {
            KeyCode::Esc => Self::dismiss(),
            KeyCode::Enter | KeyCode::Tab => self.handle_enter(),
            KeyCode::Up => {
                self.select_previous();
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            KeyCode::Down => {
                self.select_next();
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            _ => ComponentUpdate::none(),
        }
    }

    fn set_query(&mut self, query: String) -> ComponentUpdate<FileFinderEffect> {
        self.query = query;
        self.refresh_matches();
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn dismiss() -> ComponentUpdate<FileFinderEffect> {
        ComponentUpdate {
            effects: vec![FileFinderEffect::Dismiss],
            render: RenderRequest::Immediate,
        }
    }

    fn handle_enter(&mut self) -> ComponentUpdate<FileFinderEffect> {
        let Some(index) = self.matches.get(self.selected) else {
            return ComponentUpdate::none();
        };
        ComponentUpdate {
            effects: vec![FileFinderEffect::Insert(self.paths[*index].clone())],
            render: RenderRequest::Immediate,
        }
    }

    fn refresh_matches(&mut self) {
        self.matches = rank(&self.paths, &self.query, String::as_str);
        self.selected = 0;
    }

    fn select_previous(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    fn select_next(&mut self) {
        if !self.matches.is_empty() {
            self.selected = (self.selected + 1).min(self.matches.len() - 1);
        }
    }

    fn render_search(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        if area.is_empty() {
            return;
        }

        let marker = "  ";
        let prefix_width = marker.width() + SEARCH_LABEL.width();
        let query_width = usize::from(area.width).saturating_sub(prefix_width);
        let visible_query = visible_query_tail(&self.query, query_width);
        let label_style = Style::default().fg(theme.muted());
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(marker, label_style),
                Span::styled(SEARCH_LABEL, label_style),
                Span::styled(visible_query, Style::default().fg(theme.text())),
            ])),
            area,
        );
    }

    fn render_paths(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        if area.is_empty() {
            return;
        }

        let items = self.matches.iter().map(|index| {
            ListItem::new(self.paths[*index].as_str()).style(Style::default().fg(theme.text()))
        });
        let list = List::new(items)
            .highlight_style(
                Style::default()
                    .fg(theme.accent())
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol(FOCUS_MARKER);
        let selected = (!self.matches.is_empty()).then_some(self.selected);
        let mut state = ListState::default().with_selected(selected);
        frame.render_stateful_widget(list, area, &mut state);
    }
}

impl Component for FileFinder {
    type Event = FileFinderEvent;
    type Effect = FileFinderEffect;

    fn update(&mut self, event: Self::Event) -> ComponentUpdate<Self::Effect> {
        match event {
            FileFinderEvent::Terminal(Event::Key(key)) => self.update_key(key),
            FileFinderEvent::Terminal(_) => ComponentUpdate::none(),
            FileFinderEvent::Query(query) => self.set_query(query),
        }
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        if area.is_empty() {
            return;
        }

        let layout = Floating::new("Files and directories", 72, 14, &KEY_BINDINGS)
            .render(frame, area, theme);
        if layout.body.is_empty() {
            return;
        }
        let search_area = Rect {
            height: 1,
            ..layout.body
        };
        let paths_area = Rect {
            y: layout.body.y + 1,
            height: layout.body.height.saturating_sub(1),
            ..layout.body
        };
        self.render_search(frame, search_area, theme);
        self.render_paths(frame, paths_area, theme);
    }
}

pub(super) fn visible_query_tail(query: &str, width: usize) -> &str {
    let mut used = 0;
    for (index, grapheme) in query.grapheme_indices(true).rev() {
        used += grapheme.width();
        if used > width {
            return &query[index + grapheme.len()..];
        }
    }
    query
}

#[cfg(test)]
mod tests {
    use super::{Component, FileFinder, FileFinderEffect, FileFinderEvent};
    use crate::tui::theme::Theme;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend};
    use std::fs;

    fn key(code: KeyCode) -> FileFinderEvent {
        FileFinderEvent::Terminal(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn workspace() -> tempfile::TempDir {
        let workspace = tempfile::tempdir().unwrap();
        fs::create_dir_all(workspace.path().join("src/components")).unwrap();
        fs::create_dir_all(workspace.path().join("target/debug")).unwrap();
        fs::write(workspace.path().join("README.md"), "read me").unwrap();
        fs::write(workspace.path().join("src/lib.rs"), "pub mod components;").unwrap();
        fs::write(workspace.path().join("src/components/file_finder.rs"), "").unwrap();
        fs::write(workspace.path().join("target/debug/artifact"), "").unwrap();
        workspace
    }

    #[test]
    fn fuzzy_search_matches_non_contiguous_characters_and_ranks_tight_matches_first() {
        let workspace = workspace();
        let mut finder = FileFinder::new(workspace.path());
        finder.update(FileFinderEvent::Query("ff".to_owned()));

        assert_eq!(finder.matches.len(), 1);
        assert_eq!(
            finder.paths[finder.matches[0]],
            "src/components/file_finder.rs"
        );
    }

    #[test]
    fn enter_inserts_a_unique_search_result() {
        let workspace = workspace();
        let mut finder = FileFinder::new(workspace.path());
        finder.update(FileFinderEvent::Query("read".to_owned()));

        assert_eq!(
            finder.update(key(KeyCode::Enter)).effects,
            [FileFinderEffect::Insert("README.md".to_owned())]
        );
    }

    #[test]
    fn enter_inserts_a_directory_with_a_trailing_slash() {
        let workspace = workspace();
        let mut finder = FileFinder::new(workspace.path());
        finder.update(FileFinderEvent::Query("components/".to_owned()));

        assert_eq!(
            finder.update(key(KeyCode::Enter)).effects,
            [FileFinderEffect::Insert("src/components/".to_owned())]
        );
    }

    #[test]
    fn tab_inserts_the_selected_search_result() {
        let workspace = workspace();
        let mut finder = FileFinder::new(workspace.path());
        finder.update(FileFinderEvent::Query("read".to_owned()));

        assert_eq!(
            finder.update(key(KeyCode::Tab)).effects,
            [FileFinderEffect::Insert("README.md".to_owned())]
        );
    }

    #[test]
    fn arrows_navigate_results_before_selection() {
        let workspace = workspace();
        let mut finder = FileFinder::new(workspace.path());
        finder.update(key(KeyCode::Down));

        assert_eq!(
            finder.update(key(KeyCode::Enter)).effects,
            [FileFinderEffect::Insert("src/".to_owned())]
        );
    }

    #[test]
    fn query_updates_filter_results_and_escape_dismisses() {
        let workspace = workspace();
        let mut finder = FileFinder::new(workspace.path());
        finder.update(FileFinderEvent::Query("read".to_owned()));

        assert_eq!(finder.matches.len(), 1);
        assert_eq!(finder.paths[finder.matches[0]], "README.md");
        assert_eq!(
            finder.update(key(KeyCode::Esc)).effects,
            [FileFinderEffect::Dismiss]
        );
    }

    #[test]
    fn popup_uses_file_finder_chrome_and_selection_styling() {
        let workspace = workspace();
        let mut finder = FileFinder::new(workspace.path());
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();

        terminal
            .draw(|frame| finder.render(frame, frame.area(), &Theme::default()))
            .unwrap();

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(4, 3)].symbol(), "╭");
        assert_eq!(buffer[(75, 16)].symbol(), "╯");
        assert_eq!(buffer[(5, 5)].symbol(), "›");
        assert_eq!(buffer[(5, 5)].fg, Theme::default().accent());
        assert!(buffer.content().chunks(80).any(|cells| {
            cells
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
                .contains("Files and directories")
        }));
        assert!(buffer.content().chunks(80).any(|cells| {
            cells
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
                .contains("enter/tab insert")
        }));
    }

    #[test]
    fn footer_says_when_enter_or_tab_will_insert() {
        let workspace = workspace();
        let mut finder = FileFinder::new(workspace.path());
        finder.update(FileFinderEvent::Query("read".to_owned()));
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();

        terminal
            .draw(|frame| finder.render(frame, frame.area(), &Theme::default()))
            .unwrap();

        assert!(
            terminal
                .backend()
                .buffer()
                .content()
                .chunks(80)
                .any(|cells| {
                    cells
                        .iter()
                        .map(|cell| cell.symbol())
                        .collect::<String>()
                        .contains("enter/tab insert")
                })
        );
    }

    #[test]
    fn narrow_terminals_do_not_overflow_the_popup() {
        let workspace = workspace();
        let mut finder = FileFinder::new(workspace.path());
        let mut terminal = Terminal::new(TestBackend::new(3, 2)).unwrap();

        terminal
            .draw(|frame| finder.render(frame, frame.area(), &Theme::default()))
            .unwrap();

        assert_eq!(terminal.backend().buffer().area.width, 3);
    }
}
