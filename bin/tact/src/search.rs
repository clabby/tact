//! Fuzzy ranking and workspace path discovery behind the composer's mention pickers. The
//! terminal pickers and the web interface rank with the same functions so both offer the same
//! candidates in the same order.

use serde::Serialize;
use std::{cmp::Reverse, fs, path::Path};

const SKIPPED_DIRECTORIES: [&str; 4] = [".git", ".jj", "node_modules", "target"];

/// The most paths one web file query returns. The terminal picker scrolls through all of them.
const MAX_FILE_MATCHES: usize = 50;

/// Scores how well `query` matches `text` as a case-insensitive subsequence; `None` when it does
/// not match. Contiguous runs and matches at the start of a path segment score higher, and earlier
/// matches beat later ones. `query` must already be lowercase.
pub(crate) fn fuzzy_score(text: &str, query: &str) -> Option<usize> {
    if query.is_empty() {
        return Some(0);
    }

    let text = text.to_ascii_lowercase();
    let mut query = query.chars();
    let mut expected = query.next()?;
    let mut score = 0_usize;
    let mut previous_match = None;
    let mut previous_character = None;
    for (index, character) in text.chars().enumerate() {
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

/// Returns the indices of the `items` whose `key` matches `query`, best match first. Equal
/// scores keep the order of `items`, so callers choose the tie order by how they sort the input.
pub(crate) fn rank<T>(items: &[T], query: &str, key: impl Fn(&T) -> &str) -> Vec<usize> {
    let query = query.to_ascii_lowercase();
    let mut matches = items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| fuzzy_score(key(item), &query).map(|score| (index, score)))
        .collect::<Vec<_>>();
    matches.sort_by_key(|&(index, score)| (Reverse(score), index));
    matches.into_iter().map(|(index, _)| index).collect()
}

/// Every file and directory below `workspace`, relative, `/`-separated, and sorted. Directories end
/// in `/`. Version-control metadata, dependency, and build directories are skipped, as are paths
/// containing control characters.
pub(crate) fn discover_paths(workspace: &Path) -> Vec<String> {
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
            if is_skipped_directory(&path) {
                continue;
            }
            if let Some(relative) = relative_path(workspace, &path) {
                paths.push(format!("{relative}/"));
            }
            visit_directory(workspace, &path, paths);
        } else if file_type.is_file()
            && let Some(relative) = relative_path(workspace, &path)
        {
            paths.push(relative);
        }
    }
}

fn relative_path(workspace: &Path, path: &Path) -> Option<String> {
    let relative = path
        .strip_prefix(workspace)
        .ok()?
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/");
    if relative.chars().any(char::is_control) {
        return None;
    }
    Some(relative)
}

fn is_skipped_directory(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| SKIPPED_DIRECTORIES.contains(&name))
}

/// Workspace paths matching an `@` mention query, best first.
#[derive(Debug, Eq, PartialEq, Serialize)]
pub(crate) struct FileMatches {
    pub(crate) paths: Vec<String>,
}

impl FileMatches {
    /// Walks `workspace`; call it off the event loop.
    pub(crate) fn search(workspace: &Path, query: &str) -> Self {
        let mut candidates = discover_paths(workspace);
        let ranked = rank(&candidates, query, String::as_str);
        let paths = ranked
            .into_iter()
            .take(MAX_FILE_MATCHES)
            .map(|index| std::mem::take(&mut candidates[index]))
            .collect();
        Self { paths }
    }
}

#[cfg(test)]
mod tests {
    use super::{FileMatches, discover_paths, rank};
    use std::fs;

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
    fn discovers_relative_workspace_paths_and_skips_build_directories() {
        let workspace = workspace();

        assert_eq!(
            discover_paths(workspace.path()),
            [
                "README.md",
                "src/",
                "src/components/",
                "src/components/file_finder.rs",
                "src/lib.rs"
            ]
        );
    }

    #[test]
    fn ranking_prefers_tight_matches_and_keeps_input_order_for_ties() {
        let items = [
            "src/components/file_finder.rs",
            "src/lib.rs",
            "lib/b",
            "lib/a",
        ];
        assert_eq!(rank(&items, "LIB", |item| item), [2, 3, 1]);
        assert_eq!(rank(&items, "", |item| item), [0, 1, 2, 3]);
        assert_eq!(rank(&items, "sfr", |item| item), [0]);
        assert!(rank(&items, "zzz", |item| item).is_empty());
    }

    #[test]
    fn file_search_ranks_workspace_paths() {
        let workspace = workspace();

        assert_eq!(
            FileMatches::search(workspace.path(), "ff").paths,
            ["src/components/file_finder.rs"]
        );
        assert_eq!(FileMatches::search(workspace.path(), "").paths.len(), 5);
    }
}
