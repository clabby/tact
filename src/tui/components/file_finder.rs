//! Searchable workspace file picker opened from the composer.

use super::{
    floating::Floating,
    node::{Component, ComponentUpdate, RenderRequest},
};
use crate::tui::theme::Theme;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph},
};
use std::{cmp::Reverse, fs, path::Path};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const RESULTS_KEY_BINDINGS: [&str; 4] =
    ["tab focus", "↑↓ move", "enter focus results", "esc close"];
const INSERT_KEY_BINDINGS: [&str; 4] = ["tab focus", "↑↓ move", "enter insert", "esc close"];
const EMPTY_LIST_KEY_BINDINGS: [&str; 3] = ["tab focus", "↑↓ move", "esc close"];
const NO_RESULTS_KEY_BINDINGS: [&str; 2] = ["tab focus", "esc close"];
const SEARCH_LABEL: &str = "Search: ";
const FOCUS_MARKER: &str = "› ";
const SKIPPED_DIRECTORIES: [&str; 4] = [".git", ".jj", "node_modules", "target"];

pub(super) enum FileFinderEvent {
    Terminal(Event),
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum FileFinderEffect {
    Dismiss,
    Insert(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Focus {
    Search,
    List,
}

pub(super) struct FileFinder {
    files: Vec<String>,
    query: String,
    focus: Focus,
    selected: usize,
    matches: Vec<usize>,
}

impl FileFinder {
    pub(super) fn new(workspace: &Path) -> Self {
        let files = discover_files(workspace);
        let matches = (0..files.len()).collect();
        Self {
            files,
            query: String::new(),
            focus: Focus::Search,
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
            KeyCode::Backspace if self.focus == Focus::Search && !self.query.is_empty() => {
                self.remove_last_grapheme();
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            KeyCode::Backspace => Self::dismiss(),
            KeyCode::Enter => self.handle_enter(),
            KeyCode::Tab | KeyCode::BackTab => {
                self.toggle_focus();
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            KeyCode::Up if self.focus == Focus::List => {
                self.select_previous();
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            KeyCode::Down if self.focus == Focus::List => {
                self.select_next();
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            KeyCode::Char(character)
                if self.focus == Focus::Search
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.query.push(character);
                self.refresh_matches();
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            _ => ComponentUpdate::none(),
        }
    }

    fn insert_paste(&mut self, text: &str) -> ComponentUpdate<FileFinderEffect> {
        if self.focus != Focus::Search {
            return ComponentUpdate::none();
        }

        self.query
            .extend(text.chars().filter(|character| !character.is_control()));
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
        if self.focus == Focus::Search && self.matches.len() != 1 {
            self.toggle_focus();
            return ComponentUpdate::render(RenderRequest::Immediate);
        }

        let Some(index) = self.matches.get(self.selected) else {
            return ComponentUpdate::none();
        };
        ComponentUpdate {
            effects: vec![FileFinderEffect::Insert(self.files[*index].clone())],
            render: RenderRequest::Immediate,
        }
    }

    fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Search => Focus::List,
            Focus::List => Focus::Search,
        };
    }

    fn key_bindings(&self) -> &'static [&'static str] {
        if self.focus == Focus::Search {
            return match self.matches.len() {
                0 => &NO_RESULTS_KEY_BINDINGS,
                1 => &INSERT_KEY_BINDINGS,
                _ => &RESULTS_KEY_BINDINGS,
            };
        }
        if self.matches.is_empty() {
            &EMPTY_LIST_KEY_BINDINGS
        } else {
            &INSERT_KEY_BINDINGS
        }
    }

    fn remove_last_grapheme(&mut self) {
        let Some((index, _)) = self.query.grapheme_indices(true).next_back() else {
            return;
        };
        self.query.truncate(index);
        self.refresh_matches();
    }

    fn refresh_matches(&mut self) {
        let query = self.query.to_ascii_lowercase();
        let mut matches = self
            .files
            .iter()
            .enumerate()
            .filter_map(|(index, path)| fuzzy_score(path, &query).map(|score| (index, score)))
            .collect::<Vec<_>>();
        matches.sort_by_key(|(index, score)| (Reverse(*score), self.files[*index].as_str()));
        self.matches = matches.into_iter().map(|(index, _)| index).collect();
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

        let focused = self.focus == Focus::Search;
        let marker = if focused { FOCUS_MARKER } else { "  " };
        let prefix_width = marker.width() + SEARCH_LABEL.width();
        let query_width = usize::from(area.width).saturating_sub(prefix_width);
        let visible_query = visible_query_tail(&self.query, query_width);
        let focus_style = if focused {
            Style::default()
                .fg(theme.accent())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.muted())
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(marker, focus_style),
                Span::styled(SEARCH_LABEL, focus_style),
                Span::styled(visible_query, Style::default().fg(theme.text())),
            ])),
            area,
        );

        if focused {
            let cursor_offset = prefix_width + visible_query.width();
            let cursor_x = area.x
                + u16::try_from(cursor_offset)
                    .unwrap_or(u16::MAX)
                    .min(area.width.saturating_sub(1));
            frame.set_cursor_position(Position::new(cursor_x, area.y));
        }
    }

    fn render_files(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        if area.is_empty() {
            return;
        }

        let items = self.matches.iter().map(|index| {
            ListItem::new(self.files[*index].as_str()).style(Style::default().fg(theme.text()))
        });
        let focused = self.focus == Focus::List;
        let list = List::new(items)
            .highlight_style(
                Style::default()
                    .fg(theme.accent())
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol(if focused { FOCUS_MARKER } else { "  " });
        let selected = (!self.matches.is_empty()).then_some(self.selected);
        let mut state = ListState::default().with_selected(selected);
        frame.render_stateful_widget(list, area, &mut state);

        if focused && selected.is_some() {
            let visible_row = self.selected.saturating_sub(state.offset());
            let cursor_y = area.y + u16::try_from(visible_row).unwrap_or(u16::MAX);
            frame.set_cursor_position(Position::new(area.x, cursor_y.min(area.bottom() - 1)));
        }
    }
}

impl Component for FileFinder {
    type Event = FileFinderEvent;
    type Effect = FileFinderEffect;

    fn update(&mut self, event: Self::Event) -> ComponentUpdate<Self::Effect> {
        match event {
            FileFinderEvent::Terminal(Event::Key(key)) => self.update_key(key),
            FileFinderEvent::Terminal(Event::Paste(text)) => self.insert_paste(&text),
            FileFinderEvent::Terminal(_) => ComponentUpdate::none(),
        }
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        if area.is_empty() {
            return;
        }

        let layout = Floating::new("Files", 72, 14, self.key_bindings()).render(frame, area, theme);
        if layout.body.is_empty() {
            return;
        }
        let search_area = Rect {
            height: 1,
            ..layout.body
        };
        let files_area = Rect {
            y: layout.body.y + 1,
            height: layout.body.height.saturating_sub(1),
            ..layout.body
        };
        self.render_search(frame, search_area, theme);
        self.render_files(frame, files_area, theme);
    }
}

fn discover_files(workspace: &Path) -> Vec<String> {
    let mut paths = Vec::new();
    visit_directory(workspace, workspace, &mut paths);
    paths.sort_unstable();
    paths
}

fn visit_directory(workspace: &Path, directory: &Path, paths: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let mut entries = entries.flatten().collect::<Vec<_>>();
    entries.sort_unstable_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            if !is_skipped_directory(&path) {
                visit_directory(workspace, &path, paths);
            }
        } else if file_type.is_file()
            && let Ok(relative) = path.strip_prefix(workspace)
        {
            let relative = relative
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/");
            if !relative.chars().any(char::is_control) {
                paths.push(relative);
            }
        }
    }
}

fn is_skipped_directory(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| SKIPPED_DIRECTORIES.contains(&name))
}

fn fuzzy_score(path: &str, query: &str) -> Option<usize> {
    if query.is_empty() {
        return Some(0);
    }

    let path = path.to_ascii_lowercase();
    let mut query = query.chars();
    let mut expected = query.next()?;
    let mut score = 0_usize;
    let mut previous_match = None;
    let mut previous_character = None;
    for (index, character) in path.chars().enumerate() {
        if character != expected {
            previous_character = Some(character);
            continue;
        }
        score += 10;
        if previous_match.is_some_and(|previous| previous + 1 == index) {
            score += 15;
        }
        if previous_character.is_none_or(|previous| previous == '/') {
            score += 8;
        }
        previous_match = Some(index);
        let Some(next) = query.next() else {
            return Some(score.saturating_sub(index));
        };
        expected = next;
        previous_character = Some(character);
    }
    None
}

fn visible_query_tail(query: &str, width: usize) -> &str {
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
    use super::{
        Component, FileFinder, FileFinderEffect, FileFinderEvent, Focus, discover_files,
        fuzzy_score,
    };
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
    fn discovers_relative_workspace_files_and_skips_build_directories() {
        let workspace = workspace();

        assert_eq!(
            discover_files(workspace.path()),
            ["README.md", "src/components/file_finder.rs", "src/lib.rs"]
        );
    }

    #[test]
    fn fuzzy_search_matches_non_contiguous_characters_and_ranks_tight_matches_first() {
        let workspace = workspace();
        let mut finder = FileFinder::new(workspace.path());

        for character in "ff".chars() {
            finder.update(key(KeyCode::Char(character)));
        }

        assert_eq!(finder.matches.len(), 1);
        assert_eq!(
            finder.files[finder.matches[0]],
            "src/components/file_finder.rs"
        );
        assert!(fuzzy_score("src/file_finder.rs", "ff").is_some());
        assert!(fuzzy_score("README.md", "ff").is_none());
    }

    #[test]
    fn enter_inserts_a_unique_search_result() {
        let workspace = workspace();
        let mut finder = FileFinder::new(workspace.path());
        for character in "read".chars() {
            finder.update(key(KeyCode::Char(character)));
        }

        assert_eq!(
            finder.update(key(KeyCode::Enter)).effects,
            [FileFinderEffect::Insert("README.md".to_owned())]
        );
    }

    #[test]
    fn list_focus_supports_navigation_and_selection() {
        let workspace = workspace();
        let mut finder = FileFinder::new(workspace.path());
        finder.update(key(KeyCode::Tab));
        finder.update(key(KeyCode::Down));

        assert_eq!(finder.focus, Focus::List);
        assert_eq!(
            finder.update(key(KeyCode::Enter)).effects,
            [FileFinderEffect::Insert(
                "src/components/file_finder.rs".to_owned()
            )]
        );
    }

    #[test]
    fn backspace_edits_search_then_dismisses_and_escape_always_dismisses() {
        let workspace = workspace();
        let mut finder = FileFinder::new(workspace.path());
        finder.update(key(KeyCode::Char('x')));

        assert!(finder.update(key(KeyCode::Backspace)).effects.is_empty());
        assert_eq!(finder.query, "");
        assert_eq!(
            finder.update(key(KeyCode::Backspace)).effects,
            [FileFinderEffect::Dismiss]
        );

        let mut finder = FileFinder::new(workspace.path());
        finder.update(key(KeyCode::Char('x')));
        assert_eq!(
            finder.update(key(KeyCode::Esc)).effects,
            [FileFinderEffect::Dismiss]
        );
    }

    #[test]
    fn popup_uses_file_finder_chrome_and_focus_styling() {
        let workspace = workspace();
        let mut finder = FileFinder::new(workspace.path());
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();

        terminal
            .draw(|frame| finder.render(frame, frame.area(), &Theme::default()))
            .unwrap();

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(4, 3)].symbol(), "╭");
        assert_eq!(buffer[(75, 16)].symbol(), "╯");
        assert_eq!(buffer[(5, 4)].symbol(), "›");
        assert_eq!(buffer[(5, 4)].fg, Theme::default().accent());
        assert!(buffer.content().chunks(80).any(|cells| {
            cells
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
                .contains("enter focus results")
        }));
    }

    #[test]
    fn footer_says_when_enter_will_insert() {
        let workspace = workspace();
        let mut finder = FileFinder::new(workspace.path());
        for character in "read".chars() {
            finder.update(key(KeyCode::Char(character)));
        }
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
                        .contains("enter insert")
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
