//! Native browser workflow for human review of workspace changes.

mod assets;
mod diff;
mod server;

pub(crate) use assets::{AssetAvailability, ReviewAssets};
pub(crate) use diff::ReviewScope;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::future::BoxFuture;
use server::{
    OverviewLoader, ReviewBootstrap, ReviewDecision, ReviewOutcome, ReviewPage, ReviewServer,
    ScopeLoadError, ScopeLoader,
};
use sha2::{Digest, Sha256};
use std::{
    io::Write,
    path::Path,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio_util::sync::CancellationToken;

const MAX_OVERVIEW_BYTES: usize = 1024 * 1024;

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

pub(crate) async fn run(
    overview_generator: OverviewGenerator,
    workspace: &Path,
    assets: ReviewAssets,
) -> Result<Option<String>, ReviewError> {
    let context = diff::load(workspace).await?;
    let title = format!("Review {}", context.repository());
    let preparation_shutdown = CancellationToken::new();
    let _cancel_preparation_on_drop = CancelOnDrop(preparation_shutdown.clone());
    let initial_page = prepare_page(
        context.clone(),
        title.clone(),
        ReviewScope::Uncommitted,
        preparation_shutdown,
    )
    .await?;
    let bootstrap = ReviewBootstrap {
        title: title.clone(),
        repository: context.repository().to_owned(),
        trunk: context.trunk_name().to_owned(),
        default_scope: ReviewScope::Uncommitted,
    };
    let loader_context = context.clone();
    let scope_loader: ScopeLoader = Arc::new(move |scope, shutdown| {
        let context = loader_context.clone();
        let title = title.clone();
        Box::pin(async move {
            prepare_page(context, title, scope, shutdown)
                .await
                .map_err(|error| match error {
                    ReviewError::Cancelled => ScopeLoadError::Cancelled,
                    error => ScopeLoadError::Failed(error.to_string()),
                })
        })
    });
    let overview_loader: OverviewLoader = Arc::new(move |scope, patch, shutdown| {
        let overview_generator = overview_generator.clone();
        Box::pin(async move {
            generate_overview(overview_generator, scope, &patch, shutdown)
                .await
                .map_err(|error| match error {
                    ReviewError::Cancelled => ScopeLoadError::Cancelled,
                    error => ScopeLoadError::Failed(error.to_string()),
                })
        })
    });
    let token = review_token(context.repository(), SystemTime::now());
    let server = ReviewServer::start(
        bootstrap,
        initial_page,
        scope_loader,
        overview_loader,
        token,
        assets.path().to_owned(),
    )
    .await?;
    let url = server.url();
    crate::app::browser::open(&url).map_err(|source| ReviewError::OpenBrowser { url, source })?;
    match server.wait().await? {
        ReviewOutcome::Decision(decision) => Ok(Some(decision.to_markdown())),
        ReviewOutcome::Cancelled => Ok(None),
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
    scope: ReviewScope,
    shutdown: CancellationToken,
) -> Result<ReviewPage, ReviewError> {
    let snapshot = tokio::select! {
        result = context.collect(scope) => result?,
        () = shutdown.cancelled() => return Err(ReviewError::Cancelled),
    };
    Ok(ReviewPage {
        title,
        selected_scope: scope,
        diff: snapshot,
    })
}

async fn generate_overview(
    overview_generator: OverviewGenerator,
    scope: ReviewScope,
    patch: &str,
    shutdown: CancellationToken,
) -> Result<String, ReviewError> {
    let mut snapshot = tempfile::NamedTempFile::new().map_err(ReviewError::OverviewSnapshot)?;
    snapshot
        .write_all(patch.as_bytes())
        .map_err(ReviewError::OverviewSnapshot)?;
    let path = snapshot.path().to_string_lossy();
    let prompt = format!(
        "Prepare a concise HTML overview for a human reviewing `{label}`. The exact, immutable Git patch is at `{path}`. Read it without modifying the workspace. Return only a semantic HTML fragment with the change's purpose, architecture/data flow, most important files, and concrete review risks. The reviewer owns all visual styling so the fragment works in both light and dark mode: do not include styles, classes, images, scripts, external resources, markdown fences, or a full code review.",
        label = scope.label(),
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
        let mut markdown = format!("## Review: {heading}\n");
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
    #[error("failed to open the review page in a browser: {source}. Open {url} manually")]
    OpenBrowser { url: String, source: std::io::Error },
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
}

#[cfg(test)]
mod tests {
    use super::server::{CommentSide, Decision, ReviewComment, ReviewDecision};
    use super::{OverviewGenerator, ReviewScope, generate_overview};
    use std::sync::{Arc, Mutex};
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn overview_generator_reads_the_immutable_patch_and_returns_html() {
        let observed_patch = Arc::new(Mutex::new(String::new()));
        let generator: OverviewGenerator = Arc::new({
            let observed_patch = Arc::clone(&observed_patch);
            move |prompt, _shutdown| {
                let observed_patch = Arc::clone(&observed_patch);
                Box::pin(async move {
                    let path = prompt
                        .split("at `")
                        .nth(1)
                        .and_then(|value| value.split('`').next())
                        .expect("prompt should contain the snapshot path");
                    *observed_patch.lock().unwrap() = std::fs::read_to_string(path).unwrap();
                    Ok("```html\n<section>Overview</section>\n```".to_owned())
                })
            }
        });

        let overview = generate_overview(
            generator,
            ReviewScope::Uncommitted,
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
    }

    #[test]
    fn review_decision_renders_as_composer_markdown() {
        let review = ReviewDecision {
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
            "## Review: Changes requested\n\nPlease address this before merging.\n\n### Comments\n\n- `src/main.rs:12-14` (new)\n  Handle the error.\n  This can fail.\n"
        );
    }
}
