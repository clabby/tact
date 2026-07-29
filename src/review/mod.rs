//! Native browser workflow for human review of workspace changes.

mod assets;
mod diff;
mod server;

pub(crate) use assets::{AssetAvailability, ReviewAssets};
pub(crate) use diff::ReviewRange;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use server::{ReviewDecision, ReviewOutcome, ReviewPage, ReviewServer};
use sha2::{Digest, Sha256};
use std::{
    io::Write,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

const MAX_OVERVIEW_BYTES: usize = 1024 * 1024;

pub(crate) async fn load_ranges(workspace: &Path) -> Result<Vec<ReviewRange>, ReviewError> {
    Ok(diff::ranges(workspace).await?)
}

pub(crate) async fn run(
    agent: nanocodex::Nanocodex,
    workspace: &Path,
    range: ReviewRange,
    assets: ReviewAssets,
) -> Result<Option<String>, ReviewError> {
    let snapshot = diff::collect(workspace, &range).await?;
    let overview_html = generate_overview(agent, &range, &snapshot.patch).await?;
    let title = format!("Review {}", snapshot.repository);
    let token = review_token(&snapshot.patch, SystemTime::now());
    let page = ReviewPage {
        title,
        overview_html,
        diff: snapshot,
    };
    let server = ReviewServer::start(page, token, assets.path().to_owned()).await?;
    let url = server.url();
    crate::app::browser::open(&url).map_err(|source| ReviewError::OpenBrowser { url, source })?;
    match server.wait().await? {
        ReviewOutcome::Decision(decision) => Ok(Some(decision.to_markdown())),
        ReviewOutcome::Cancelled => Ok(None),
    }
}

async fn generate_overview(
    agent: nanocodex::Nanocodex,
    range: &ReviewRange,
    patch: &str,
) -> Result<String, ReviewError> {
    let mut snapshot = tempfile::NamedTempFile::new().map_err(ReviewError::OverviewSnapshot)?;
    snapshot
        .write_all(patch.as_bytes())
        .map_err(ReviewError::OverviewSnapshot)?;
    let path = snapshot.path().to_string_lossy();
    let prompt = format!(
        "Prepare a concise HTML overview for a human reviewing `{label}`. The exact, immutable Git patch is at `{path}`. Read it without modifying the workspace. Return only a self-contained HTML fragment with the change's purpose, architecture/data flow, most important files, and concrete review risks. Do not include scripts, external resources, markdown fences, or a full code review.",
        label = range.label(),
    );
    let (child, mut events) = agent.spawn().await?;
    let event_task = tokio::spawn(async move { while events.recv().await.is_some() {} });
    let turn = child.prompt(prompt).await?;
    let result = turn.result().await;
    let shutdown = child.shutdown().await;
    event_task.await.map_err(ReviewError::OverviewEvents)?;
    let result = result?;
    shutdown?;
    let overview = strip_html_fence(result.final_message().trim());
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

fn review_token(patch: &str, now: SystemTime) -> String {
    let timestamp = now
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .to_le_bytes();
    let mut digest = Sha256::new();
    digest.update(patch);
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
    Agent(#[from] nanocodex::NanocodexError),
    #[error("review overview event task failed: {0}")]
    OverviewEvents(tokio::task::JoinError),
    #[error("the agent returned an empty review overview")]
    EmptyOverview,
    #[error("the agent returned a review overview larger than 1 MiB")]
    OverviewTooLarge,
}

#[cfg(test)]
mod tests {
    use super::server::{CommentSide, Decision, ReviewComment, ReviewDecision};

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
