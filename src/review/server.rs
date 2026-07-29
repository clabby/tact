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
    pub(super) selected_range: super::diff::ReviewRange,
    #[serde(flatten)]
    pub(super) diff: super::diff::DiffSnapshot,
}

#[derive(Clone, Serialize)]
pub(super) struct ReviewBootstrap {
    pub(super) title: String,
    pub(super) repository: String,
    pub(super) trunk: String,
    pub(super) range_targets: Vec<super::diff::ReviewTarget>,
    pub(super) default_range: super::diff::ReviewRange,
}

#[derive(Clone)]
pub(super) struct PreparedReview {
    pub(super) bootstrap: ReviewBootstrap,
    pub(super) initial_page: ReviewPage,
    pub(super) range_loader: RangeLoader,
    pub(super) version: super::diff::WorkspaceVersion,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RangeRequest {
    range: super::diff::ReviewRange,
}

#[derive(Serialize)]
struct OverviewResponse {
    selected_range: super::diff::ReviewRange,
    overview_html: String,
}

#[derive(Serialize)]
struct ReviewStatus {
    changed: bool,
}

#[derive(Serialize)]
struct RefreshResponse {
    bootstrap: ReviewBootstrap,
    page: ReviewPage,
}

#[derive(Serialize)]
struct ScopeError {
    error: String,
}

pub(super) type RangeLoader = Arc<
    dyn Fn(
            super::diff::ReviewRange,
            CancellationToken,
        ) -> BoxFuture<'static, Result<ReviewPage, ScopeLoadError>>
        + Send
        + Sync,
>;

pub(super) type OverviewLoader = Arc<
    dyn Fn(
            super::diff::ReviewRange,
            String,
            String,
            CancellationToken,
        ) -> BoxFuture<'static, Result<String, ScopeLoadError>>
        + Send
        + Sync,
>;

pub(super) type StatusLoader = Arc<
    dyn Fn(
            CancellationToken,
        ) -> BoxFuture<'static, Result<super::diff::WorkspaceVersion, ScopeLoadError>>
        + Send
        + Sync,
>;

pub(super) type RefreshLoader = Arc<
    dyn Fn(CancellationToken) -> BoxFuture<'static, Result<PreparedReview, ScopeLoadError>>
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
    session: Mutex<ReviewSession>,
    status_loader: StatusLoader,
    refresh_loader: RefreshLoader,
    overview_loader: OverviewLoader,
    overview_generation: Mutex<()>,
    refresh_generation: Mutex<()>,
    scope_shutdown: CancellationToken,
    outcome: Mutex<Option<oneshot::Sender<ReviewOutcome>>>,
}

struct ReviewSession {
    generation: u64,
    bootstrap: ReviewBootstrap,
    range_loader: RangeLoader,
    range_pages: HashMap<super::diff::ReviewRange, ReviewPage>,
    overviews: HashMap<super::diff::ReviewRange, String>,
    version: super::diff::WorkspaceVersion,
}

impl ReviewSession {
    fn new(review: PreparedReview) -> Self {
        Self {
            generation: 0,
            bootstrap: review.bootstrap,
            range_loader: review.range_loader,
            range_pages: HashMap::from([(review.initial_page.selected_range, review.initial_page)]),
            overviews: HashMap::new(),
            version: review.version,
        }
    }

    fn replace(&mut self, review: PreparedReview) {
        let generation = self.generation.wrapping_add(1);
        *self = Self::new(review);
        self.generation = generation;
    }
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
        review: PreparedReview,
        status_loader: StatusLoader,
        refresh_loader: RefreshLoader,
        overview_loader: OverviewLoader,
        token: String,
        assets: PathBuf,
    ) -> Result<Self, std::io::Error> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let address = listener.local_addr()?;
        let (outcome_tx, outcome) = oneshot::channel();
        let scope_shutdown = CancellationToken::new();
        let state = Arc::new(ServerState {
            assets,
            session: Mutex::new(ReviewSession::new(review)),
            status_loader,
            refresh_loader,
            overview_loader,
            overview_generation: Mutex::new(()),
            refresh_generation: Mutex::new(()),
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
        format!("http://{}/{}/index.html", self.address, self.token)
    }

    #[cfg(test)]
    fn endpoint_url(&self, endpoint: &str) -> String {
        format!("http://{}/{}/{}", self.address, self.token, endpoint)
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
        .route("/index.html", get(index))
        .route("/app.js", get(javascript))
        .route("/app.css", get(stylesheet))
        .route("/api/review", get(review))
        .route("/api/status", get(review_status))
        .route("/api/refresh", post(refresh_review))
        .route("/api/range", post(load_range))
        .route("/api/overview", post(load_overview))
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
    let bootstrap = state.session.lock().await.bootstrap.clone();
    let mut response = Json(bootstrap).into_response();
    secure(&mut response);
    response
}

async fn review_status(State(state): State<Arc<ServerState>>) -> Response<Body> {
    let current = match (state.status_loader)(state.scope_shutdown.clone()).await {
        Ok(version) => version,
        Err(ScopeLoadError::Cancelled) => return StatusCode::GONE.into_response(),
        Err(ScopeLoadError::Failed(error)) => {
            return secure_json(StatusCode::SERVICE_UNAVAILABLE, ScopeError { error });
        }
    };
    let changed = current != state.session.lock().await.version;
    secure_json(StatusCode::OK, ReviewStatus { changed })
}

async fn refresh_review(State(state): State<Arc<ServerState>>) -> Response<Body> {
    let _refresh = state.refresh_generation.lock().await;
    let review = match (state.refresh_loader)(state.scope_shutdown.clone()).await {
        Ok(review) => review,
        Err(ScopeLoadError::Cancelled) => return StatusCode::GONE.into_response(),
        Err(ScopeLoadError::Failed(error)) => {
            return secure_json(StatusCode::UNPROCESSABLE_ENTITY, ScopeError { error });
        }
    };
    let response = RefreshResponse {
        bootstrap: review.bootstrap.clone(),
        page: review.initial_page.clone(),
    };
    state.session.lock().await.replace(review);
    secure_json(StatusCode::OK, response)
}

async fn load_range(
    State(state): State<Arc<ServerState>>,
    Json(request): Json<RangeRequest>,
) -> Response<Body> {
    let (loader, generation) = {
        let session = state.session.lock().await;
        if let Some(page) = session.range_pages.get(&request.range).cloned() {
            return secure_json(StatusCode::OK, page);
        }
        (Arc::clone(&session.range_loader), session.generation)
    };

    let page = match loader(request.range, state.scope_shutdown.clone()).await {
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
    let mut session = state.session.lock().await;
    if session.generation != generation {
        return secure_json(
            StatusCode::CONFLICT,
            ScopeError {
                error: "the review changed while this range was loading".to_owned(),
            },
        );
    }
    session.range_pages.insert(request.range, page.clone());
    secure_json(StatusCode::OK, page)
}

async fn load_overview(
    State(state): State<Arc<ServerState>>,
    Json(request): Json<RangeRequest>,
) -> Response<Body> {
    if let Some(overview_html) = state
        .session
        .lock()
        .await
        .overviews
        .get(&request.range)
        .cloned()
    {
        return secure_json(
            StatusCode::OK,
            OverviewResponse {
                selected_range: request.range,
                overview_html,
            },
        );
    }
    let _generation = state.overview_generation.lock().await;
    if let Some(overview_html) = state
        .session
        .lock()
        .await
        .overviews
        .get(&request.range)
        .cloned()
    {
        return secure_json(
            StatusCode::OK,
            OverviewResponse {
                selected_range: request.range,
                overview_html,
            },
        );
    }

    let (label, patch, session_generation) = {
        let session = state.session.lock().await;
        let Some(page) = session.range_pages.get(&request.range) else {
            return secure_json(
                StatusCode::CONFLICT,
                ScopeError {
                    error: "load this change range before requesting its overview".to_owned(),
                },
            );
        };
        (
            page.diff.scope.clone(),
            page.diff.patch.clone(),
            session.generation,
        )
    };
    let overview_html =
        match (state.overview_loader)(request.range, label, patch, state.scope_shutdown.clone())
            .await
        {
            Ok(overview) => overview,
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
    let mut session = state.session.lock().await;
    if session.generation != session_generation {
        return secure_json(
            StatusCode::CONFLICT,
            ScopeError {
                error: "the review changed while its overview was loading".to_owned(),
            },
        );
    }
    session
        .overviews
        .insert(request.range, overview_html.clone());
    secure_json(
        StatusCode::OK,
        OverviewResponse {
            selected_range: request.range,
            overview_html,
        },
    )
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
        Decision, OverviewLoader, PreparedReview, RangeLoader, RefreshLoader, ReviewBootstrap,
        ReviewOutcome, ReviewPage, ReviewServer, ScopeLoadError, StatusLoader,
    };
    use crate::review::diff::{
        DiffSnapshot, ReviewRange, ReviewTarget, ReviewTargetKind, WorkspaceVersion,
    };
    use std::{
        sync::{
            Arc,
            atomic::{AtomicU8, AtomicUsize, Ordering},
        },
        time::Duration,
    };
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
            .post(server.endpoint_url("api/decision"))
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
    async fn generated_review_url_serves_the_index() {
        let assets = tempfile::tempdir().unwrap();
        std::fs::write(assets.path().join("index.html"), "review page").unwrap();
        std::fs::write(assets.path().join("app.js"), "").unwrap();
        std::fs::write(assets.path().join("app.css"), "").unwrap();
        let server = start_server(&assets).await;

        let response = reqwest::get(server.url()).await.unwrap();

        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.text().await.unwrap(), "review page");
    }

    #[tokio::test]
    async fn status_detects_changes_and_refresh_replaces_the_review_snapshot() {
        let assets = tempfile::tempdir().unwrap();
        for name in ["index.html", "app.js", "app.css"] {
            std::fs::write(assets.path().join(name), "").unwrap();
        }
        let version = Arc::new(AtomicU8::new(1));
        let status_loader: StatusLoader = Arc::new({
            let version = Arc::clone(&version);
            move |_shutdown| {
                let value = version.load(Ordering::SeqCst);
                Box::pin(async move { Ok(WorkspaceVersion::test(value)) })
            }
        });
        let refresh_loader: RefreshLoader = Arc::new({
            let version = Arc::clone(&version);
            move |_shutdown| {
                let value = version.load(Ordering::SeqCst);
                let mut review = prepared_review(loader());
                review.version = WorkspaceVersion::test(value);
                review.initial_page.diff.patch = "refreshed patch".to_owned();
                Box::pin(async move { Ok(review) })
            }
        });
        let server = ReviewServer::start(
            prepared_review(loader()),
            status_loader,
            refresh_loader,
            overview_loader(),
            "test-token".to_owned(),
            assets.path().to_owned(),
        )
        .await
        .unwrap();
        let client = reqwest::Client::new();

        let initial = client
            .get(server.endpoint_url("api/status"))
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap();
        assert_eq!(initial["changed"], false);

        version.store(2, Ordering::SeqCst);
        let changed = client
            .get(server.endpoint_url("api/status"))
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap();
        assert_eq!(changed["changed"], true);

        let refreshed = client
            .post(server.endpoint_url("api/refresh"))
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap();
        assert_eq!(refreshed["page"]["patch"], "refreshed patch");
        let current = client
            .get(server.endpoint_url("api/status"))
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap();
        assert_eq!(current["changed"], false);
    }

    #[tokio::test]
    async fn default_scope_is_ready_before_the_browser_requests_it() {
        let assets = tempfile::tempdir().unwrap();
        for name in ["index.html", "app.js", "app.css"] {
            std::fs::write(assets.path().join(name), "").unwrap();
        }
        let loads = Arc::new(AtomicUsize::new(0));
        let loader: RangeLoader = Arc::new({
            let loads = Arc::clone(&loads);
            move |scope, _shutdown| {
                loads.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move { Ok(page(scope)) })
            }
        });
        let server = start_server_with_loader(&assets, loader).await;

        let response = reqwest::Client::new()
            .post(server.endpoint_url("api/range"))
            .json(&serde_json::json!({ "range": uncommitted_range() }))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(loads.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn server_can_cancel_an_abandoned_review() {
        let assets = tempfile::tempdir().unwrap();
        for name in ["index.html", "app.js", "app.css"] {
            std::fs::write(assets.path().join(name), "").unwrap();
        }
        let server = start_server(&assets).await;
        let response = reqwest::Client::new()
            .post(server.endpoint_url("api/cancel"))
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
            .post(server.endpoint_url("api/range"))
            .json(&serde_json::json!({ "range": full_range() }))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(
            response.json::<serde_json::Value>().await.unwrap()["selected_range"],
            serde_json::json!({ "from": 0, "to": 2 })
        );

        reqwest::Client::new()
            .post(server.endpoint_url("api/cancel"))
            .send()
            .await
            .unwrap();
        assert!(matches!(
            server.wait().await.unwrap(),
            ReviewOutcome::Cancelled
        ));
    }

    #[tokio::test]
    async fn overview_is_generated_only_when_requested_and_then_cached() {
        let assets = tempfile::tempdir().unwrap();
        for name in ["index.html", "app.js", "app.css"] {
            std::fs::write(assets.path().join(name), "").unwrap();
        }
        let loads = Arc::new(AtomicUsize::new(0));
        let overview_loader: OverviewLoader = Arc::new({
            let loads = Arc::clone(&loads);
            move |_range, _label, patch, _shutdown| {
                loads.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move { Ok(format!("<p>{patch}</p>")) })
            }
        });
        let server = start_server_with_loaders(&assets, loader(), overview_loader).await;

        assert_eq!(loads.load(Ordering::SeqCst), 0);
        for _ in 0..2 {
            let response = reqwest::Client::new()
                .post(server.endpoint_url("api/overview"))
                .json(&serde_json::json!({ "range": uncommitted_range() }))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), reqwest::StatusCode::OK);
            assert_eq!(
                response.json::<serde_json::Value>().await.unwrap()["overview_html"],
                "<p>patch</p>"
            );
        }
        assert_eq!(loads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn concurrent_overview_requests_share_one_generation() {
        let assets = tempfile::tempdir().unwrap();
        for name in ["index.html", "app.js", "app.css"] {
            std::fs::write(assets.path().join(name), "").unwrap();
        }
        let loads = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let overview_loader: OverviewLoader = Arc::new({
            let loads = Arc::clone(&loads);
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            move |_range, _label, _patch, _shutdown| {
                let started = Arc::clone(&started);
                let release = Arc::clone(&release);
                loads.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move {
                    started.notify_one();
                    release.notified().await;
                    Ok("<p>Overview</p>".to_owned())
                })
            }
        });
        let server = start_server_with_loaders(&assets, loader(), overview_loader).await;
        let url = server.endpoint_url("api/overview");
        let first = tokio::spawn(request_overview(url.clone(), uncommitted_range()));
        started.notified().await;
        let second = tokio::spawn(request_overview(url, uncommitted_range()));
        tokio::task::yield_now().await;

        release.notify_waiters();

        assert_eq!(first.await.unwrap(), reqwest::StatusCode::OK);
        assert_eq!(second.await.unwrap(), reqwest::StatusCode::OK);
        assert_eq!(loads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn overview_requires_the_scope_to_be_loaded() {
        let assets = tempfile::tempdir().unwrap();
        for name in ["index.html", "app.js", "app.css"] {
            std::fs::write(assets.path().join(name), "").unwrap();
        }
        let loads = Arc::new(AtomicUsize::new(0));
        let overview_loader: OverviewLoader = Arc::new({
            let loads = Arc::clone(&loads);
            move |_range, _label, _patch, _shutdown| {
                loads.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok("<p>Overview</p>".to_owned()) })
            }
        });
        let server = start_server_with_loaders(&assets, loader(), overview_loader).await;

        let status = request_overview(server.endpoint_url("api/overview"), full_range()).await;

        assert_eq!(status, reqwest::StatusCode::CONFLICT);
        assert_eq!(loads.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn interrupted_overview_cancels_the_review() {
        let assets = tempfile::tempdir().unwrap();
        for name in ["index.html", "app.js", "app.css"] {
            std::fs::write(assets.path().join(name), "").unwrap();
        }
        let overview_loader: OverviewLoader = Arc::new(|_range, _label, _patch, _shutdown| {
            Box::pin(async { Err(ScopeLoadError::Cancelled) })
        });
        let server = start_server_with_loaders(&assets, loader(), overview_loader).await;

        let response = reqwest::Client::new()
            .post(server.endpoint_url("api/overview"))
            .json(&serde_json::json!({ "range": uncommitted_range() }))
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
        let loader: RangeLoader = Arc::new({
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
            let url = server.endpoint_url("api/range");
            async move {
                reqwest::Client::new()
                    .post(url)
                    .json(&serde_json::json!({ "range": full_range() }))
                    .send()
                    .await
            }
        });
        started.notified().await;

        reqwest::Client::new()
            .post(server.endpoint_url("api/cancel"))
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

    #[tokio::test]
    async fn cancelling_stops_an_active_overview_load() {
        let assets = tempfile::tempdir().unwrap();
        for name in ["index.html", "app.js", "app.css"] {
            std::fs::write(assets.path().join(name), "").unwrap();
        }
        let started = Arc::new(Notify::new());
        let overview_loader: OverviewLoader = Arc::new({
            let started = Arc::clone(&started);
            move |_range, _label, _patch, shutdown| {
                let started = Arc::clone(&started);
                Box::pin(async move {
                    started.notify_one();
                    shutdown.cancelled().await;
                    Err(ScopeLoadError::Cancelled)
                })
            }
        });
        let server = start_server_with_loaders(&assets, loader(), overview_loader).await;
        let overview_request = tokio::spawn({
            let url = server.endpoint_url("api/overview");
            async move { request_overview(url, uncommitted_range()).await }
        });
        started.notified().await;

        reqwest::Client::new()
            .post(server.endpoint_url("api/cancel"))
            .send()
            .await
            .unwrap();

        let outcome = tokio::time::timeout(Duration::from_secs(1), server.wait())
            .await
            .expect("server should stop without waiting for the overview")
            .unwrap();
        assert!(matches!(outcome, ReviewOutcome::Cancelled));
        assert_eq!(overview_request.await.unwrap(), reqwest::StatusCode::GONE);
    }

    async fn start_server(assets: &tempfile::TempDir) -> ReviewServer {
        start_server_with_loader(assets, loader()).await
    }

    async fn start_server_with_loader(
        assets: &tempfile::TempDir,
        loader: RangeLoader,
    ) -> ReviewServer {
        start_server_with_loaders(assets, loader, overview_loader()).await
    }

    async fn start_server_with_loaders(
        assets: &tempfile::TempDir,
        loader: RangeLoader,
        overview_loader: OverviewLoader,
    ) -> ReviewServer {
        let review = prepared_review(loader);
        let status_loader: StatusLoader =
            Arc::new(|_shutdown| Box::pin(async { Ok(WorkspaceVersion::test(1)) }));
        let refreshed = review.clone();
        let refresh_loader: RefreshLoader = Arc::new(move |_shutdown| {
            let refreshed = refreshed.clone();
            Box::pin(async move { Ok(refreshed) })
        });
        ReviewServer::start(
            review,
            status_loader,
            refresh_loader,
            overview_loader,
            "test-token".to_owned(),
            assets.path().to_owned(),
        )
        .await
        .unwrap()
    }

    fn prepared_review(loader: RangeLoader) -> PreparedReview {
        PreparedReview {
            bootstrap: ReviewBootstrap {
                title: "Review repo".to_owned(),
                repository: "repo".to_owned(),
                trunk: "main".to_owned(),
                range_targets: targets(),
                default_range: uncommitted_range(),
            },
            initial_page: page(uncommitted_range()),
            range_loader: loader,
            version: WorkspaceVersion::test(1),
        }
    }

    fn loader() -> RangeLoader {
        Arc::new(|range, _shutdown| Box::pin(async move { Ok(page(range)) }))
    }

    fn overview_loader() -> OverviewLoader {
        Arc::new(|_range, _label, _patch, _shutdown| {
            Box::pin(async { Ok("<p>Overview</p>".to_owned()) })
        })
    }

    async fn request_overview(url: String, range: ReviewRange) -> reqwest::StatusCode {
        reqwest::Client::new()
            .post(url)
            .json(&serde_json::json!({ "range": range }))
            .send()
            .await
            .unwrap()
            .status()
    }

    fn uncommitted_range() -> ReviewRange {
        ReviewRange { from: 1, to: 2 }
    }

    fn full_range() -> ReviewRange {
        ReviewRange { from: 0, to: 2 }
    }

    fn targets() -> Vec<ReviewTarget> {
        vec![
            ReviewTarget {
                index: 0,
                kind: ReviewTargetKind::Trunk,
                short_id: "base".to_owned(),
                title: "main · Base".to_owned(),
            },
            ReviewTarget {
                index: 1,
                kind: ReviewTargetKind::Commit,
                short_id: "head".to_owned(),
                title: "Current commit".to_owned(),
            },
            ReviewTarget {
                index: 2,
                kind: ReviewTargetKind::WorkingTree,
                short_id: "WT".to_owned(),
                title: "Uncommitted changes".to_owned(),
            },
        ]
    }

    fn page(range: ReviewRange) -> ReviewPage {
        ReviewPage {
            title: "Review".to_owned(),
            selected_range: range,
            diff: DiffSnapshot {
                patch: "patch".to_owned(),
                repository: "repo".to_owned(),
                scope: "Selected range".to_owned(),
                base: "HEAD".to_owned(),
            },
        }
    }
}
