//! Native browser workflow for human review of workspace changes.

mod assets;
mod diff;
mod server;

pub(crate) use assets::{AssetAvailability, ReviewAssets};
pub(crate) use diff::ReviewRange;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::future::BoxFuture;
use server::{
    OverviewLoader, PreparedReview, RangeLoader, RefreshLoader, ReviewBootstrap, ReviewDecision,
    ReviewOutcome, ReviewPage, ReviewServer, ScopeLoadError, StatusLoader,
};
use sha2::{Digest, Sha256};
use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio_util::sync::CancellationToken;

const MAX_OVERVIEW_BYTES: usize = 1024 * 1024;
const MAX_PREPARATION_ATTEMPTS: usize = 3;

pub(crate) type OverviewGenerator = Arc<
    dyn Fn(String, CancellationToken) -> BoxFuture<'static, Result<String, OverviewGenerationError>>
        + Send
        + Sync,
>;

#[derive(Debug)]
pub(crate) enum OverviewGenerationError {
    Cancelled,
    Failed(String),
}

pub(crate) struct ReviewService;

pub(crate) struct ReviewHandle {
    server: ReviewServer,
}

impl ReviewService {
    pub(crate) async fn start(
        overview_generator: OverviewGenerator,
        workspace: &Path,
        assets: ReviewAssets,
    ) -> Result<ReviewHandle, ReviewError> {
        let preparation_shutdown = CancellationToken::new();
        let _cancel_preparation_on_drop = CancelOnDrop(preparation_shutdown.clone());
        let workspace = workspace.to_path_buf();
        let prepared = prepare_review(workspace.clone(), preparation_shutdown).await?;
        let repository = prepared.bootstrap.repository.clone();
        let status_workspace = workspace.clone();
        let status_loader: StatusLoader = Arc::new(move |shutdown| {
            let workspace = status_workspace.clone();
            Box::pin(async move {
                tokio::select! {
                    result = diff::current_version(&workspace) => result.map_err(|error| ScopeLoadError::Failed(error.to_string())),
                    () = shutdown.cancelled() => Err(ScopeLoadError::Cancelled),
                }
            })
        });
        let refresh_loader: RefreshLoader = Arc::new(move |shutdown| {
            let workspace = workspace.clone();
            Box::pin(async move {
                prepare_review(workspace, shutdown)
                    .await
                    .map_err(scope_load_error)
            })
        });
        let overview_loader: OverviewLoader = Arc::new(move |_range, label, patch, shutdown| {
            let overview_generator = overview_generator.clone();
            Box::pin(async move {
                generate_overview(overview_generator, &label, &patch, shutdown)
                    .await
                    .map_err(|error| match error {
                        ReviewError::Cancelled => ScopeLoadError::Cancelled,
                        error => ScopeLoadError::Failed(error.to_string()),
                    })
            })
        });
        let token = review_token(&repository, SystemTime::now());
        let server = ReviewServer::start(
            prepared,
            status_loader,
            refresh_loader,
            overview_loader,
            token,
            assets,
        )
        .await?;
        Ok(ReviewHandle { server })
    }
}

impl ReviewHandle {
    pub(crate) fn url(&self) -> String {
        self.server.url()
    }

    pub(crate) async fn wait(self) -> Result<Option<String>, ReviewError> {
        match self.server.wait().await? {
            ReviewOutcome::Decision(decision) => Ok(Some(decision.to_markdown())),
            ReviewOutcome::Cancelled => Ok(None),
        }
    }
}

async fn prepare_review(
    workspace: PathBuf,
    shutdown: CancellationToken,
) -> Result<PreparedReview, ReviewError> {
    for _ in 0..MAX_PREPARATION_ATTEMPTS {
        let context = tokio::select! {
            result = diff::load(&workspace) => result?,
            () = shutdown.cancelled() => return Err(ReviewError::Cancelled),
        };
        let title = format!("Review {}", context.repository());
        let default_range = context.default_range();
        let snapshot_id = context.snapshot_id();
        let initial_page = prepare_page(
            context.clone(),
            title.clone(),
            default_range,
            snapshot_id.clone(),
            shutdown.clone(),
        )
        .await?;
        let version = context.version();
        let current = tokio::select! {
            result = diff::current_version(&workspace) => result?,
            () = shutdown.cancelled() => return Err(ReviewError::Cancelled),
        };
        if current != version {
            continue;
        }
        let bootstrap = ReviewBootstrap {
            protocol_version: server::PROTOCOL_VERSION,
            generation: 0,
            workspace_version: version.clone(),
            snapshot_id: snapshot_id.clone(),
            title: title.clone(),
            repository: context.repository().to_owned(),
            trunk: context.trunk_name().to_owned(),
            range_targets: context.range_targets(),
            default_range,
        };
        let range_loader: RangeLoader = Arc::new(move |range, shutdown| {
            let context = context.clone();
            let title = title.clone();
            let snapshot_id = snapshot_id.clone();
            Box::pin(async move {
                prepare_page(context, title, range, snapshot_id, shutdown)
                    .await
                    .map_err(scope_load_error)
            })
        });
        return Ok(PreparedReview {
            bootstrap,
            initial_page,
            range_loader,
            version,
        });
    }
    Err(ReviewError::WorkspaceChanged)
}

fn scope_load_error(error: ReviewError) -> ScopeLoadError {
    match error {
        ReviewError::Cancelled => ScopeLoadError::Cancelled,
        error => ScopeLoadError::Failed(error.to_string()),
    }
}

struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

async fn prepare_page(
    context: diff::ReviewContext,
    title: String,
    range: ReviewRange,
    snapshot_id: diff::SnapshotId,
    shutdown: CancellationToken,
) -> Result<ReviewPage, ReviewError> {
    let snapshot = tokio::select! {
        result = context.collect(range) => result?,
        () = shutdown.cancelled() => return Err(ReviewError::Cancelled),
    };
    let patch_id = diff::patch_id(&snapshot_id, range, &snapshot.patch);
    Ok(ReviewPage {
        generation: 0,
        snapshot_id,
        patch_id,
        title,
        selected_range: range,
        diff: snapshot,
    })
}

async fn generate_overview(
    overview_generator: OverviewGenerator,
    label: &str,
    patch: &str,
    shutdown: CancellationToken,
) -> Result<String, ReviewError> {
    let mut snapshot = tempfile::NamedTempFile::new().map_err(ReviewError::OverviewSnapshot)?;
    snapshot
        .write_all(patch.as_bytes())
        .map_err(ReviewError::OverviewSnapshot)?;
    let path = snapshot.path().to_string_lossy();
    let prompt = format!(
        "Create an HTML overview that explains and visualizes the features in `{label}` for a human reviewer. The exact, immutable Git patch is at `{path}`. Read it without modifying the workspace. Scale the depth of the overview to the size and complexity of the change: a large change warrants a substantial walkthrough, while a small change should remain focused. Explain the purpose, user-visible behavior, architecture and data flow, important files, and areas that deserve reviewer attention. Use clear diagrams when they materially help explain architecture, state, or interactions; inline semantic HTML and inline SVG are available. Return only the HTML fragment, not a full code review. The reviewer owns all visual styling so the fragment works in both light and dark mode: do not include styles, classes, fixed colors, raster images, scripts, external resources, or markdown fences.",
        label = label,
    );
    let result = match overview_generator(prompt, shutdown).await {
        Ok(result) => result,
        Err(OverviewGenerationError::Cancelled) => return Err(ReviewError::Cancelled),
        Err(OverviewGenerationError::Failed(error)) => return Err(ReviewError::Overview(error)),
    };
    let overview = strip_html_fence(result.trim());
    if overview.is_empty() {
        return Err(ReviewError::EmptyOverview);
    }
    if overview.len() > MAX_OVERVIEW_BYTES {
        return Err(ReviewError::OverviewTooLarge);
    }
    Ok(overview.to_owned())
}

fn strip_html_fence(value: &str) -> &str {
    value
        .strip_prefix("```html")
        .or_else(|| value.strip_prefix("```HTML"))
        .and_then(|value| value.strip_suffix("```"))
        .map(str::trim)
        .unwrap_or(value)
}

fn review_token(seed: &str, now: SystemTime) -> String {
    let timestamp = now
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .to_le_bytes();
    let mut digest = Sha256::new();
    digest.update(seed);
    digest.update(timestamp);
    digest.update(std::process::id().to_le_bytes());
    URL_SAFE_NO_PAD.encode(digest.finalize())
}

impl ReviewDecision {
    fn to_markdown(&self) -> String {
        let heading = match self.decision {
            server::Decision::Approve => "Approved",
            server::Decision::RequestChanges => "Changes requested",
        };
        let mut markdown = format!(
            "## Review: {heading}\n\n**Scope:** {scope}\n**Range:** {from} → {to}\n**Snapshot:** `{snapshot}`\n**Patch:** `{patch}`\n",
            scope = self.scope,
            from = self.range.from,
            to = self.range.to,
            snapshot = self.snapshot_id,
            patch = self.patch_id,
        );
        if !self.summary.trim().is_empty() {
            markdown.push('\n');
            markdown.push_str(self.summary.trim());
            markdown.push('\n');
        }
        if self.comments.is_empty() {
            return markdown;
        }

        markdown.push_str("\n### Comments\n");
        for comment in &self.comments {
            let side = match comment.side {
                server::CommentSide::Additions => "new",
                server::CommentSide::Deletions => "old",
            };
            let lines = if comment.start_line == comment.end_line {
                comment.start_line.to_string()
            } else {
                format!("{}-{}", comment.start_line, comment.end_line)
            };
            markdown.push_str(&format!(
                "\n- `{path}:{lines}` ({side})\n  {body}\n",
                path = comment.path,
                body = comment.body.trim().replace('\n', "\n  "),
            ));
        }
        markdown
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ReviewError {
    #[error(transparent)]
    Assets(#[from] assets::AssetError),
    #[error(transparent)]
    Diff(#[from] diff::DiffError),
    #[error("failed to start the review server: {0}")]
    StartServer(#[from] std::io::Error),
    #[error(transparent)]
    Server(#[from] server::ServerError),
    #[error("failed to create the review overview snapshot: {0}")]
    OverviewSnapshot(std::io::Error),
    #[error("failed to generate the review overview: {0}")]
    Overview(String),
    #[error("review overview generation was cancelled")]
    Cancelled,
    #[error("the agent returned an empty review overview")]
    EmptyOverview,
    #[error("the agent returned a review overview larger than 1 MiB")]
    OverviewTooLarge,
    #[error("the workspace kept changing while the review was being prepared; try again")]
    WorkspaceChanged,
}

impl ReviewError {
    pub(crate) fn user_message(&self) -> String {
        if matches!(self, Self::Diff(diff::DiffError::NotRepository(_))) {
            return "The folder must be a git repository.".to_owned();
        }

        format!("Review failed: {self}")
    }
}

#[cfg(test)]
mod tests {
    use super::server::{CommentSide, Decision, ReviewComment, ReviewDecision};
    use super::{OverviewGenerator, generate_overview};
    use crate::review::diff::{PatchId, ReviewRange, SnapshotId};
    use std::sync::{Arc, Mutex};
    use tokio_util::sync::CancellationToken;

    #[test]
    fn non_repository_error_has_a_direct_action_message() {
        let error = super::ReviewError::Diff(super::diff::DiffError::NotRepository(
            "/tmp/not-a-repository".into(),
        ));

        assert_eq!(error.user_message(), "The folder must be a git repository.");
    }

    #[tokio::test]
    async fn overview_generator_reads_the_immutable_patch_and_returns_html() {
        let observed_patch = Arc::new(Mutex::new(String::new()));
        let observed_prompt = Arc::new(Mutex::new(String::new()));
        let generator: OverviewGenerator = Arc::new({
            let observed_patch = Arc::clone(&observed_patch);
            let observed_prompt = Arc::clone(&observed_prompt);
            move |prompt, _shutdown| {
                let observed_patch = Arc::clone(&observed_patch);
                let observed_prompt = Arc::clone(&observed_prompt);
                Box::pin(async move {
                    let path = prompt
                        .split("at `")
                        .nth(1)
                        .and_then(|value| value.split('`').next())
                        .expect("prompt should contain the snapshot path");
                    *observed_patch.lock().unwrap() = std::fs::read_to_string(path).unwrap();
                    *observed_prompt.lock().unwrap() = prompt;
                    Ok("```html\n<section>Overview</section>\n```".to_owned())
                })
            }
        });

        let overview = generate_overview(
            generator,
            "Uncommitted changes",
            "diff --git a/file b/file\n",
            CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(overview, "<section>Overview</section>");
        assert_eq!(
            *observed_patch.lock().unwrap(),
            "diff --git a/file b/file\n"
        );
        let prompt = observed_prompt.lock().unwrap();
        assert!(prompt.contains("explains and visualizes the features"));
        assert!(prompt.contains("Scale the depth"));
        assert!(prompt.contains("inline SVG"));
        assert!(prompt.contains("both light and dark mode"));
    }

    #[test]
    fn review_decision_renders_as_composer_markdown() {
        let review = ReviewDecision {
            generation: 3,
            snapshot_id: SnapshotId::test(1),
            patch_id: PatchId::test(2),
            range: ReviewRange { from: 0, to: 2 },
            scope: "Full branch".to_owned(),
            decision: Decision::RequestChanges,
            summary: "Please address this before merging.".to_owned(),
            comments: vec![ReviewComment {
                path: "src/main.rs".to_owned(),
                side: CommentSide::Additions,
                start_line: 12,
                end_line: 14,
                body: "Handle the error.\nThis can fail.".to_owned(),
            }],
        };

        assert_eq!(
            review.to_markdown(),
            "## Review: Changes requested\n\n**Scope:** Full branch\n**Range:** 0 → 2\n**Snapshot:** `snapshot-1`\n**Patch:** `patch-2`\n\nPlease address this before merging.\n\n### Comments\n\n- `src/main.rs:12-14` (new)\n  Handle the error.\n  This can fail.\n"
        );
    }
}
