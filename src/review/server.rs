use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, State},
    http::{HeaderValue, Response, StatusCode, header},
    response::IntoResponse,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use std::{net::SocketAddr, path::PathBuf, sync::Arc};
use tokio::{
    net::TcpListener,
    sync::{Mutex, oneshot},
};
use tokio_util::sync::CancellationToken;

const MAX_DECISION_BYTES: usize = 1024 * 1024;
const MAX_COMMENTS: usize = 256;
const MAX_COMMENT_BYTES: usize = 64 * 1024;
const MAX_SUMMARY_BYTES: usize = 64 * 1024;
const MAX_PATH_BYTES: usize = 4096;

#[derive(Clone, Serialize)]
pub(super) struct ReviewPage {
    pub(super) title: String,
    pub(super) overview_html: String,
    #[serde(flatten)]
    pub(super) diff: super::diff::DiffSnapshot,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReviewDecision {
    pub(super) decision: Decision,
    #[serde(default)]
    pub(super) summary: String,
    #[serde(default)]
    pub(super) comments: Vec<ReviewComment>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Decision {
    Approve,
    RequestChanges,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReviewComment {
    pub(super) path: String,
    pub(super) side: CommentSide,
    pub(super) start_line: u32,
    pub(super) end_line: u32,
    pub(super) body: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum CommentSide {
    Additions,
    Deletions,
}

struct ServerState {
    assets: PathBuf,
    page: ReviewPage,
    outcome: Mutex<Option<oneshot::Sender<ReviewOutcome>>>,
}

pub(super) enum ReviewOutcome {
    Decision(ReviewDecision),
    Cancelled,
}

pub(super) struct ReviewServer {
    address: SocketAddr,
    token: String,
    outcome: oneshot::Receiver<ReviewOutcome>,
    shutdown: CancellationToken,
    task: tokio::task::JoinHandle<Result<(), std::io::Error>>,
}

impl ReviewServer {
    pub(super) async fn start(
        page: ReviewPage,
        token: String,
        assets: PathBuf,
    ) -> Result<Self, std::io::Error> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let address = listener.local_addr()?;
        let (outcome_tx, outcome) = oneshot::channel();
        let state = Arc::new(ServerState {
            assets,
            page,
            outcome: Mutex::new(Some(outcome_tx)),
        });
        let app = Router::new().nest(&format!("/{token}"), router(state));
        let shutdown = CancellationToken::new();
        let server_shutdown = shutdown.clone();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(server_shutdown.cancelled_owned())
                .await
        });
        Ok(Self {
            address,
            token,
            outcome,
            shutdown,
            task,
        })
    }

    pub(super) fn url(&self) -> String {
        format!("http://{}/{}/", self.address, self.token)
    }

    pub(super) async fn wait(mut self) -> Result<ReviewOutcome, ServerError> {
        let outcome = (&mut self.outcome).await?;
        self.shutdown.cancel();
        (&mut self.task).await??;
        Ok(outcome)
    }
}

impl Drop for ReviewServer {
    fn drop(&mut self) {
        self.shutdown.cancel();
        self.task.abort();
    }
}

fn router(state: Arc<ServerState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/app.js", get(javascript))
        .route("/app.css", get(stylesheet))
        .route("/api/review", get(review))
        .route("/api/decision", post(submit))
        .route("/api/cancel", post(cancel))
        .layer(DefaultBodyLimit::max(MAX_DECISION_BYTES))
        .with_state(state)
}

async fn index(State(state): State<Arc<ServerState>>) -> impl IntoResponse {
    asset(&state.assets, "index.html", "text/html; charset=utf-8").await
}

async fn javascript(State(state): State<Arc<ServerState>>) -> impl IntoResponse {
    asset(&state.assets, "app.js", "text/javascript; charset=utf-8").await
}

async fn stylesheet(State(state): State<Arc<ServerState>>) -> impl IntoResponse {
    asset(&state.assets, "app.css", "text/css; charset=utf-8").await
}

async fn review(State(state): State<Arc<ServerState>>) -> impl IntoResponse {
    let mut response = Json(&state.page).into_response();
    secure(&mut response);
    response
}

async fn submit(
    State(state): State<Arc<ServerState>>,
    Json(decision): Json<ReviewDecision>,
) -> impl IntoResponse {
    if invalid_decision(&decision) {
        return (StatusCode::UNPROCESSABLE_ENTITY, "invalid review decision").into_response();
    }

    let Some(sender) = state.outcome.lock().await.take() else {
        return (StatusCode::CONFLICT, "review already submitted").into_response();
    };
    if sender.send(ReviewOutcome::Decision(decision)).is_err() {
        return (StatusCode::GONE, "review is no longer active").into_response();
    }
    StatusCode::NO_CONTENT.into_response()
}

async fn cancel(State(state): State<Arc<ServerState>>) -> impl IntoResponse {
    let Some(sender) = state.outcome.lock().await.take() else {
        return StatusCode::NO_CONTENT;
    };
    let _ = sender.send(ReviewOutcome::Cancelled);
    StatusCode::NO_CONTENT
}

fn invalid_comment(comment: &ReviewComment) -> bool {
    comment.path.trim().is_empty()
        || comment.path.len() > MAX_PATH_BYTES
        || comment.body.trim().is_empty()
        || comment.body.len() > MAX_COMMENT_BYTES
        || comment.start_line == 0
        || comment.end_line < comment.start_line
}

fn invalid_decision(decision: &ReviewDecision) -> bool {
    decision.summary.len() > MAX_SUMMARY_BYTES
        || decision.comments.len() > MAX_COMMENTS
        || decision.comments.iter().any(invalid_comment)
}

async fn asset(root: &std::path::Path, name: &str, content_type: &'static str) -> Response<Body> {
    let contents = match tokio::fs::read(root.join(name)).await {
        Ok(contents) => contents,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    let mut response = Response::new(Body::from(contents));
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    secure(&mut response);
    response
}

fn secure(response: &mut Response<Body>) {
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; font-src 'self'; connect-src 'self'; frame-src 'self'",
        ),
    );
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ServerError {
    #[error("the review page closed before submitting a decision")]
    Closed(#[from] oneshot::error::RecvError),
    #[error("review server task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
    #[error("review server failed: {0}")]
    Serve(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::{Decision, ReviewOutcome, ReviewPage, ReviewServer};
    use crate::review::diff::DiffSnapshot;

    #[tokio::test]
    async fn server_returns_the_submitted_decision() {
        let assets = tempfile::tempdir().unwrap();
        std::fs::write(assets.path().join("index.html"), "review").unwrap();
        std::fs::write(assets.path().join("app.js"), "").unwrap();
        std::fs::write(assets.path().join("app.css"), "").unwrap();
        let server = ReviewServer::start(page(), "test-token".to_owned(), assets.path().to_owned())
            .await
            .unwrap();
        let client = reqwest::Client::new();
        let response = client
            .post(format!("{}api/decision", server.url()))
            .json(&serde_json::json!({
                "decision": "approve",
                "summary": "Looks good",
                "comments": []
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT);

        let ReviewOutcome::Decision(super::ReviewDecision {
            decision, summary, ..
        }) = server.wait().await.unwrap()
        else {
            panic!("review should be submitted");
        };
        assert!(matches!(decision, Decision::Approve));
        assert_eq!(summary, "Looks good");
    }

    #[tokio::test]
    async fn server_can_cancel_an_abandoned_review() {
        let assets = tempfile::tempdir().unwrap();
        for name in ["index.html", "app.js", "app.css"] {
            std::fs::write(assets.path().join(name), "").unwrap();
        }
        let server = ReviewServer::start(page(), "test-token".to_owned(), assets.path().to_owned())
            .await
            .unwrap();
        let response = reqwest::Client::new()
            .post(format!("{}api/cancel", server.url()))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT);
        assert!(matches!(
            server.wait().await.unwrap(),
            ReviewOutcome::Cancelled
        ));
    }

    fn page() -> ReviewPage {
        ReviewPage {
            title: "Review".to_owned(),
            overview_html: "<p>Overview</p>".to_owned(),
            diff: DiffSnapshot {
                patch: "patch".to_owned(),
                repository: "repo".to_owned(),
                scope: "Branch changes".to_owned(),
                base: "abc123".to_owned(),
            },
        }
    }
}
