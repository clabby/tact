use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    process::{Output, Stdio},
};
use tokio::{io::AsyncReadExt, process::Command};

const MAX_DIFF_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub(crate) struct ReviewRange {
    pub(crate) from: usize,
    pub(crate) to: usize,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct ReviewTarget {
    pub(super) index: usize,
    pub(super) kind: ReviewTargetKind,
    pub(super) short_id: String,
    pub(super) title: String,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ReviewTargetKind {
    Trunk,
    Commit,
    WorkingTree,
}

#[derive(Clone)]
struct RangePoint {
    target: ReviewTarget,
    revision: Option<String>,
}

#[derive(Clone)]
pub(super) struct ReviewContext {
    root: PathBuf,
    repository: String,
    trunk: Trunk,
    range_points: Vec<RangePoint>,
}

#[derive(Clone, Serialize)]
pub(super) struct DiffSnapshot {
    pub(super) patch: String,
    pub(super) repository: String,
    pub(super) scope: String,
    pub(super) base: String,
}

impl ReviewContext {
    pub(super) async fn load(workspace: &Path) -> Result<Self, DiffError> {
        let root = repository_root(workspace).await?;
        let trunk = resolve_trunk(&root).await?;
        let range_points = load_range_points(&root, &trunk).await?;
        let repository = root
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("repository")
            .to_owned();
        Ok(Self {
            root,
            repository,
            trunk,
            range_points,
        })
    }

    pub(super) fn repository(&self) -> &str {
        &self.repository
    }

    pub(super) fn trunk_name(&self) -> &str {
        &self.trunk.name
    }

    pub(super) fn range_targets(&self) -> Vec<ReviewTarget> {
        self.range_points
            .iter()
            .map(|point| point.target.clone())
            .collect()
    }

    pub(super) fn default_range(&self) -> ReviewRange {
        let to = self.range_points.len() - 1;
        ReviewRange { from: to - 1, to }
    }

    pub(super) fn full_range(&self) -> ReviewRange {
        ReviewRange {
            from: 0,
            to: self.range_points.len() - 1,
        }
    }

    pub(super) fn range_label(&self, range: ReviewRange) -> Result<String, DiffError> {
        self.validate_range(range)?;
        if range == self.default_range() {
            return Ok("Uncommitted changes".to_owned());
        }
        if range == self.full_range() {
            return Ok("Full branch".to_owned());
        }
        let from = &self.range_points[range.from].target;
        let to = &self.range_points[range.to].target;
        Ok(format!("{} → {}", target_label(from), target_label(to)))
    }

    pub(super) async fn collect(&self, range: ReviewRange) -> Result<DiffSnapshot, DiffError> {
        self.validate_range(range)?;
        let base = self.range_points[range.from]
            .revision
            .as_deref()
            .expect("a valid range cannot start at the working tree");
        let head = self.range_points[range.to].revision.as_deref();
        let output = match head {
            Some(head) => {
                git_output_limited(
                    &self.root,
                    [
                        "diff",
                        "--binary",
                        "--find-renames",
                        "--find-copies",
                        "--no-ext-diff",
                        base,
                        head,
                        "--",
                    ],
                    MAX_DIFF_BYTES,
                    0,
                )
                .await?
            }
            None => {
                git_output_limited(
                    &self.root,
                    [
                        "diff",
                        "--binary",
                        "--find-renames",
                        "--find-copies",
                        "--no-ext-diff",
                        base,
                        "--",
                    ],
                    MAX_DIFF_BYTES,
                    0,
                )
                .await?
            }
        };
        ensure_success(output.status, &output.stderr)?;
        let mut patch = String::from_utf8(output.stdout)?;

        if head.is_none() {
            append_untracked_files(&self.root, &mut patch).await?;
        }
        Ok(DiffSnapshot {
            patch,
            repository: self.repository.clone(),
            scope: self.range_label(range)?,
            base: base.to_owned(),
        })
    }

    fn validate_range(&self, range: ReviewRange) -> Result<(), DiffError> {
        if range.from < range.to && range.to < self.range_points.len() {
            return Ok(());
        }
        Err(DiffError::InvalidRange {
            from: range.from,
            to: range.to,
            target_count: self.range_points.len(),
        })
    }
}

fn target_label(target: &ReviewTarget) -> &str {
    match target.kind {
        ReviewTargetKind::WorkingTree => "Working tree",
        ReviewTargetKind::Trunk | ReviewTargetKind::Commit => &target.short_id,
    }
}

pub(super) async fn load(workspace: &Path) -> Result<ReviewContext, DiffError> {
    ReviewContext::load(workspace).await
}

async fn repository_root(workspace: &Path) -> Result<std::path::PathBuf, DiffError> {
    let output = git_output(workspace, ["rev-parse", "--show-toplevel"]).await?;
    if !output.status.success() {
        return Err(DiffError::NotRepository(workspace.to_owned()));
    }

    let root = String::from_utf8(output.stdout)?;
    Ok(std::path::PathBuf::from(root.trim()))
}

async fn resolve_trunk(root: &Path) -> Result<Trunk, DiffError> {
    for candidate in [
        "refs/remotes/origin/HEAD",
        "refs/remotes/upstream/HEAD",
        "main",
        "master",
    ] {
        if revision_exists(root, candidate).await? {
            return Ok(Trunk {
                name: candidate.to_owned(),
                merge_base: merge_base(root, candidate).await?,
            });
        }
    }

    Err(DiffError::BaseNotFound)
}

#[derive(Clone)]
struct Trunk {
    name: String,
    merge_base: String,
}

async fn load_range_points(root: &Path, trunk: &Trunk) -> Result<Vec<RangePoint>, DiffError> {
    let trunk_commit = commit_metadata(root, &trunk.merge_base).await?;
    let mut points = vec![RangePoint {
        target: ReviewTarget {
            index: 0,
            kind: ReviewTargetKind::Trunk,
            short_id: trunk_commit.short_id,
            title: format!("{} · {}", trunk.name, trunk_commit.title),
        },
        revision: Some(trunk.merge_base.clone()),
    }];
    let range = format!("{}..HEAD", trunk.merge_base);
    let output = git_output(
        root,
        [
            "log",
            "--first-parent",
            "--reverse",
            "--format=%H%x00%h%x00%s",
            &range,
        ],
    )
    .await?;
    ensure_success(output.status, &output.stderr)?;
    let commits = String::from_utf8(output.stdout)?;
    for line in commits.lines().filter(|line| !line.is_empty()) {
        let mut fields = line.splitn(3, '\0');
        let revision = fields.next().unwrap_or_default();
        let short_id = fields.next().unwrap_or_default();
        let title = fields.next().unwrap_or_default();
        if revision.is_empty() || short_id.is_empty() {
            return Err(DiffError::InvalidCommitMetadata);
        }
        points.push(RangePoint {
            target: ReviewTarget {
                index: points.len(),
                kind: ReviewTargetKind::Commit,
                short_id: short_id.to_owned(),
                title: title.to_owned(),
            },
            revision: Some(revision.to_owned()),
        });
    }
    points.push(RangePoint {
        target: ReviewTarget {
            index: points.len(),
            kind: ReviewTargetKind::WorkingTree,
            short_id: "WT".to_owned(),
            title: "Uncommitted changes".to_owned(),
        },
        revision: None,
    });
    Ok(points)
}

struct CommitMetadata {
    short_id: String,
    title: String,
}

async fn commit_metadata(root: &Path, revision: &str) -> Result<CommitMetadata, DiffError> {
    let output = git_output(root, ["show", "--no-patch", "--format=%h%x00%s", revision]).await?;
    ensure_success(output.status, &output.stderr)?;
    let value = String::from_utf8(output.stdout)?;
    let Some((short_id, title)) = value.trim().split_once('\0') else {
        return Err(DiffError::InvalidCommitMetadata);
    };
    Ok(CommitMetadata {
        short_id: short_id.to_owned(),
        title: title.to_owned(),
    })
}

async fn revision_exists(root: &Path, revision: &str) -> Result<bool, DiffError> {
    let output = git_output(root, ["rev-parse", "--verify", "--quiet", revision]).await?;
    Ok(output.status.success())
}

async fn merge_base(root: &Path, revision: &str) -> Result<String, DiffError> {
    let output = git_output(root, ["merge-base", revision, "HEAD"]).await?;
    if !output.status.success() {
        return Err(DiffError::InvalidBase(revision.to_owned()));
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

async fn append_untracked_files(root: &Path, patch: &mut String) -> Result<(), DiffError> {
    let output = git_output(root, ["ls-files", "--others", "--exclude-standard", "-z"]).await?;
    ensure_success(output.status, &output.stderr)?;

    for bytes in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let path = std::str::from_utf8(bytes)?;
        let output = git_output_limited(
            root,
            [
                "diff",
                "--binary",
                "--no-ext-diff",
                "--no-index",
                "--",
                null_device(),
                path,
            ],
            MAX_DIFF_BYTES.saturating_sub(patch.len()),
            patch.len(),
        )
        .await?;
        if output.status.code() != Some(1) && !output.status.success() {
            ensure_success(output.status, &output.stderr)?;
        }
        patch.push_str(&String::from_utf8(output.stdout)?);
    }
    Ok(())
}

async fn git_output<const N: usize>(
    root: &Path,
    arguments: [&str; N],
) -> Result<Output, DiffError> {
    Command::new("git")
        .args(arguments)
        .current_dir(root)
        .kill_on_drop(true)
        .output()
        .await
        .map_err(DiffError::StartGit)
}

async fn git_output_limited<const N: usize>(
    root: &Path,
    arguments: [&str; N],
    limit: usize,
    used: usize,
) -> Result<Output, DiffError> {
    let mut child = Command::new("git")
        .args(arguments)
        .current_dir(root)
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(DiffError::StartGit)?;
    let stdout = child.stdout.take().expect("piped git stdout must exist");
    let mut stderr = child.stderr.take().expect("piped git stderr must exist");
    let stderr_task = tokio::spawn(async move {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).await.map(|_| bytes)
    });
    let mut output = Vec::with_capacity(limit.min(64 * 1024));
    stdout
        .take(limit.saturating_add(1) as u64)
        .read_to_end(&mut output)
        .await
        .map_err(DiffError::ReadGit)?;
    if output.len() > limit {
        let _ = child.kill().await;
        let _ = child.wait().await;
        stderr_task.abort();
        return Err(DiffError::TooLarge {
            actual: used.saturating_add(output.len()),
            maximum: MAX_DIFF_BYTES,
        });
    }
    let status = child.wait().await.map_err(DiffError::WaitGit)?;
    let stderr = stderr_task
        .await
        .map_err(DiffError::GitOutputTask)?
        .map_err(DiffError::ReadGit)?;
    Ok(Output {
        status,
        stdout: output,
        stderr,
    })
}

fn ensure_success(status: std::process::ExitStatus, stderr: &[u8]) -> Result<(), DiffError> {
    if status.success() {
        return Ok(());
    }

    Err(DiffError::GitFailed(
        String::from_utf8_lossy(stderr).trim().to_owned(),
    ))
}

#[cfg(unix)]
const fn null_device() -> &'static str {
    "/dev/null"
}

#[cfg(windows)]
const fn null_device() -> &'static str {
    "NUL"
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum DiffError {
    #[error("failed to start git: {0}")]
    StartGit(#[source] std::io::Error),
    #[error("failed to read git output: {0}")]
    ReadGit(#[source] std::io::Error),
    #[error("failed to wait for git: {0}")]
    WaitGit(#[source] std::io::Error),
    #[error("git output task failed: {0}")]
    GitOutputTask(#[source] tokio::task::JoinError),
    #[error("git command failed: {0}")]
    GitFailed(String),
    #[error("review workspace is not in a Git repository: {0}")]
    NotRepository(std::path::PathBuf),
    #[error("could not determine the branch base; pass `base` explicitly")]
    BaseNotFound,
    #[error("could not find a merge base between HEAD and `{0}`")]
    InvalidBase(String),
    #[error("the selected review range {from}..{to} is invalid for {target_count} targets")]
    InvalidRange {
        from: usize,
        to: usize,
        target_count: usize,
    },
    #[error("git returned invalid commit metadata for the review range")]
    InvalidCommitMetadata,
    #[error("review diff is {actual} bytes, exceeding the {maximum}-byte limit")]
    TooLarge { actual: usize, maximum: usize },
    #[error("git output was not valid UTF-8: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),
    #[error("git returned a path that is not valid UTF-8: {0}")]
    PathUtf8(#[from] std::str::Utf8Error),
}

#[cfg(test)]
mod tests {
    use super::{ReviewRange, ReviewTargetKind, load};
    use std::{fs, path::Path, process::Command};
    use tempfile::TempDir;

    #[tokio::test]
    async fn uncommitted_scope_includes_tracked_and_untracked_files() {
        let repository = repository();
        fs::write(repository.path().join("tracked.txt"), "changed\n").unwrap();
        fs::write(repository.path().join("new.txt"), "new\n").unwrap();

        let context = load(repository.path()).await.unwrap();
        let snapshot = context.collect(context.default_range()).await.unwrap();

        assert!(snapshot.patch.contains("tracked.txt"));
        assert!(snapshot.patch.contains("new.txt"));
        assert!(snapshot.patch.contains("+changed"));
        assert!(snapshot.patch.contains("+new"));
    }

    #[tokio::test]
    async fn branch_scope_starts_at_the_merge_base() {
        let repository = repository();
        git(repository.path(), ["checkout", "--quiet", "-b", "feature"]);
        fs::write(repository.path().join("tracked.txt"), "feature\n").unwrap();
        git(repository.path(), ["add", "tracked.txt"]);
        git(repository.path(), ["commit", "--quiet", "-m", "feature"]);

        let context = load(repository.path()).await.unwrap();
        let snapshot = context.collect(context.full_range()).await.unwrap();

        assert!(snapshot.patch.contains("+feature"));
        assert_ne!(snapshot.base, "HEAD");
    }

    #[tokio::test]
    async fn any_interval_between_trunk_commits_and_working_tree_can_be_selected() {
        let repository = repository();
        git(repository.path(), ["checkout", "--quiet", "-b", "feature"]);
        fs::write(repository.path().join("first.txt"), "first\n").unwrap();
        git(repository.path(), ["add", "first.txt"]);
        git(
            repository.path(),
            ["commit", "--quiet", "-m", "first change"],
        );
        fs::write(repository.path().join("second.txt"), "second\n").unwrap();
        git(repository.path(), ["add", "second.txt"]);
        git(
            repository.path(),
            ["commit", "--quiet", "-m", "second change"],
        );
        fs::write(repository.path().join("working.txt"), "working\n").unwrap();

        let context = load(repository.path()).await.unwrap();
        let targets = context.range_targets();
        assert_eq!(targets.len(), 4);
        assert!(matches!(targets[0].kind, ReviewTargetKind::Trunk));
        assert_eq!(targets[1].title, "first change");
        assert_eq!(targets[2].title, "second change");
        assert!(matches!(targets[3].kind, ReviewTargetKind::WorkingTree));

        let committed = context
            .collect(ReviewRange { from: 1, to: 2 })
            .await
            .unwrap();
        assert!(!committed.patch.contains("first.txt"));
        assert!(committed.patch.contains("second.txt"));
        assert!(!committed.patch.contains("working.txt"));

        let through_working_tree = context
            .collect(ReviewRange { from: 2, to: 3 })
            .await
            .unwrap();
        assert!(through_working_tree.patch.contains("working.txt"));
        assert!(!through_working_tree.patch.contains("second.txt"));
    }

    #[tokio::test]
    async fn reversed_or_empty_ranges_are_rejected() {
        let repository = repository();
        let context = load(repository.path()).await.unwrap();

        for range in [
            ReviewRange { from: 0, to: 0 },
            ReviewRange { from: 1, to: 0 },
        ] {
            assert!(matches!(
                context.collect(range).await,
                Err(super::DiffError::InvalidRange { .. })
            ));
        }
    }

    fn repository() -> TempDir {
        let directory = TempDir::new().unwrap();
        git(
            directory.path(),
            ["init", "--quiet", "--initial-branch=main"],
        );
        git(
            directory.path(),
            ["config", "user.email", "test@example.com"],
        );
        git(directory.path(), ["config", "user.name", "Test User"]);
        git(directory.path(), ["config", "commit.gpgSign", "false"]);
        fs::write(directory.path().join("tracked.txt"), "initial\n").unwrap();
        git(directory.path(), ["add", "tracked.txt"]);
        git(directory.path(), ["commit", "--quiet", "-m", "initial"]);
        directory
    }

    fn git<const N: usize>(root: &Path, arguments: [&str; N]) {
        let status = Command::new("git")
            .args(arguments)
            .current_dir(root)
            .status()
            .unwrap();
        assert!(status.success());
    }
}
