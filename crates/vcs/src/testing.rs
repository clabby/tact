//! Repository fixtures and patch summaries shared by this crate's tests.

use crate::FilePatch;
use std::{collections::BTreeMap, fs, path::Path, process::Command};
use tempfile::TempDir;

/// Runs a version control command and fails the test if it does not succeed.
pub(crate) fn run(program: &str, directory: &Path, arguments: &[&str]) {
    let output = Command::new(program)
        .args(arguments)
        .current_dir(directory)
        .env("JJ_USER", "Test User")
        .env("JJ_EMAIL", "test@example.com")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{program} {arguments:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

pub(crate) fn git(directory: &Path, arguments: &[&str]) {
    run("git", directory, arguments);
}

/// Initializes a git repository on `branch` with an identity, without commits.
pub(crate) fn init_git(directory: &Path, branch: &str) {
    git(
        directory,
        &["init", "--quiet", &format!("--initial-branch={branch}")],
    );
    for (key, value) in [
        ("user.email", "test@example.com"),
        ("user.name", "Test User"),
        ("commit.gpgSign", "false"),
    ] {
        git(directory, &["config", key, value]);
    }
}

/// Writes `name` and commits everything in the working tree.
pub(crate) fn commit_file(directory: &Path, name: &str, contents: &str, message: &str) {
    fs::write(directory.join(name), contents).unwrap();
    git(directory, &["add", "."]);
    git(directory, &["commit", "--quiet", "-m", message]);
}

/// A git repository on `main` whose only commit adds `tracked.txt`.
pub(crate) fn repository() -> TempDir {
    repository_with_initial_branch("main")
}

pub(crate) fn repository_with_initial_branch(branch: &str) -> TempDir {
    let directory = TempDir::new().unwrap();
    init_git(directory.path(), branch);
    commit_file(directory.path(), "tracked.txt", "initial\n", "initial");
    directory
}

/// The lines each file in `patch` adds, keyed by the file's path.
pub(crate) fn added_lines(patch: &str) -> BTreeMap<String, Vec<String>> {
    FilePatch::parse_all(patch)
        .iter()
        .map(|file| {
            let path = file.path().unwrap_or_default().to_owned();
            let lines = file
                .hunks
                .iter()
                .flat_map(|hunk| hunk.added())
                .map(str::to_owned)
                .collect();
            (path, lines)
        })
        .collect()
}

/// The expected value of [`added_lines`], written as literals.
pub(crate) fn expected_lines(files: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
    files
        .iter()
        .map(|(path, lines)| {
            (
                (*path).to_owned(),
                lines.iter().map(|line| (*line).to_owned()).collect(),
            )
        })
        .collect()
}
