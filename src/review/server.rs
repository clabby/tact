use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, State},
    http::{HeaderValue, Response, StatusCode, header},
    response::IntoResponse,
    routing::{get, post},
};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, net::SocketAddr, path::PathBuf, sync::Arc};
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
    pub(super) selected_scope: super::diff::ReviewScope,
    #[serde(flatten)]
    pub(super) diff: super::diff::DiffSnapshot,
}

#[derive(Clone, Serialize)]
pub(super) struct ReviewBootstrap {
    pub(super) title: String,
    pub(super) repository: String,
    pub(super) trunk: String,
    pub(super) default_scope: super::diff::ReviewScope,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScopeRequest {
    scope: super::diff::ReviewScope,
}

#[derive(Serialize)]
struct ScopeError {
    error: String,
}

pub(super) type ScopeLoader = Arc<
    dyn Fn(
            super::diff::ReviewScope,
            CancellationToken,
        ) -> BoxFuture<'static, Result<ReviewPage, ScopeLoadError>>
        + Send
        + Sync,
>;

pub(super) enum ScopeLoadError {
    Cancelled,
    Failed(String),
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
    bootstrap: ReviewBootstrap,
    scope_loader: ScopeLoader,
    scope_pages: Mutex<HashMap<super::diff::ReviewScope, ReviewPage>>,
    scope_shutdown: CancellationToken,
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
    scope_shutdown: CancellationToken,
    shutdown: CancellationToken,
    task: tokio::task::JoinHandle<Result<(), std::io::Error>>,
}

impl ReviewServer {
    pub(super) async fn start(
        bootstrap: ReviewBootstrap,
        scope_loader: ScopeLoader,
        token: String,
        assets: PathBuf,
    ) -> Result<Self, std::io::Error> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let address = listener.local_addr()?;
        let (outcome_tx, outcome) = oneshot::channel();
        let scope_shutdown = CancellationToken::new();
        let state = Arc::new(ServerState {
            assets,
            bootstrap,
            scope_loader,
            scope_pages: Mutex::new(HashMap::new()),
            scope_shutdown: scope_shutdown.clone(),
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
            scope_shutdown,
            shutdown,
            task,
        })
    }

    pub(super) fn url(&self) -> String {
        format!("http://{}/{}/", self.address, self.token)
    }

    pub(super) async fn wait(mut self) -> Result<ReviewOutcome, ServerError> {
        let outcome = (&mut self.outcome).await?;
        self.scope_shutdown.cancel();
        self.shutdown.cancel();
        (&mut self.task).await??;
        Ok(outcome)
    }
}

impl Drop for ReviewServer {
    fn drop(&mut self) {
        self.scope_shutdown.cancel();
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
        .route("/api/scope", post(load_scope))
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
    let mut response = Json(&state.bootstrap).into_response();
    secure(&mut response);
    response
}

async fn load_scope(
    State(state): State<Arc<ServerState>>,
    Json(request): Json<ScopeRequest>,
) -> Response<Body> {
    if let Some(page) = state.scope_pages.lock().await.get(&request.scope).cloned() {
        return secure_json(StatusCode::OK, page);
    }

    let page = match (state.scope_loader)(request.scope, state.scope_shutdown.clone()).await {
        Ok(page) => page,
        Err(ScopeLoadError::Cancelled) => {
            if let Some(sender) = state.outcome.lock().await.take() {
                let _ = sender.send(ReviewOutcome::Cancelled);
            }
            return secure_json(
                StatusCode::GONE,
                ScopeError {
                    error: "review overview generation was cancelled".to_owned(),
                },
            );
        }
        Err(ScopeLoadError::Failed(error)) => {
            return secure_json(StatusCode::UNPROCESSABLE_ENTITY, ScopeError { error });
        }
    };
    state
        .scope_pages
        .lock()
        .await
        .insert(request.scope, page.clone());
    secure_json(StatusCode::OK, page)
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

fn secure_json(status: StatusCode, value: impl Serialize) -> Response<Body> {
    let mut response = (status, Json(value)).into_response();
    secure(&mut response);
    response
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
            "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self'; connect-src 'self'; frame-src 'self'",
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
    use super::{
        Decision, ReviewBootstrap, ReviewOutcome, ReviewPage, ReviewServer, ScopeLoadError,
        ScopeLoader,
    };
    use crate::review::diff::{DiffSnapshot, ReviewScope};
    use std::{sync::Arc, time::Duration};
    use tokio::sync::Notify;

    #[tokio::test]
    async fn server_returns_the_submitted_decision() {
        let assets = tempfile::tempdir().unwrap();
        std::fs::write(assets.path().join("index.html"), "review").unwrap();
        std::fs::write(assets.path().join("app.js"), "").unwrap();
        std::fs::write(assets.path().join("app.css"), "").unwrap();
        let server = start_server(&assets).await;
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
        let server = start_server(&assets).await;
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

    #[tokio::test]
    async fn server_loads_the_selected_scope() {
        let assets = tempfile::tempdir().unwrap();
        for name in ["index.html", "app.js", "app.css"] {
            std::fs::write(assets.path().join(name), "").unwrap();
        }
        let server = start_server(&assets).await;
        let response = reqwest::Client::new()
            .post(format!("{}api/scope", server.url()))
            .json(&serde_json::json!({ "scope": "full_branch" }))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(
            response.json::<serde_json::Value>().await.unwrap()["selected_scope"],
            "full_branch"
        );

        reqwest::Client::new()
            .post(format!("{}api/cancel", server.url()))
            .send()
            .await
            .unwrap();
        assert!(matches!(
            server.wait().await.unwrap(),
            ReviewOutcome::Cancelled
        ));
    }

    #[tokio::test]
    async fn interrupted_overview_cancels_the_review() {
        let assets = tempfile::tempdir().unwrap();
        for name in ["index.html", "app.js", "app.css"] {
            std::fs::write(assets.path().join(name), "").unwrap();
        }
        let loader: ScopeLoader =
            Arc::new(|_scope, _shutdown| Box::pin(async { Err(ScopeLoadError::Cancelled) }));
        let server = start_server_with_loader(&assets, loader).await;

        let response = reqwest::Client::new()
            .post(format!("{}api/scope", server.url()))
            .json(&serde_json::json!({ "scope": "uncommitted" }))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), reqwest::StatusCode::GONE);
        assert!(matches!(
            server.wait().await.unwrap(),
            ReviewOutcome::Cancelled
        ));
    }

    #[tokio::test]
    async fn cancelling_stops_an_active_scope_load() {
        let assets = tempfile::tempdir().unwrap();
        for name in ["index.html", "app.js", "app.css"] {
            std::fs::write(assets.path().join(name), "").unwrap();
        }
        let started = Arc::new(Notify::new());
        let loader: ScopeLoader = Arc::new({
            let started = Arc::clone(&started);
            move |_scope, shutdown| {
                let started = Arc::clone(&started);
                Box::pin(async move {
                    started.notify_one();
                    shutdown.cancelled().await;
                    Err(ScopeLoadError::Cancelled)
                })
            }
        });
        let server = start_server_with_loader(&assets, loader).await;
        let scope_request = tokio::spawn({
            let url = format!("{}api/scope", server.url());
            async move {
                reqwest::Client::new()
                    .post(url)
                    .json(&serde_json::json!({ "scope": "uncommitted" }))
                    .send()
                    .await
            }
        });
        started.notified().await;

        reqwest::Client::new()
            .post(format!("{}api/cancel", server.url()))
            .send()
            .await
            .unwrap();

        let outcome = tokio::time::timeout(Duration::from_secs(1), server.wait())
            .await
            .expect("server should stop without waiting for the scope load")
            .unwrap();
        assert!(matches!(outcome, ReviewOutcome::Cancelled));
        scope_request.await.unwrap().unwrap();
    }

    async fn start_server(assets: &tempfile::TempDir) -> ReviewServer {
        start_server_with_loader(assets, loader()).await
    }

    async fn start_server_with_loader(
        assets: &tempfile::TempDir,
        loader: ScopeLoader,
    ) -> ReviewServer {
        ReviewServer::start(
            ReviewBootstrap {
                title: "Review repo".to_owned(),
                repository: "repo".to_owned(),
                trunk: "main".to_owned(),
                default_scope: ReviewScope::Uncommitted,
            },
            loader,
            "test-token".to_owned(),
            assets.path().to_owned(),
        )
        .await
        .unwrap()
    }

    fn loader() -> ScopeLoader {
        Arc::new(|scope, _shutdown| Box::pin(async move { Ok(page(scope)) }))
    }

    fn page(scope: ReviewScope) -> ReviewPage {
        ReviewPage {
            title: "Review".to_owned(),
            overview_html: "<p>Overview</p>".to_owned(),
            selected_scope: scope,
            diff: DiffSnapshot {
                patch: "patch".to_owned(),
                repository: "repo".to_owned(),
                scope: scope.label().to_owned(),
                base: "HEAD".to_owned(),
            },
        }
    }
}
