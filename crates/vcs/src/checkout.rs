//! Checkouts of a repository: how to point git at one, and which others share its repository.
//!
//! A checkout is a directory holding a working copy: a git repository, a git worktree, a colocated
//! jj repository, or a jj workspace. Review reads all of them with the same git commands. A jj
//! workspace has no `.git`, so git is pointed at the repository's git store with an explicit work
//! tree and a throwaway index; nothing in the user's repository is written.

use crate::VcsError;
use serde::Serialize;
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    process::Stdio,
};
use tempfile::TempDir;
use tokio::process::Command;

/// The directories of every checkout in the repository that contains `directory`, including
/// `directory` itself. A directory outside any repository is its own only member.
pub async fn family_paths(directory: &Path) -> Vec<PathBuf> {
    let Ok(checkout) = Checkout::detect(directory).await else {
        return vec![directory.to_owned()];
    };
    let mut paths = checkout
        .family()
        .await
        .into_iter()
        .map(|member| member.path)
        .collect::<Vec<_>>();
    if !paths.iter().any(|path| path == directory) {
        paths.push(directory.to_owned());
    }
    paths
}

/// A directory holding one working copy of a repository.
pub struct Checkout {
    work_tree: PathBuf,
    view: GitView,
}

/// How git reaches the checkout's repository.
enum GitView {
    /// A `.git` is discoverable from the work tree, so git needs no help.
    Ambient,
    /// A jj workspace without `.git`: the git store that backs the repository, the commit the
    /// working copy is based on, and an index seeded from that commit's tree. The index is private
    /// to this value and removed with it.
    Jj {
        git_dir: PathBuf,
        head: String,
        index: TempDir,
    },
}

impl Checkout {
    /// Finds the checkout that contains `path`.
    ///
    /// The nearest ancestor with a `.git` or `.jj` decides. A `.git` there means git can be used
    /// directly, including a colocated jj repository; a lone `.jj` means a jj workspace.
    pub async fn detect(path: &Path) -> Result<Self, VcsError> {
        let marker = path.ancestors().find_map(|directory| {
            let git = directory.join(".git").exists();
            let jj = directory.join(".jj").exists();
            (git || jj).then(|| (directory.to_owned(), git))
        });
        match marker {
            Some((_, true)) | None => Self::detect_git(path).await,
            Some((root, false)) => Self::detect_jj(root).await,
        }
    }

    async fn detect_git(path: &Path) -> Result<Self, VcsError> {
        let output = git_at(path)
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .await
            .map_err(VcsError::StartGit)?;
        if !output.status.success() {
            let error = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            if error.contains("not a git repository") {
                return Err(VcsError::NotRepository(path.to_owned()));
            }
            return Err(VcsError::GitFailed(error));
        }
        let root = String::from_utf8(output.stdout)?;
        Ok(Self {
            work_tree: PathBuf::from(root.trim()),
            view: GitView::Ambient,
        })
    }

    async fn detect_jj(root: PathBuf) -> Result<Self, VcsError> {
        let git_dir = PathBuf::from(
            jj(&root, ["git", "root"])
                .await?
                .trim_end_matches(['\n', '\r']),
        );
        let head = jj(&root, ["log", "-r", "@-", "--no-graph", "-T", "commit_id"])
            .await?
            .trim()
            .to_owned();
        let index = TempDir::new().map_err(VcsError::Scratch)?;
        let seeded = Command::new("git")
            .args(["read-tree", &head])
            .env("GIT_DIR", &git_dir)
            .env("GIT_INDEX_FILE", index.path().join("index"))
            .output()
            .await
            .map_err(VcsError::StartGit)?;
        if !seeded.status.success() {
            return Err(VcsError::GitFailed(
                String::from_utf8_lossy(&seeded.stderr).trim().to_owned(),
            ));
        }
        Ok(Self {
            work_tree: root,
            view: GitView::Jj {
                git_dir,
                head,
                index,
            },
        })
    }

    pub(crate) fn work_tree(&self) -> &Path {
        &self.work_tree
    }

    /// The commit the working copy is based on: git's `HEAD`, or a jj workspace's parent commit.
    pub(crate) fn head(&self) -> &str {
        match &self.view {
            GitView::Ambient => "HEAD",
            GitView::Jj { head, .. } => head,
        }
    }

    /// A git command that runs against this checkout.
    pub(crate) fn git(&self) -> Command {
        let mut command = git_at(&self.work_tree);
        if let GitView::Jj { git_dir, index, .. } = &self.view {
            command
                .env("GIT_DIR", git_dir)
                .env("GIT_WORK_TREE", &self.work_tree)
                .env("GIT_INDEX_FILE", index.path().join("index"));
        }
        command
    }

    /// Whether a path belongs to the version control system's own bookkeeping rather than the
    /// working copy.
    pub(crate) fn is_bookkeeping(path: &str) -> bool {
        [".jj", ".git"]
            .iter()
            .any(|name| path == *name || path.starts_with(&format!("{name}/")))
    }

    /// How many files differ from the head commit, counting files git does not track yet. `None`
    /// when git cannot say.
    pub async fn changed_files(&self) -> Option<usize> {
        let changed = self
            .git()
            .args(["diff", "--name-only", "-z", self.head(), "--"])
            .output()
            .await
            .ok()?;
        let untracked = self
            .git()
            .args(["ls-files", "--others", "--exclude-standard", "-z"])
            .output()
            .await
            .ok()?;
        if !changed.status.success() || !untracked.status.success() {
            return None;
        }
        let names = changed
            .stdout
            .split(|byte| *byte == 0)
            .chain(untracked.stdout.split(|byte| *byte == 0))
            .filter(|name| !name.is_empty())
            .filter_map(|name| std::str::from_utf8(name).ok())
            .filter(|name| !Self::is_bookkeeping(name))
            .collect::<std::collections::HashSet<_>>();
        Some(names.len())
    }

    /// Every checkout of the same repository, this one included, main checkout first.
    pub async fn family(&self) -> Vec<FamilyMember> {
        let mut members = Vec::new();
        if self.work_tree.join(".jj").exists() || matches!(self.view, GitView::Jj { .. }) {
            members.extend(jj_workspaces(&self.work_tree).await);
        }
        if matches!(self.view, GitView::Ambient) {
            for member in git_worktrees(&self.work_tree).await {
                if !members.iter().any(|known| known.path == member.path) {
                    members.push(member);
                }
            }
        }
        if !members.iter().any(|member| member.path == self.work_tree) {
            members.insert(
                0,
                FamilyMember {
                    path: self.work_tree.clone(),
                    label: self
                        .work_tree
                        .file_name()
                        .map_or_else(String::new, |name| name.to_string_lossy().into_owned()),
                    kind: CheckoutKind::Git,
                    missing: false,
                },
            );
        }
        members
    }
}

/// One checkout of a repository, as listed to the user.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FamilyMember {
    pub path: PathBuf,
    /// The branch, or the jj workspace name.
    pub label: String,
    pub kind: CheckoutKind,
    /// The directory no longer exists, for example a worktree that was deleted without pruning.
    pub missing: bool,
}

impl FamilyMember {
    /// A checkout known only by its directory, such as one whose repository could not be read.
    pub fn standalone(path: &Path) -> Self {
        Self {
            path: path.to_owned(),
            label: path
                .file_name()
                .map_or_else(String::new, |name| name.to_string_lossy().into_owned()),
            kind: CheckoutKind::Git,
            missing: !path.exists(),
        }
    }
}

/// The version control system that lists a checkout.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckoutKind {
    Git,
    Jj,
}

fn git_at(directory: &Path) -> Command {
    let mut command = Command::new("git");
    command.current_dir(directory).kill_on_drop(true);
    command
}

/// Runs `jj` in `directory` without snapshotting the working copy, so reading never writes.
async fn jj<const N: usize>(directory: &Path, arguments: [&str; N]) -> Result<String, VcsError> {
    jj_dynamic(directory, &arguments).await
}

async fn jj_dynamic<S: AsRef<OsStr>>(
    directory: &Path,
    arguments: &[S],
) -> Result<String, VcsError> {
    let output = Command::new("jj")
        .arg("--ignore-working-copy")
        .args(arguments)
        .current_dir(directory)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => VcsError::JjMissing,
            _ => VcsError::StartGit(error),
        })?;
    if !output.status.success() {
        return Err(VcsError::Jj(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    Ok(String::from_utf8(output.stdout)?)
}

/// The jj workspaces of the repository `directory` belongs to. A failure yields none: the list
/// only decorates a checkout that has already been read successfully.
async fn jj_workspaces(directory: &Path) -> Vec<FamilyMember> {
    let Ok(names) = jj(directory, ["workspace", "list", "-T", "name ++ \"\\n\""]).await else {
        return Vec::new();
    };
    let mut members = Vec::new();
    for name in names.lines().filter(|name| !name.is_empty()) {
        let Ok(root) = jj(directory, ["workspace", "root", "--name", name]).await else {
            continue;
        };
        let path = PathBuf::from(root.trim_end_matches(['\n', '\r']));
        members.push(FamilyMember {
            missing: !path.exists(),
            path,
            label: name.to_owned(),
            kind: CheckoutKind::Jj,
        });
    }
    members
}

/// The git worktrees of the repository `directory` belongs to, from `git worktree list`.
async fn git_worktrees(directory: &Path) -> Vec<FamilyMember> {
    let Ok(output) = git_at(directory)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .await
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    parse_worktrees(&String::from_utf8_lossy(&output.stdout))
}

fn parse_worktrees(listing: &str) -> Vec<FamilyMember> {
    listing
        .split("\n\n")
        .filter_map(|block| {
            let mut path = None;
            let mut label = None;
            let mut head = None;
            let mut bare = false;
            for line in block.lines() {
                if let Some(value) = line.strip_prefix("worktree ") {
                    path = Some(PathBuf::from(value));
                } else if let Some(value) = line.strip_prefix("branch ") {
                    label = Some(
                        value
                            .strip_prefix("refs/heads/")
                            .unwrap_or(value)
                            .to_owned(),
                    );
                } else if let Some(value) = line.strip_prefix("HEAD ") {
                    head = Some(value.chars().take(7).collect::<String>());
                } else if line == "bare" {
                    bare = true;
                }
            }
            let path = path.filter(|_| !bare)?;
            Some(FamilyMember {
                missing: !path.exists(),
                label: label.or(head).unwrap_or_default(),
                kind: CheckoutKind::Git,
                path,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Checkout, CheckoutKind, parse_worktrees};
    use crate::{
        ReviewContext, WorkspaceVersion,
        testing::{added_lines, commit_file, expected_lines, init_git, run},
    };
    use std::{
        fs,
        path::{Path, PathBuf},
        process::Command,
    };
    use tempfile::TempDir;

    #[derive(Clone, Copy, Debug)]
    enum Kind {
        Git,
        GitWorktree,
        JjColocated,
        JjWorkspace,
    }

    const KINDS: [Kind; 4] = [
        Kind::Git,
        Kind::GitWorktree,
        Kind::JjColocated,
        Kind::JjWorkspace,
    ];

    /// A repository with a `main` trunk, one `feature` commit on top of it, and the directory to
    /// review. Returns nothing for the jj kinds when `jj` is not installed.
    fn fixture(kind: Kind) -> Option<(TempDir, PathBuf)> {
        let jj_kind = matches!(kind, Kind::JjColocated | Kind::JjWorkspace);
        if jj_kind && Command::new("jj").arg("--version").output().is_err() {
            return None;
        }
        let directory = TempDir::new().unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();
        let repository = root.join("repository");
        fs::create_dir(&repository).unwrap();
        if jj_kind {
            run("jj", &repository, &["git", "init", "--colocate", "."]);
            fs::write(repository.join("tracked.txt"), "initial\n").unwrap();
            run("jj", &repository, &["commit", "-m", "initial"]);
            run(
                "jj",
                &repository,
                &["bookmark", "create", "main", "-r", "@-"],
            );
            fs::write(repository.join("feature.txt"), "feature\n").unwrap();
            run("jj", &repository, &["commit", "-m", "feature"]);
        } else {
            init_git(&repository, "main");
            commit_file(&repository, "tracked.txt", "initial\n", "initial");
            run(
                "git",
                &repository,
                &["checkout", "--quiet", "-b", "feature"],
            );
            commit_file(&repository, "feature.txt", "feature\n", "feature");
        }
        let path = match kind {
            Kind::Git | Kind::JjColocated => repository,
            Kind::GitWorktree => {
                let worktree = root.join("worktree");
                let worktree_name = worktree.to_string_lossy().into_owned();
                run(
                    "git",
                    &repository,
                    &["worktree", "add", "--quiet", "-b", "other", &worktree_name],
                );
                worktree
            }
            Kind::JjWorkspace => {
                let workspace = root.join("workspace");
                let workspace_name = workspace.to_string_lossy().into_owned();
                run("jj", &repository, &["workspace", "add", &workspace_name]);
                workspace
            }
        };
        Some((directory, path))
    }

    #[test]
    fn worktree_listing_names_branches_and_detached_heads_and_skips_bare_entries() {
        let listing = "worktree /repo\nHEAD 1111111aaaa\nbranch refs/heads/main\n\n\
                       worktree /repo-feature\nHEAD 2222222bbbb\ndetached\n\n\
                       worktree /repo.git\nbare\n";

        let members = parse_worktrees(listing);

        assert_eq!(
            members
                .iter()
                .map(|member| (member.path.clone(), member.label.as_str(), member.kind))
                .collect::<Vec<_>>(),
            [
                (PathBuf::from("/repo"), "main", CheckoutKind::Git),
                (PathBuf::from("/repo-feature"), "2222222", CheckoutKind::Git),
            ]
        );
    }

    #[tokio::test]
    async fn every_kind_of_checkout_reviews_the_same_way() {
        for kind in KINDS {
            let Some((_directory, path)) = fixture(kind) else {
                continue;
            };
            fs::write(path.join("tracked.txt"), "edited\n").unwrap();
            fs::write(path.join("new.txt"), "new\n").unwrap();

            let context = ReviewContext::load(&path)
                .await
                .unwrap_or_else(|error| panic!("{kind:?}: {error}"));
            let snapshot = context.collect(context.full_range()).await.unwrap();

            assert_eq!(
                added_lines(&snapshot.patch),
                expected_lines(&[
                    ("feature.txt", &["feature"]),
                    ("new.txt", &["new"]),
                    ("tracked.txt", &["edited"]),
                ]),
                "{kind:?}"
            );
            assert_eq!(context.trunk_name(), "main", "{kind:?}");
        }
    }

    #[tokio::test]
    async fn every_kind_of_checkout_notices_a_change() {
        for kind in KINDS {
            let Some((_directory, path)) = fixture(kind) else {
                continue;
            };
            let before = WorkspaceVersion::current(&path).await.unwrap();
            assert_eq!(
                before,
                WorkspaceVersion::current(&path).await.unwrap(),
                "{kind:?}"
            );

            fs::write(path.join("tracked.txt"), "again\n").unwrap();

            assert_ne!(
                before,
                WorkspaceVersion::current(&path).await.unwrap(),
                "{kind:?}"
            );
        }
    }

    #[tokio::test]
    async fn reviewing_does_not_touch_the_repository() {
        for kind in [Kind::JjWorkspace, Kind::JjColocated] {
            let Some((_directory, path)) = fixture(kind) else {
                continue;
            };
            let log = |directory: &Path| {
                let output = Command::new("jj")
                    .args([
                        "--ignore-working-copy",
                        "op",
                        "log",
                        "--no-graph",
                        "-T",
                        "id",
                    ])
                    .current_dir(directory)
                    .output()
                    .unwrap();
                String::from_utf8(output.stdout).unwrap()
            };
            let before = log(&path);

            ReviewContext::load(&path).await.unwrap();

            assert_eq!(before, log(&path), "{kind:?}");
        }
    }

    #[tokio::test]
    async fn a_checkout_lists_the_other_checkouts_of_its_repository() {
        for (kind, expected) in [
            (Kind::GitWorktree, 2),
            (Kind::JjWorkspace, 2),
            (Kind::Git, 1),
        ] {
            let Some((_directory, path)) = fixture(kind) else {
                continue;
            };

            let checkout = Checkout::detect(&path).await.unwrap();
            let members = checkout.family().await;

            assert_eq!(members.len(), expected, "{kind:?}: {members:?}");
            assert!(
                members.iter().any(|member| member.path == path),
                "{kind:?}: {members:?}"
            );
        }
    }

    #[test]
    fn bookkeeping_directories_are_recognised() {
        assert!(Checkout::is_bookkeeping(".jj/repo"));
        assert!(Checkout::is_bookkeeping(".git"));
        assert!(!Checkout::is_bookkeeping(".github/workflows/ci.yml"));
        assert!(!Checkout::is_bookkeeping("src/.jj.rs"));
    }
}
