//! Native browser workflow for human review of workspace changes.

mod assets;
mod diff;
mod server;

pub(crate) use assets::{AssetAvailability, ReviewAssets};
pub(crate) use diff::ReviewRange;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::future::BoxFuture;
use server::{
    PreparedReview, ReviewBootstrap, ReviewDecision, ReviewOutcome, ReviewPage, ReviewServer,
    ScopeLoadError,
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

struct ReviewBackend {
    workspace: PathBuf,
    overview_generator: OverviewGenerator,
}

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
        let backend = Arc::new(ReviewBackend {
            workspace: workspace.to_path_buf(),
            overview_generator,
        });
        let prepared = backend.prepare(preparation_shutdown).await?;
        let repository = prepared.bootstrap.repository.clone();
        let token = review_token(&repository, SystemTime::now());
        let server = ReviewServer::start(prepared, backend, token, assets).await?;
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

impl ReviewBackend {
    async fn prepare(&self, shutdown: CancellationToken) -> Result<PreparedReview, ReviewError> {
        for _ in 0..MAX_PREPARATION_ATTEMPTS {
            let context = tokio::select! {
                result = diff::load(&self.workspace) => result?,
                () = shutdown.cancelled() => return Err(ReviewError::Cancelled),
            };
            let default_range = context.default_range();
            let initial_page = self
                .prepare_page(context.clone(), default_range, shutdown.clone())
                .await?;
            let version = context.version();
            if self
                .current_version(shutdown.clone())
                .await
                .map_err(|error| match error {
                    ScopeLoadError::Cancelled => ReviewError::Cancelled,
                    ScopeLoadError::Failed(error) => ReviewError::Overview(error),
                })?
                != version
            {
                continue;
            }
            let bootstrap = ReviewBootstrap {
                protocol_version: server::PROTOCOL_VERSION,
                generation: 0,
                title: format!("Review {}", context.repository()),
                repository: context.repository().to_owned(),
                trunk: context.trunk_name().to_owned(),
                range_targets: context.range_targets(),
                default_range,
            };
            return Ok(PreparedReview {
                bootstrap,
                initial_page,
                context,
                version,
            });
        }
        Err(ReviewError::WorkspaceChanged)
    }

    async fn current_version(
        &self,
        shutdown: CancellationToken,
    ) -> Result<diff::WorkspaceVersion, ScopeLoadError> {
        tokio::select! {
            result = diff::current_version(&self.workspace) => {
                result.map_err(|error| ScopeLoadError::Failed(error.to_string()))
            }
            () = shutdown.cancelled() => Err(ScopeLoadError::Cancelled),
        }
    }

    async fn prepare_page(
        &self,
        context: diff::ReviewContext,
        range: ReviewRange,
        shutdown: CancellationToken,
    ) -> Result<ReviewPage, ReviewError> {
        let diff = tokio::select! {
            result = context.collect(range) => result?,
            () = shutdown.cancelled() => return Err(ReviewError::Cancelled),
        };
        Ok(ReviewPage {
            generation: 0,
            selected_range: range,
            diff,
        })
    }

    async fn overview(
        &self,
        label: &str,
        patch: &str,
        shutdown: CancellationToken,
    ) -> Result<String, ScopeLoadError> {
        generate_overview(self.overview_generator.clone(), label, patch, shutdown)
            .await
            .map_err(scope_load_error)
    }
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
        "Create a self-contained HTML overview of the features in `{label}` for a human reviewer. The exact, immutable Git patch is at `{path}`. Read it without modifying the workspace. Scale the depth and presentation to the change: a small change can be restrained and compact, while a large or architectural change warrants a substantial walkthrough. Explain the purpose, user-visible behavior, architecture and data flow, important files, and areas that deserve reviewer attention. Give the overview a visual identity appropriate to this particular change instead of making it look like rendered Markdown. You may include a `<style>` element, classes, responsive layouts, and inline SVG. Use diagrams or other visualizations when they materially improve understanding, but do not force them into every overview. The iframe document exposes `data-theme=\"light\"`, `data-theme=\"dark\"`, or `data-theme=\"system\"` on its root; define an intentional palette for both light and dark appearances, including a `prefers-color-scheme` fallback for system mode. Return only the HTML fragment, not a full code review or a Markdown fence. Keep it accessible and responsive. Do not include scripts, event handlers, external resources, or raster images.",
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
            "## Review: {heading}\n\n**Scope:** {scope}\n**Range:** {from} → {to}\n",
            scope = self.scope,
            from = self.range.from,
            to = self.range.to,
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
    use crate::review::diff::ReviewRange;
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
        assert!(prompt.contains("self-contained HTML overview"));
        assert!(prompt.contains("Scale the depth and presentation"));
        assert!(prompt.contains("inline SVG"));
        assert!(prompt.contains("visual identity appropriate to this particular change"));
        assert!(prompt.contains("both light and dark appearances"));
        assert!(prompt.contains("do not force them into every overview"));
    }

    #[test]
    fn review_decision_renders_as_composer_markdown() {
        let review = ReviewDecision {
            generation: 3,
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
            "## Review: Changes requested\n\n**Scope:** Full branch\n**Range:** 0 → 2\n\nPlease address this before merging.\n\n### Comments\n\n- `src/main.rs:12-14` (new)\n  Handle the error.\n  This can fail.\n"
        );
    }
}
