use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    process::{Output, Stdio},
};
use tokio::{io::AsyncReadExt, process::Command};

const MAX_DIFF_BYTES: usize = 32 * 1024 * 1024;
const FULL_CONTEXT: &str = "--unified=2147483647";

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
    version: WorkspaceVersion,
}

#[derive(Clone, Serialize)]
pub(super) struct DiffSnapshot {
    pub(super) patch: String,
    #[serde(skip)]
    pub(super) overview_patch: String,
    pub(super) repository: String,
    pub(super) scope: String,
    pub(super) base: String,
}

#[derive(Clone, Copy)]
pub(super) enum PatchSide {
    Additions,
    Deletions,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct WorkspaceVersion([u8; 32]);

impl ReviewContext {
    pub(super) async fn load(workspace: &Path) -> Result<Self, DiffError> {
        let root = repository_root(workspace).await?;
        let trunk = resolve_trunk(&root).await?;
        let version = workspace_version_at(&root, &trunk).await?;
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
            version,
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
        self.full_range()
    }

    fn uncommitted_range(&self) -> ReviewRange {
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
        if range == self.uncommitted_range() {
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
        let Some(head) = self.range_points[range.to].revision.as_deref() else {
            return self.collect_working_tree(base, range).await;
        };
        let overview_patch = committed_patch(&self.root, base, head, false).await?;
        let patch = committed_patch(&self.root, base, head, true).await?;

        Ok(DiffSnapshot {
            patch,
            overview_patch,
            repository: self.repository.clone(),
            scope: self.range_label(range)?,
            base: base.to_owned(),
        })
    }

    pub(super) fn version(&self) -> WorkspaceVersion {
        self.version.clone()
    }

    async fn collect_working_tree(
        &self,
        base: &str,
        range: ReviewRange,
    ) -> Result<DiffSnapshot, DiffError> {
        for _ in 0..3 {
            let overview_patch = working_tree_patch(&self.root, base, false).await?;
            let patch = working_tree_patch(&self.root, base, true).await?;
            if working_tree_patch(&self.root, base, false).await? == overview_patch {
                return Ok(DiffSnapshot {
                    patch,
                    overview_patch,
                    repository: self.repository.clone(),
                    scope: self.range_label(range)?,
                    base: base.to_owned(),
                });
            }
        }
        Err(DiffError::WorkspaceChangedDuringSnapshot)
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

impl DiffSnapshot {
    pub(super) fn contains_anchor(
        &self,
        path: &str,
        side: PatchSide,
        start_line: u32,
        end_line: u32,
    ) -> bool {
        if start_line == 0 || end_line < start_line {
            return false;
        }

        let mut old_path = None;
        let mut new_path = None;
        for line in self.patch.lines() {
            if line.starts_with("diff --git ") {
                old_path = None;
                new_path = None;
                continue;
            }
            if let Some(value) = line.strip_prefix("--- ") {
                old_path = patch_path(value);
                continue;
            }
            if let Some(value) = line.strip_prefix("+++ ") {
                new_path = patch_path(value);
                continue;
            }
            let Some((old_start, old_count, new_start, new_count)) = parse_hunk_header(line) else {
                continue;
            };
            let (candidate_path, first, count) = match side {
                PatchSide::Additions => (new_path.as_deref(), new_start, new_count),
                PatchSide::Deletions => (old_path.as_deref(), old_start, old_count),
            };
            if candidate_path != Some(path) || count == 0 {
                continue;
            }
            let Some(last) = first.checked_add(count - 1) else {
                continue;
            };
            if start_line >= first && end_line <= last {
                return true;
            }
        }
        false
    }
}

fn patch_path(value: &str) -> Option<String> {
    let value = value.split('\t').next().unwrap_or(value);
    if value == "/dev/null" {
        return None;
    }
    let value = decode_git_path(value)?;
    Some(
        value
            .strip_prefix("a/")
            .or_else(|| value.strip_prefix("b/"))
            .unwrap_or(&value)
            .to_owned(),
    )
}

fn decode_git_path(value: &str) -> Option<String> {
    let Some(quoted) = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
    else {
        return Some(value.to_owned());
    };
    let bytes = quoted.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'\\' {
            decoded.push(bytes[index]);
            index += 1;
            continue;
        }
        index += 1;
        let escaped = *bytes.get(index)?;
        if escaped.is_ascii_digit() && escaped < b'8' {
            let mut value = 0_u8;
            let mut digits = 0;
            while digits < 3 {
                let Some(digit) = bytes.get(index).copied() else {
                    break;
                };
                if !(b'0'..=b'7').contains(&digit) {
                    break;
                }
                value = value.checked_mul(8)?.checked_add(digit - b'0')?;
                index += 1;
                digits += 1;
            }
            decoded.push(value);
            continue;
        }
        decoded.push(match escaped {
            b'a' => 0x07,
            b'b' => 0x08,
            b'f' => 0x0c,
            b'n' => b'\n',
            b'r' => b'\r',
            b't' => b'\t',
            b'v' => 0x0b,
            b'\\' => b'\\',
            b'"' => b'"',
            _ => return None,
        });
        index += 1;
    }
    String::from_utf8(decoded).ok()
}

fn parse_hunk_header(line: &str) -> Option<(u32, u32, u32, u32)> {
    let header = line.strip_prefix("@@ -")?;
    let (old, remainder) = header.split_once(" +")?;
    let (new, _) = remainder.split_once(" @@")?;
    let (old_start, old_count) = parse_hunk_range(old)?;
    let (new_start, new_count) = parse_hunk_range(new)?;
    Some((old_start, old_count, new_start, new_count))
}

fn parse_hunk_range(value: &str) -> Option<(u32, u32)> {
    let (start, count) = value.split_once(',').unwrap_or((value, "1"));
    Some((start.parse().ok()?, count.parse().ok()?))
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

pub(super) async fn current_version(workspace: &Path) -> Result<WorkspaceVersion, DiffError> {
    let root = repository_root(workspace).await?;
    let trunk = resolve_trunk(&root).await?;
    workspace_version_at(&root, &trunk).await
}

async fn workspace_version_at(root: &Path, trunk: &Trunk) -> Result<WorkspaceVersion, DiffError> {
    let output = git_output(root, ["rev-parse", "HEAD"]).await?;
    ensure_success(output.status, &output.stderr)?;
    let head = String::from_utf8(output.stdout)?.trim().to_owned();
    let patch = working_tree_patch(root, &head, false).await?;
    Ok(workspace_version(&trunk.merge_base, &head, &patch))
}

fn workspace_version(trunk: &str, head: &str, patch: &str) -> WorkspaceVersion {
    let mut digest = Sha256::new();
    for value in [trunk, head, patch] {
        digest.update(value.len().to_le_bytes());
        digest.update(value.as_bytes());
    }
    WorkspaceVersion(digest.finalize().into())
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

async fn committed_patch(
    root: &Path,
    base: &str,
    head: &str,
    full_context: bool,
) -> Result<String, DiffError> {
    let mut arguments = vec![
        "diff",
        "--binary",
        "--find-renames",
        "--find-copies",
        "--no-ext-diff",
    ];
    if full_context {
        arguments.push(FULL_CONTEXT);
    }
    arguments.extend([base, head, "--"]);
    let output = git_output_limited(root, arguments, MAX_DIFF_BYTES, 0).await?;
    ensure_success(output.status, &output.stderr)?;
    Ok(String::from_utf8(output.stdout)?)
}

async fn append_untracked_files(
    root: &Path,
    patch: &mut String,
    full_context: bool,
) -> Result<(), DiffError> {
    let output = git_output(root, ["ls-files", "--others", "--exclude-standard", "-z"]).await?;
    ensure_success(output.status, &output.stderr)?;

    for bytes in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let path = std::str::from_utf8(bytes)?;
        let mut arguments = vec!["diff", "--binary", "--no-ext-diff", "--no-index"];
        if full_context {
            arguments.push(FULL_CONTEXT);
        }
        arguments.extend(["--", null_device(), path]);
        let output = git_output_limited(
            root,
            arguments,
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

async fn working_tree_patch(
    root: &Path,
    base: &str,
    full_context: bool,
) -> Result<String, DiffError> {
    let mut arguments = vec![
        "diff",
        "--binary",
        "--find-renames",
        "--find-copies",
        "--no-ext-diff",
    ];
    if full_context {
        arguments.push(FULL_CONTEXT);
    }
    arguments.extend([base, "--"]);
    let output = git_output_limited(root, arguments, MAX_DIFF_BYTES, 0).await?;
    ensure_success(output.status, &output.stderr)?;
    let mut patch = String::from_utf8(output.stdout)?;
    append_untracked_files(root, &mut patch, full_context).await?;
    Ok(patch)
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

async fn git_output_limited<I, S>(
    root: &Path,
    arguments: I,
    limit: usize,
    used: usize,
) -> Result<Output, DiffError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
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
    #[error("workspace kept changing while the review snapshot was collected")]
    WorkspaceChangedDuringSnapshot,
    #[error("review diff is {actual} bytes, exceeding the {maximum}-byte limit")]
    TooLarge { actual: usize, maximum: usize },
    #[error("git output was not valid UTF-8: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),
    #[error("git returned a path that is not valid UTF-8: {0}")]
    PathUtf8(#[from] std::str::Utf8Error),
}

#[cfg(test)]
mod tests {
    use super::{DiffSnapshot, PatchSide, ReviewRange, ReviewTargetKind, current_version, load};
    use std::{fs, path::Path, process::Command};
    use tempfile::TempDir;

    #[tokio::test]
    async fn uncommitted_scope_includes_tracked_and_untracked_files() {
        let repository = repository();
        fs::write(repository.path().join("tracked.txt"), "changed\n").unwrap();
        fs::write(repository.path().join("new.txt"), "new\n").unwrap();

        let context = load(repository.path()).await.unwrap();
        let snapshot = context.collect(context.uncommitted_range()).await.unwrap();

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
    async fn review_patch_tracks_renames_and_binary_content() {
        let repository = repository();
        git(repository.path(), ["mv", "tracked.txt", "renamed.txt"]);

        let context = load(repository.path()).await.unwrap();
        let snapshot = context.collect(context.uncommitted_range()).await.unwrap();
        assert!(snapshot.patch.contains("rename from tracked.txt"));
        assert!(snapshot.patch.contains("rename to renamed.txt"));

        fs::write(repository.path().join("renamed.txt"), [0xff, 0x00]).unwrap();
        let snapshot = context.collect(context.uncommitted_range()).await.unwrap();
        assert!(snapshot.patch.contains("GIT binary patch"));
    }

    #[tokio::test]
    async fn review_patch_contains_full_git_context_for_expansion() {
        let repository = repository();
        let original = (1..=40)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        fs::write(repository.path().join("tracked.txt"), &original).unwrap();
        git(repository.path(), ["add", "tracked.txt"]);
        git(repository.path(), ["commit", "--quiet", "-m", "long file"]);
        let changed = original.replace("line 20\n", "changed line 20\n");
        fs::write(repository.path().join("tracked.txt"), changed).unwrap();

        let context = load(repository.path()).await.unwrap();
        let snapshot = context.collect(context.uncommitted_range()).await.unwrap();

        assert!(snapshot.patch.contains(" line 1\n"));
        assert!(snapshot.patch.contains(" line 40\n"));
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
        assert_eq!(context.default_range(), context.full_range());
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

    #[tokio::test]
    async fn workspace_version_detects_further_edits_to_an_already_modified_file() {
        let repository = repository();
        fs::write(repository.path().join("tracked.txt"), "first edit\n").unwrap();
        let context = load(repository.path()).await.unwrap();
        let _snapshot = context.collect(context.uncommitted_range()).await.unwrap();
        let initial = context.version();

        fs::write(repository.path().join("tracked.txt"), "second edit\n").unwrap();

        assert_ne!(current_version(repository.path()).await.unwrap(), initial);
    }

    #[tokio::test]
    async fn clean_feature_branch_snapshot_matches_the_current_workspace_version() {
        let repository = repository();
        git(repository.path(), ["checkout", "--quiet", "-b", "feature"]);
        fs::write(repository.path().join("tracked.txt"), "feature\n").unwrap();
        git(repository.path(), ["add", "tracked.txt"]);
        git(repository.path(), ["commit", "--quiet", "-m", "feature"]);

        let context = load(repository.path()).await.unwrap();
        let _snapshot = context.collect(context.full_range()).await.unwrap();

        assert_eq!(
            context.version(),
            current_version(repository.path()).await.unwrap()
        );
    }

    #[test]
    fn comment_anchors_decode_git_quoted_paths() {
        let snapshot = DiffSnapshot {
            patch: concat!(
                "diff --git \"a/caf\\303\\251.rs\" \"b/caf\\303\\251.rs\"\n",
                "--- \"a/caf\\303\\251.rs\"\n",
                "+++ \"b/caf\\303\\251.rs\"\n",
                "@@ -1 +1 @@\n",
                "-old\n",
                "+new\n",
            )
            .to_owned(),
            overview_patch: "-old\n+new\n".to_owned(),
            repository: "repo".to_owned(),
            scope: "Uncommitted changes".to_owned(),
            base: "HEAD".to_owned(),
        };

        assert!(snapshot.contains_anchor("café.rs", PatchSide::Additions, 1, 1));
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
