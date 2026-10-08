//! Changes in a checkout, captured for review.
//!
//! A review context lists the selectable ranges from trunk through each commit to the working
//! tree. A range resolves to one full-context patch, and a content version detects when the
//! checkout changed underneath an open review.

use crate::{Checkout, FilePatch, VcsError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    process::{Output, Stdio},
    sync::Arc,
};
use tokio::io::AsyncReadExt;

#[cfg(test)]
type PatchHook = std::sync::Arc<dyn Fn(&Path, bool, PatchHookPhase) + Send + Sync>;

#[cfg(test)]
static PATCH_HOOK: std::sync::RwLock<Option<PatchHook>> = std::sync::RwLock::new(None);

#[cfg(test)]
static PATCH_HOOK_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
#[derive(Clone, Copy)]
enum PatchHookPhase {
    Before,
    After,
}

const MAX_DIFF_BYTES: usize = 32 * 1024 * 1024;
const FULL_CONTEXT: &str = "--unified=2147483647";

/// An interval between two review targets, as indices into [`ReviewContext::range_targets`].
/// A valid range has `from < to`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct ReviewRange {
    pub from: usize,
    pub to: usize,
}

/// A point a review range can start or end at.
#[derive(Clone, Debug, Serialize)]
pub struct ReviewTarget {
    pub index: usize,
    pub kind: ReviewTargetKind,
    pub short_id: String,
    pub title: String,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewTargetKind {
    /// The merge base with the trunk branch; always the first target.
    Trunk,
    /// A first-parent commit between the merge base and the checkout's head.
    Commit,
    /// The uncommitted state of the checkout; always the last target.
    WorkingTree,
}

#[derive(Clone)]
struct RangePoint {
    target: ReviewTarget,
    revision: Option<String>,
}

/// The reviewable targets of one checkout, read once. Cloning shares the underlying checkout.
#[derive(Clone)]
pub struct ReviewContext {
    root: Arc<Checkout>,
    repository: String,
    trunk: Trunk,
    range_points: Vec<RangePoint>,
    version: WorkspaceVersion,
}

/// The change across one review range.
#[derive(Clone, Serialize)]
pub struct DiffSnapshot {
    /// A `git diff` patch with full file context, untracked files included.
    pub patch: String,
    #[serde(skip)]
    pub overview: OverviewContext,
    /// The name of the checkout's directory.
    pub repository: String,
    /// A human-readable label for the range.
    pub scope: String,
    /// The commit the patch starts from.
    pub base: String,
}

/// Where a snapshot came from, so an agent can inspect the same change in the repository.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OverviewContext {
    pub repository: PathBuf,
    pub range: OverviewRange,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OverviewRange {
    Commits { base: String, head: String },
    WorkingTree { base: String },
}

/// Which side of a patch a line number refers to.
#[derive(Clone, Copy)]
pub enum PatchSide {
    Additions,
    Deletions,
}

/// A content hash of a checkout's trunk merge base, head commit, and working-tree changes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceVersion([u8; 32]);

impl WorkspaceVersion {
    /// Reads the current version of the checkout that contains `workspace`.
    pub async fn current(workspace: &Path) -> Result<Self, VcsError> {
        let root = Checkout::detect(workspace).await?;
        let trunk = resolve_trunk(&root).await?;
        workspace_version_at(&root, &trunk).await
    }

    /// An opaque, comparable rendering for clients.
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}

impl ReviewContext {
    /// Reads the targets of the checkout that contains `workspace`.
    pub async fn load(workspace: &Path) -> Result<Self, VcsError> {
        let root = Checkout::detect(workspace).await?;
        let trunk = resolve_trunk(&root).await?;
        let version = workspace_version_at(&root, &trunk).await?;
        let range_points = load_range_points(&root, &trunk).await?;
        let repository = root
            .work_tree()
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("repository")
            .to_owned();
        Ok(Self {
            root: Arc::new(root),
            repository,
            trunk,
            range_points,
            version,
        })
    }

    pub fn repository(&self) -> &str {
        &self.repository
    }

    pub fn trunk_name(&self) -> &str {
        &self.trunk.name
    }

    /// Trunk first, then each commit oldest first, then the working tree.
    pub fn range_targets(&self) -> Vec<ReviewTarget> {
        self.range_points
            .iter()
            .map(|point| point.target.clone())
            .collect()
    }

    /// The range a review opens with: the whole branch.
    pub fn default_range(&self) -> ReviewRange {
        self.full_range()
    }

    fn uncommitted_range(&self) -> ReviewRange {
        let to = self.range_points.len() - 1;
        ReviewRange { from: to - 1, to }
    }

    pub fn full_range(&self) -> ReviewRange {
        ReviewRange {
            from: 0,
            to: self.range_points.len() - 1,
        }
    }

    /// A short label for `range`, such as "Full branch".
    pub fn range_label(&self, range: ReviewRange) -> Result<String, VcsError> {
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

    /// Captures the patch for `range`. A range ending at the working tree is captured until two
    /// consecutive captures agree, so the patch never mixes two states.
    pub async fn collect(&self, range: ReviewRange) -> Result<DiffSnapshot, VcsError> {
        self.validate_range(range)?;
        let base = self.range_points[range.from]
            .revision
            .as_deref()
            .expect("a valid range cannot start at the working tree");
        let Some(head) = self.range_points[range.to].revision.as_deref() else {
            return self.collect_working_tree(base, range).await;
        };
        let patch = committed_patch(&self.root, base, head, true).await?;

        Ok(DiffSnapshot {
            patch,
            overview: OverviewContext {
                repository: self.root.work_tree().to_owned(),
                range: OverviewRange::Commits {
                    base: base.to_owned(),
                    head: head.to_owned(),
                },
            },
            repository: self.repository.clone(),
            scope: self.range_label(range)?,
            base: base.to_owned(),
        })
    }

    /// The checkout's version when this context was loaded.
    pub fn version(&self) -> WorkspaceVersion {
        self.version.clone()
    }

    async fn collect_working_tree(
        &self,
        base: &str,
        range: ReviewRange,
    ) -> Result<DiffSnapshot, VcsError> {
        for _ in 0..3 {
            let patch = working_tree_patch(&self.root, base, true).await?;
            if working_tree_patch(&self.root, base, true).await? != patch {
                continue;
            }
            return Ok(DiffSnapshot {
                patch,
                overview: OverviewContext {
                    repository: self.root.work_tree().to_owned(),
                    range: OverviewRange::WorkingTree {
                        base: base.to_owned(),
                    },
                },
                repository: self.repository.clone(),
                scope: self.range_label(range)?,
                base: base.to_owned(),
            });
        }
        Err(VcsError::WorkspaceChangedDuringSnapshot)
    }

    fn validate_range(&self, range: ReviewRange) -> Result<(), VcsError> {
        if range.from < range.to && range.to < self.range_points.len() {
            return Ok(());
        }
        Err(VcsError::InvalidRange {
            from: range.from,
            to: range.to,
            target_count: self.range_points.len(),
        })
    }
}

impl DiffSnapshot {
    /// Whether the lines `start_line` through `end_line` of `path` fall inside one hunk on the
    /// given side of the patch, so a comment anchored there points at reviewed content.
    pub fn contains_anchor(
        &self,
        path: &str,
        side: PatchSide,
        start_line: u32,
        end_line: u32,
    ) -> bool {
        FilePatch::parse_all(&self.patch).iter().any(|file| {
            let candidate = match side {
                PatchSide::Additions => file.new_path.as_deref(),
                PatchSide::Deletions => file.old_path.as_deref(),
            };
            candidate == Some(path)
                && file.hunks.iter().any(|hunk| {
                    let span = match side {
                        PatchSide::Additions => hunk.new,
                        PatchSide::Deletions => hunk.old,
                    };
                    span.contains(start_line, end_line)
                })
        })
    }
}

fn target_label(target: &ReviewTarget) -> &str {
    match target.kind {
        ReviewTargetKind::WorkingTree => "Working tree",
        ReviewTargetKind::Trunk | ReviewTargetKind::Commit => &target.short_id,
    }
}

async fn workspace_version_at(
    root: &Checkout,
    trunk: &Trunk,
) -> Result<WorkspaceVersion, VcsError> {
    let output = git_output(root, ["rev-parse", root.head()]).await?;
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

async fn resolve_trunk(root: &Checkout) -> Result<Trunk, VcsError> {
    let current_branch = current_branch(root).await?;
    let mut candidates = ["refs/remotes/origin/HEAD", "refs/remotes/upstream/HEAD"]
        .map(str::to_owned)
        .to_vec();
    if let Some(upstream) = current_upstream(root, current_branch.as_deref()).await? {
        candidates.push(upstream);
    }
    candidates.extend(["main", "master", "trunk", "develop"].map(str::to_owned));
    if let Some(current_branch) = current_branch {
        candidates.push(current_branch);
    }

    for candidate in candidates {
        if revision_exists(root, &candidate).await? {
            let merge_base = merge_base(root, &candidate).await?;
            let name = symbolic_ref(root, &candidate).await?.unwrap_or(candidate);
            return Ok(Trunk { merge_base, name });
        }
    }

    Err(VcsError::BaseNotFound)
}

async fn current_branch(root: &Checkout) -> Result<Option<String>, VcsError> {
    symbolic_ref(root, root.head()).await
}

async fn symbolic_ref(root: &Checkout, reference: &str) -> Result<Option<String>, VcsError> {
    let output = git_output(root, ["symbolic-ref", "--quiet", "--short", reference]).await?;
    if output.status.code() == Some(1) {
        return Ok(None);
    }
    ensure_success(output.status, &output.stderr)?;
    let target = String::from_utf8(output.stdout)?.trim().to_owned();
    Ok((!target.is_empty()).then_some(target))
}

async fn current_upstream(
    root: &Checkout,
    current_branch: Option<&str>,
) -> Result<Option<String>, VcsError> {
    let Some(current_branch) = current_branch else {
        return Ok(None);
    };
    let reference = format!("refs/heads/{current_branch}");
    let output = git_output(
        root,
        [
            "for-each-ref",
            "--format=%(upstream:short)",
            reference.as_str(),
        ],
    )
    .await?;
    ensure_success(output.status, &output.stderr)?;
    let upstream = String::from_utf8(output.stdout)?.trim().to_owned();
    if upstream.is_empty()
        || upstream == current_branch
        || upstream.ends_with(&format!("/{current_branch}"))
    {
        return Ok(None);
    }
    Ok(Some(upstream))
}

#[derive(Clone)]
struct Trunk {
    name: String,
    merge_base: String,
}

async fn load_range_points(root: &Checkout, trunk: &Trunk) -> Result<Vec<RangePoint>, VcsError> {
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
    let range = format!("{}..{}", trunk.merge_base, root.head());
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
            return Err(VcsError::InvalidCommitMetadata);
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

async fn commit_metadata(root: &Checkout, revision: &str) -> Result<CommitMetadata, VcsError> {
    let output = git_output(root, ["show", "--no-patch", "--format=%h%x00%s", revision]).await?;
    ensure_success(output.status, &output.stderr)?;
    let value = String::from_utf8(output.stdout)?;
    let Some((short_id, title)) = value.trim().split_once('\0') else {
        return Err(VcsError::InvalidCommitMetadata);
    };
    Ok(CommitMetadata {
        short_id: short_id.to_owned(),
        title: title.to_owned(),
    })
}

async fn revision_exists(root: &Checkout, revision: &str) -> Result<bool, VcsError> {
    let output = git_output(root, ["rev-parse", "--verify", "--quiet", revision]).await?;
    if output.status.success() {
        return Ok(true);
    }
    if output.status.code() == Some(1) {
        return Ok(false);
    }
    ensure_success(output.status, &output.stderr)?;
    Ok(false)
}

async fn merge_base(root: &Checkout, revision: &str) -> Result<String, VcsError> {
    let output = git_output(root, ["merge-base", revision, root.head()]).await?;
    if !output.status.success() {
        return Err(VcsError::InvalidBase(revision.to_owned()));
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

async fn committed_patch(
    root: &Checkout,
    base: &str,
    head: &str,
    full_context: bool,
) -> Result<String, VcsError> {
    let mut arguments = vec![
        "diff",
        "--binary",
        "--find-renames",
        "--find-copies",
        "--no-ext-diff",
        "--src-prefix=a/",
        "--dst-prefix=b/",
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
    root: &Checkout,
    patch: &mut String,
    full_context: bool,
) -> Result<(), VcsError> {
    let output = git_output(root, ["ls-files", "--others", "--exclude-standard", "-z"]).await?;
    ensure_success(output.status, &output.stderr)?;

    for bytes in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let path = std::str::from_utf8(bytes)?;
        if Checkout::is_bookkeeping(path) {
            continue;
        }
        let mut arguments = vec![
            "diff",
            "--binary",
            "--no-ext-diff",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "--no-index",
        ];
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
    root: &Checkout,
    base: &str,
    full_context: bool,
) -> Result<String, VcsError> {
    #[cfg(test)]
    run_patch_hook(root.work_tree(), full_context, PatchHookPhase::Before);

    let mut arguments = vec![
        "diff",
        "--binary",
        "--find-renames",
        "--find-copies",
        "--no-ext-diff",
        "--src-prefix=a/",
        "--dst-prefix=b/",
    ];
    if full_context {
        arguments.push(FULL_CONTEXT);
    }
    arguments.extend([base, "--"]);
    let output = git_output_limited(root, arguments, MAX_DIFF_BYTES, 0).await?;
    ensure_success(output.status, &output.stderr)?;
    let mut patch = String::from_utf8(output.stdout)?;
    append_untracked_files(root, &mut patch, full_context).await?;

    #[cfg(test)]
    run_patch_hook(root.work_tree(), full_context, PatchHookPhase::After);

    Ok(patch)
}

#[cfg(test)]
fn run_patch_hook(root: &Path, full_context: bool, phase: PatchHookPhase) {
    let hook = PATCH_HOOK.read().unwrap().clone();
    if let Some(hook) = hook {
        hook(root, full_context, phase);
    }
}

async fn git_output<const N: usize>(
    root: &Checkout,
    arguments: [&str; N],
) -> Result<Output, VcsError> {
    root.git()
        .args(arguments)
        .output()
        .await
        .map_err(VcsError::StartGit)
}

async fn git_output_limited<I, S>(
    root: &Checkout,
    arguments: I,
    limit: usize,
    used: usize,
) -> Result<Output, VcsError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut child = root
        .git()
        .args(arguments)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(VcsError::StartGit)?;
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
        .map_err(VcsError::ReadGit)?;
    if output.len() > limit {
        let _ = child.kill().await;
        let _ = child.wait().await;
        stderr_task.abort();
        return Err(VcsError::TooLarge {
            actual: used.saturating_add(output.len()),
            maximum: MAX_DIFF_BYTES,
        });
    }
    let status = child.wait().await.map_err(VcsError::WaitGit)?;
    let stderr = stderr_task
        .await
        .map_err(VcsError::GitOutputTask)?
        .map_err(VcsError::ReadGit)?;
    Ok(Output {
        status,
        stdout: output,
        stderr,
    })
}

fn ensure_success(status: std::process::ExitStatus, stderr: &[u8]) -> Result<(), VcsError> {
    if status.success() {
        return Ok(());
    }

    Err(VcsError::GitFailed(
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

#[cfg(test)]
mod tests {
    use super::{
        DiffSnapshot, OverviewContext, OverviewRange, PATCH_HOOK, PATCH_HOOK_SERIAL, PatchHook,
        PatchHookPhase, PatchSide, ReviewContext, ReviewRange, ReviewTargetKind, WorkspaceVersion,
    };
    use crate::{
        Checkout, FilePatch, LineSpan, VcsError,
        testing::{
            added_lines, commit_file, expected_lines, git, repository,
            repository_with_initial_branch,
        },
    };
    use std::{
        collections::BTreeSet,
        fs,
        path::Path,
        sync::{
            Arc, MutexGuard,
            atomic::{AtomicUsize, Ordering},
        },
    };
    use tempfile::TempDir;

    fn paths(patch: &str) -> BTreeSet<String> {
        FilePatch::parse_all(patch)
            .iter()
            .filter_map(|file| file.path().map(str::to_owned))
            .collect()
    }

    fn feature_branch(repository: &TempDir) {
        git(repository.path(), &["checkout", "--quiet", "-b", "feature"]);
        commit_file(repository.path(), "tracked.txt", "feature\n", "feature");
    }

    #[tokio::test]
    async fn uncommitted_scope_includes_tracked_and_untracked_files() {
        let repository = repository();
        fs::write(repository.path().join("tracked.txt"), "changed\n").unwrap();
        fs::write(repository.path().join("new.txt"), "new\n").unwrap();

        let context = ReviewContext::load(repository.path()).await.unwrap();
        let snapshot = context.collect(context.uncommitted_range()).await.unwrap();

        assert_eq!(
            added_lines(&snapshot.patch),
            expected_lines(&[("new.txt", &["new"]), ("tracked.txt", &["changed"])])
        );
    }

    #[tokio::test]
    async fn branch_scope_starts_at_the_merge_base() {
        let repository = repository();
        feature_branch(&repository);

        let context = ReviewContext::load(repository.path()).await.unwrap();
        let snapshot = context.collect(context.full_range()).await.unwrap();

        assert_eq!(
            added_lines(&snapshot.patch),
            expected_lines(&[("tracked.txt", &["feature"])])
        );
        assert_ne!(snapshot.base, "HEAD");
    }

    #[tokio::test]
    async fn review_patch_uses_canonical_prefixes_despite_git_configuration() {
        for setting in ["diff.mnemonicPrefix", "diff.noprefix"] {
            let repository = repository();
            git(repository.path(), &["checkout", "--quiet", "-b", "feature"]);
            commit_file(repository.path(), "committed.txt", "committed\n", "feature");
            fs::write(repository.path().join("tracked.txt"), "working tree\n").unwrap();
            fs::write(repository.path().join("untracked.txt"), "untracked\n").unwrap();
            git(repository.path(), &["config", setting, "true"]);

            let context = ReviewContext::load(repository.path()).await.unwrap();
            let snapshot = context.collect(context.full_range()).await.unwrap();
            let files = FilePatch::parse_all(&snapshot.patch);

            assert!(
                snapshot
                    .patch
                    .lines()
                    .filter(|line| line.starts_with("diff --git "))
                    .all(|header| header.starts_with("diff --git a/") && header.contains(" b/")),
                "{setting}: {}",
                snapshot.patch
            );
            assert_eq!(
                files
                    .iter()
                    .map(|file| (file.old_path.as_deref(), file.new_path.as_deref()))
                    .collect::<Vec<_>>(),
                [
                    (None, Some("committed.txt")),
                    (Some("tracked.txt"), Some("tracked.txt")),
                    (None, Some("untracked.txt")),
                ],
                "{setting}"
            );
        }
    }

    #[tokio::test]
    async fn review_patch_tracks_renames_and_binary_content() {
        let repository = repository();
        git(repository.path(), &["mv", "tracked.txt", "renamed.txt"]);

        let context = ReviewContext::load(repository.path()).await.unwrap();
        let snapshot = context.collect(context.uncommitted_range()).await.unwrap();
        let files = FilePatch::parse_all(&snapshot.patch);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].old_path.as_deref(), Some("tracked.txt"));
        assert_eq!(files[0].new_path.as_deref(), Some("renamed.txt"));
        assert!(files[0].hunks.is_empty());

        fs::write(repository.path().join("renamed.txt"), [0xff, 0x00]).unwrap();
        let snapshot = context.collect(context.uncommitted_range()).await.unwrap();
        let files = FilePatch::parse_all(&snapshot.patch);
        assert!(
            files
                .iter()
                .any(|file| file.new_path.as_deref() == Some("renamed.txt") && file.binary),
            "{files:?}"
        );
    }

    #[tokio::test]
    async fn review_patch_contains_full_git_context_for_expansion() {
        let repository = repository();
        let original = (1..=40)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        commit_file(repository.path(), "tracked.txt", &original, "long file");
        let changed = original.replace("line 20\n", "changed line 20\n");
        fs::write(repository.path().join("tracked.txt"), changed).unwrap();

        let context = ReviewContext::load(repository.path()).await.unwrap();
        let snapshot = context.collect(context.uncommitted_range()).await.unwrap();
        let files = FilePatch::parse_all(&snapshot.patch);

        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0]
                .hunks
                .iter()
                .map(|hunk| (hunk.old, hunk.new))
                .collect::<Vec<_>>(),
            [(
                LineSpan {
                    start: 1,
                    count: 40
                },
                LineSpan {
                    start: 1,
                    count: 40
                }
            )]
        );
    }

    #[tokio::test]
    async fn any_interval_between_trunk_commits_and_working_tree_can_be_selected() {
        let repository = repository();
        git(repository.path(), &["checkout", "--quiet", "-b", "feature"]);
        commit_file(repository.path(), "first.txt", "first\n", "first change");
        commit_file(repository.path(), "second.txt", "second\n", "second change");
        fs::write(repository.path().join("working.txt"), "working\n").unwrap();

        let context = ReviewContext::load(repository.path()).await.unwrap();
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
        assert_eq!(
            committed.overview,
            OverviewContext {
                repository: context.root.work_tree().to_owned(),
                range: OverviewRange::Commits {
                    base: context.range_points[1].revision.clone().unwrap(),
                    head: context.range_points[2].revision.clone().unwrap(),
                },
            }
        );
        assert_eq!(
            paths(&committed.patch),
            BTreeSet::from(["second.txt".to_owned()])
        );

        let through_working_tree = context
            .collect(ReviewRange { from: 2, to: 3 })
            .await
            .unwrap();
        assert_eq!(
            through_working_tree.overview,
            OverviewContext {
                repository: context.root.work_tree().to_owned(),
                range: OverviewRange::WorkingTree {
                    base: context.range_points[2].revision.clone().unwrap(),
                },
            }
        );
        assert_eq!(
            paths(&through_working_tree.patch),
            BTreeSet::from(["working.txt".to_owned()])
        );
    }

    #[tokio::test]
    async fn reversed_or_empty_ranges_are_rejected() {
        let repository = repository();
        let context = ReviewContext::load(repository.path()).await.unwrap();

        for range in [
            ReviewRange { from: 0, to: 0 },
            ReviewRange { from: 1, to: 0 },
        ] {
            assert!(matches!(
                context.collect(range).await,
                Err(VcsError::InvalidRange { .. })
            ));
        }
    }

    #[tokio::test]
    async fn workspace_version_detects_further_edits_to_an_already_modified_file() {
        let repository = repository();
        fs::write(repository.path().join("tracked.txt"), "first edit\n").unwrap();
        let context = ReviewContext::load(repository.path()).await.unwrap();
        let _snapshot = context.collect(context.uncommitted_range()).await.unwrap();
        let initial = context.version();

        fs::write(repository.path().join("tracked.txt"), "second edit\n").unwrap();

        assert_ne!(
            WorkspaceVersion::current(repository.path()).await.unwrap(),
            initial
        );
    }

    #[tokio::test]
    async fn clean_feature_branch_snapshot_matches_the_current_workspace_version() {
        let repository = repository();
        feature_branch(&repository);

        let context = ReviewContext::load(repository.path()).await.unwrap();
        let _snapshot = context.collect(context.full_range()).await.unwrap();

        assert_eq!(
            context.version(),
            WorkspaceVersion::current(repository.path()).await.unwrap()
        );
    }

    #[tokio::test]
    async fn working_tree_snapshot_rejects_changes_between_captures() {
        let repository = repository();
        let path = repository.path().join("tracked.txt");
        fs::write(&path, "state-a\n").unwrap();
        let context = ReviewContext::load(repository.path()).await.unwrap();
        let full_context_calls = Arc::new(AtomicUsize::new(0));
        let _hook = install_patch_hook({
            let root = fs::canonicalize(repository.path()).unwrap();
            let path = path.clone();
            move |candidate, full_context, phase| {
                if candidate != root || !matches!(phase, PatchHookPhase::Before) {
                    return;
                }
                let first_full_context_capture = full_context
                    && full_context_calls
                        .fetch_add(1, Ordering::SeqCst)
                        .is_multiple_of(2);
                let contents = if first_full_context_capture {
                    "state-b-with-a-different-size\n"
                } else {
                    "state-a\n"
                };
                fs::write(&path, contents).unwrap();
            }
        });

        assert!(matches!(
            context.collect(context.uncommitted_range()).await,
            Err(VcsError::WorkspaceChangedDuringSnapshot)
        ));
    }

    #[tokio::test]
    async fn develop_branch_can_define_trunk_without_a_remote_head() {
        let repository = repository_with_initial_branch("develop");
        feature_branch(&repository);

        let context = ReviewContext::load(repository.path()).await.unwrap();
        let snapshot = context.collect(context.full_range()).await.unwrap();

        assert_eq!(context.trunk_name(), "develop");
        assert_eq!(
            added_lines(&snapshot.patch),
            expected_lines(&[("tracked.txt", &["feature"])])
        );
    }

    #[tokio::test]
    async fn remote_default_branch_is_named_without_the_head_alias() {
        let repository = repository();
        point_origin_head_at_main(&repository);
        git(repository.path(), &["checkout", "--quiet", "-b", "feature"]);

        let context = ReviewContext::load(repository.path()).await.unwrap();

        assert_eq!(context.trunk_name(), "origin/main");
    }

    #[tokio::test]
    async fn remote_default_branch_precedes_a_differently_named_feature_upstream() {
        let repository = repository();
        git(repository.path(), &["remote", "add", "origin", "."]);
        point_origin_head_at_main(&repository);
        git(repository.path(), &["checkout", "--quiet", "-b", "topic"]);
        commit_file(repository.path(), "pushed.txt", "pushed\n", "pushed");
        git(
            repository.path(),
            &["update-ref", "refs/remotes/origin/topic", "HEAD"],
        );
        git(
            repository.path(),
            &[
                "checkout",
                "--quiet",
                "--track",
                "-b",
                "pr/1234",
                "origin/topic",
            ],
        );
        commit_file(repository.path(), "local.txt", "local\n", "local");

        let context = ReviewContext::load(repository.path()).await.unwrap();
        let snapshot = context.collect(context.full_range()).await.unwrap();

        assert_eq!(context.trunk_name(), "origin/main");
        assert_eq!(
            added_lines(&snapshot.patch),
            expected_lines(&[("local.txt", &["local"]), ("pushed.txt", &["pushed"])])
        );
    }

    #[tokio::test]
    async fn current_branch_upstream_can_define_an_arbitrary_trunk() {
        let repository = repository_with_initial_branch("stable");
        git(repository.path(), &["checkout", "--quiet", "-b", "feature"]);
        git(repository.path(), &["config", "branch.feature.remote", "."]);
        git(
            repository.path(),
            &["config", "branch.feature.merge", "refs/heads/stable"],
        );

        let context = ReviewContext::load(repository.path()).await.unwrap();

        assert_eq!(context.trunk_name(), "stable");
    }

    #[tokio::test]
    async fn same_branch_remote_upstream_is_not_treated_as_trunk() {
        let repository = repository();
        git(repository.path(), &["checkout", "--quiet", "-b", "feature"]);
        git(repository.path(), &["remote", "add", "origin", "."]);
        git(
            repository.path(),
            &["update-ref", "refs/remotes/origin/feature", "HEAD"],
        );
        git(
            repository.path(),
            &["config", "branch.feature.remote", "origin"],
        );
        git(
            repository.path(),
            &["config", "branch.feature.merge", "refs/heads/feature"],
        );

        let context = ReviewContext::load(repository.path()).await.unwrap();

        assert_eq!(context.trunk_name(), "main");
    }

    #[tokio::test]
    async fn repository_discovery_preserves_non_repository_git_failures() {
        let repository = repository();
        fs::write(repository.path().join(".git/config"), "[invalid\n").unwrap();

        assert!(matches!(
            Checkout::detect(repository.path()).await.map(|_| ()),
            Err(VcsError::GitFailed(_))
        ));
    }

    #[tokio::test]
    async fn repository_discovery_identifies_a_directory_outside_git() {
        let directory = TempDir::new().unwrap();

        assert!(matches!(
            Checkout::detect(directory.path()).await.map(|_| ()),
            Err(VcsError::NotRepository(path)) if path == directory.path()
        ));
    }

    #[test]
    fn comment_anchors_must_fall_inside_a_hunk_on_the_named_side() {
        let snapshot = DiffSnapshot {
            patch: concat!(
                "diff --git \"a/caf\\303\\251.rs\" \"b/caf\\303\\251.rs\"\n",
                "--- \"a/caf\\303\\251.rs\"\n",
                "+++ \"b/caf\\303\\251.rs\"\n",
                "@@ -1 +1,2 @@\n",
                "-old\n",
                "+new\n",
                "+newer\n",
            )
            .to_owned(),
            overview: OverviewContext {
                repository: "/repo".into(),
                range: OverviewRange::WorkingTree {
                    base: "HEAD".to_owned(),
                },
            },
            repository: "repo".to_owned(),
            scope: "Uncommitted changes".to_owned(),
            base: "HEAD".to_owned(),
        };

        assert!(snapshot.contains_anchor("café.rs", PatchSide::Additions, 1, 2));
        assert!(snapshot.contains_anchor("café.rs", PatchSide::Deletions, 1, 1));
        assert!(!snapshot.contains_anchor("café.rs", PatchSide::Deletions, 1, 2));
        assert!(!snapshot.contains_anchor("cafe.rs", PatchSide::Additions, 1, 1));
    }

    fn point_origin_head_at_main(repository: &TempDir) {
        git(
            repository.path(),
            &["update-ref", "refs/remotes/origin/main", "HEAD"],
        );
        git(
            repository.path(),
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        );
    }

    struct PatchHookGuard {
        _serial: MutexGuard<'static, ()>,
    }

    impl Drop for PatchHookGuard {
        fn drop(&mut self) {
            *PATCH_HOOK.write().unwrap() = None;
        }
    }

    fn install_patch_hook(
        hook: impl Fn(&Path, bool, PatchHookPhase) + Send + Sync + 'static,
    ) -> PatchHookGuard {
        let serial = PATCH_HOOK_SERIAL.lock().unwrap();
        *PATCH_HOOK.write().unwrap() = Some(Arc::new(hook) as PatchHook);
        PatchHookGuard { _serial: serial }
    }
}
