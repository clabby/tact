use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderValue, Response, StatusCode, header},
    response::IntoResponse,
    routing::{get, post},
};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    net::SocketAddr,
    sync::Arc,
};
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
pub(super) const PROTOCOL_VERSION: u32 = 1;
const MAX_CACHED_PAGES: usize = 8;
const MAX_CACHED_PAGE_BYTES: usize = 64 * 1024 * 1024;
const MAX_CACHED_OVERVIEWS: usize = 8;
const MAX_CACHED_OVERVIEW_BYTES: usize = 8 * 1024 * 1024;

type OverviewOperationKey = (u64, super::diff::ReviewRange);
type OverviewOperations = Mutex<HashMap<OverviewOperationKey, Arc<Mutex<()>>>>;

#[derive(Clone, Serialize)]
pub(super) struct ReviewPage {
    pub(super) generation: u64,
    pub(super) snapshot_id: super::diff::SnapshotId,
    pub(super) patch_id: super::diff::PatchId,
    pub(super) title: String,
    pub(super) selected_range: super::diff::ReviewRange,
    #[serde(flatten)]
    pub(super) diff: super::diff::DiffSnapshot,
}

#[derive(Clone, Serialize)]
pub(super) struct ReviewBootstrap {
    pub(super) protocol_version: u32,
    pub(super) generation: u64,
    pub(super) workspace_version: super::diff::WorkspaceVersion,
    pub(super) snapshot_id: super::diff::SnapshotId,
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
    generation: u64,
    range: super::diff::ReviewRange,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotRequest {
    generation: u64,
    snapshot_id: super::diff::SnapshotId,
    patch_id: super::diff::PatchId,
    range: super::diff::ReviewRange,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GenerationRequest {
    generation: u64,
}

#[derive(Serialize)]
struct OverviewResponse {
    generation: u64,
    snapshot_id: super::diff::SnapshotId,
    patch_id: super::diff::PatchId,
    selected_range: super::diff::ReviewRange,
    overview_html: String,
}

#[derive(Serialize)]
struct ReviewStatus {
    generation: u64,
    workspace_version: super::diff::WorkspaceVersion,
    changed: bool,
}

#[derive(Serialize)]
struct RefreshResponse {
    #[serde(flatten)]
    bootstrap: ReviewBootstrap,
    page: ReviewPage,
}

#[derive(Serialize)]
struct ScopeError {
    code: ErrorCode,
    error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    retryable: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    snapshot_valid: Option<bool>,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum ErrorCode {
    StaleSnapshot,
    InvalidRange,
    WorkspaceChanged,
    OverviewFailed,
    OperationCancelled,
    SessionCancelled,
    InvalidCommentAnchor,
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
    pub(super) generation: u64,
    pub(super) snapshot_id: super::diff::SnapshotId,
    pub(super) patch_id: super::diff::PatchId,
    pub(super) range: super::diff::ReviewRange,
    #[serde(skip_deserializing, default)]
    pub(super) scope: String,
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
    assets: super::ReviewAssets,
    session: Mutex<ReviewSession>,
    status_loader: StatusLoader,
    refresh_loader: RefreshLoader,
    overview_loader: OverviewLoader,
    overview_operations: OverviewOperations,
    refresh_generation: Mutex<()>,
    session_shutdown: CancellationToken,
    outcome: Mutex<Option<oneshot::Sender<ReviewOutcome>>>,
}

struct ReviewSession {
    generation: u64,
    bootstrap: ReviewBootstrap,
    default_page: ReviewPage,
    range_loader: RangeLoader,
    range_pages: BoundedCache<super::diff::ReviewRange, ReviewPage>,
    overviews: BoundedCache<super::diff::ReviewRange, String>,
    version: super::diff::WorkspaceVersion,
    snapshot_id: super::diff::SnapshotId,
    generation_shutdown: CancellationToken,
    session_shutdown: CancellationToken,
}

struct BoundedCache<K, V> {
    values: HashMap<K, (V, usize)>,
    order: VecDeque<K>,
    bytes: usize,
    max_entries: usize,
    max_bytes: usize,
}

impl<K, V> BoundedCache<K, V>
where
    K: Clone + Eq + std::hash::Hash,
{
    fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            values: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
            max_entries,
            max_bytes,
        }
    }

    fn get(&self, key: &K) -> Option<&V> {
        self.values.get(key).map(|(value, _)| value)
    }

    fn insert(&mut self, key: K, value: V, bytes: usize) {
        if bytes > self.max_bytes {
            return;
        }
        if let Some((_, old_bytes)) = self.values.remove(&key) {
            self.bytes = self.bytes.saturating_sub(old_bytes);
            self.order.retain(|existing| existing != &key);
        }
        while self.values.len() >= self.max_entries
            || self.bytes.saturating_add(bytes) > self.max_bytes
        {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some((_, old_bytes)) = self.values.remove(&oldest) {
                self.bytes = self.bytes.saturating_sub(old_bytes);
            }
        }
        self.bytes = self.bytes.saturating_add(bytes);
        self.order.push_back(key.clone());
        self.values.insert(key, (value, bytes));
    }
}

impl ReviewSession {
    fn new(review: PreparedReview, session_shutdown: CancellationToken) -> Self {
        let snapshot_id = review.initial_page.snapshot_id.clone();
        Self {
            generation: 0,
            bootstrap: review.bootstrap,
            default_page: review.initial_page,
            range_loader: review.range_loader,
            range_pages: BoundedCache::new(MAX_CACHED_PAGES, MAX_CACHED_PAGE_BYTES),
            overviews: BoundedCache::new(MAX_CACHED_OVERVIEWS, MAX_CACHED_OVERVIEW_BYTES),
            version: review.version,
            snapshot_id,
            generation_shutdown: session_shutdown.child_token(),
            session_shutdown,
        }
    }

    fn replace(&mut self, review: PreparedReview) {
        let generation = self.generation.wrapping_add(1);
        self.generation_shutdown.cancel();
        let session_shutdown = self.session_shutdown.clone();
        *self = Self::new(review, session_shutdown);
        self.generation = generation;
        self.bootstrap.generation = generation;
        self.default_page.generation = generation;
        for (page, _) in self.range_pages.values.values_mut() {
            page.generation = generation;
        }
    }

    fn page(&self, range: &super::diff::ReviewRange) -> Option<&ReviewPage> {
        if range == &self.bootstrap.default_range {
            return Some(&self.default_page);
        }
        self.range_pages.get(range)
    }

    fn insert_page(&mut self, page: ReviewPage) {
        let bytes = page.diff.patch.len()
            + page
                .diff
                .file_contexts
                .iter()
                .map(|context| context.old_contents.len() + context.new_contents.len())
                .sum::<usize>();
        self.range_pages.insert(page.selected_range, page, bytes);
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
    session_shutdown: CancellationToken,
    shutdown: CancellationToken,
    task: tokio::task::JoinHandle<Result<(), std::io::Error>>,
    state: Arc<ServerState>,
}

impl ReviewServer {
    pub(super) async fn start(
        review: PreparedReview,
        status_loader: StatusLoader,
        refresh_loader: RefreshLoader,
        overview_loader: OverviewLoader,
        token: String,
        assets: super::ReviewAssets,
    ) -> Result<Self, std::io::Error> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let address = listener.local_addr()?;
        let (outcome_tx, outcome) = oneshot::channel();
        let session_shutdown = CancellationToken::new();
        let state = Arc::new(ServerState {
            assets,
            session: Mutex::new(ReviewSession::new(review, session_shutdown.clone())),
            status_loader,
            refresh_loader,
            overview_loader,
            overview_operations: Mutex::new(HashMap::new()),
            refresh_generation: Mutex::new(()),
            session_shutdown: session_shutdown.clone(),
            outcome: Mutex::new(Some(outcome_tx)),
        });
        let app = Router::new().nest(&format!("/{token}"), router(Arc::clone(&state)));
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
            session_shutdown,
            shutdown,
            task,
            state,
        })
    }

    pub(super) fn url(&self) -> String {
        format!(
            "http://{}/{}/{}",
            self.address,
            self.token,
            self.state.assets.entrypoint()
        )
    }

    #[cfg(test)]
    fn endpoint_url(&self, endpoint: &str) -> String {
        format!("http://{}/{}/{}", self.address, self.token, endpoint)
    }

    pub(super) async fn wait(mut self) -> Result<ReviewOutcome, ServerError> {
        let outcome = (&mut self.outcome).await?;
        self.session_shutdown.cancel();
        self.shutdown.cancel();
        (&mut self.task).await??;
        Ok(outcome)
    }

    #[cfg(test)]
    pub(super) async fn cancel(&self) {
        self.state.session_shutdown.cancel();
        self.state.session.lock().await.generation_shutdown.cancel();
        if let Some(sender) = self.state.outcome.lock().await.take() {
            let _ = sender.send(ReviewOutcome::Cancelled);
        }
    }
}

impl Drop for ReviewServer {
    fn drop(&mut self) {
        self.session_shutdown.cancel();
        self.shutdown.cancel();
        self.task.abort();
    }
}

fn router(state: Arc<ServerState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/review", get(review))
        .route("/api/status", get(review_status))
        .route("/api/refresh", post(refresh_review))
        .route("/api/range", post(load_range))
        .route("/api/overview", post(load_overview))
        .route("/api/decision", post(submit))
        .route("/api/cancel", post(cancel))
        .route("/{*asset_path}", get(static_asset))
        .layer(DefaultBodyLimit::max(MAX_DECISION_BYTES))
        .with_state(state)
}

async fn index(State(state): State<Arc<ServerState>>) -> impl IntoResponse {
    asset(&state.assets, state.assets.entrypoint()).await
}

async fn static_asset(
    State(state): State<Arc<ServerState>>,
    Path(asset_path): Path<String>,
) -> impl IntoResponse {
    asset(&state.assets, &asset_path).await
}

async fn review(State(state): State<Arc<ServerState>>) -> impl IntoResponse {
    let session = state.session.lock().await;
    let page = session
        .page(&session.bootstrap.default_range)
        .expect("the default review page must remain cached")
        .clone();
    secure_json(
        StatusCode::OK,
        RefreshResponse {
            bootstrap: session.bootstrap.clone(),
            page,
        },
    )
}

async fn review_status(State(state): State<Arc<ServerState>>) -> Response<Body> {
    let (generation, version, shutdown) = {
        let session = state.session.lock().await;
        (
            session.generation,
            session.version.clone(),
            session.generation_shutdown.clone(),
        )
    };
    let current = match (state.status_loader)(shutdown).await {
        Ok(version) => version,
        Err(ScopeLoadError::Cancelled) => {
            return error_response(
                StatusCode::CONFLICT,
                ErrorCode::OperationCancelled,
                "workspace status check was cancelled",
                true,
                true,
            );
        }
        Err(ScopeLoadError::Failed(error)) => {
            return error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorCode::WorkspaceChanged,
                error,
                true,
                false,
            );
        }
    };
    let session = state.session.lock().await;
    if session.generation != generation {
        return stale_snapshot("the review changed while status was loading");
    }
    secure_json(
        StatusCode::OK,
        ReviewStatus {
            generation,
            workspace_version: current.clone(),
            changed: current != version,
        },
    )
}

async fn refresh_review(
    State(state): State<Arc<ServerState>>,
    Json(request): Json<GenerationRequest>,
) -> Response<Body> {
    let _refresh = state.refresh_generation.lock().await;
    let shutdown = {
        let session = state.session.lock().await;
        if request.generation != session.generation {
            return stale_snapshot("the review generation is stale");
        }
        session.generation_shutdown.clone()
    };
    let review = match (state.refresh_loader)(shutdown).await {
        Ok(review) => review,
        Err(ScopeLoadError::Cancelled) => {
            return error_response(
                StatusCode::CONFLICT,
                ErrorCode::OperationCancelled,
                "review refresh was cancelled",
                true,
                true,
            );
        }
        Err(ScopeLoadError::Failed(error)) => {
            return error_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                ErrorCode::WorkspaceChanged,
                error,
                true,
                false,
            );
        }
    };
    let mut session = state.session.lock().await;
    if request.generation != session.generation {
        return stale_snapshot("the review changed while refresh was loading");
    }
    session.replace(review);
    state.overview_operations.lock().await.clear();
    let page = session
        .page(&session.bootstrap.default_range)
        .expect("the refreshed default page must remain cached")
        .clone();
    secure_json(
        StatusCode::OK,
        RefreshResponse {
            bootstrap: session.bootstrap.clone(),
            page,
        },
    )
}

async fn load_range(
    State(state): State<Arc<ServerState>>,
    Json(request): Json<RangeRequest>,
) -> Response<Body> {
    let (loader, generation, version, shutdown) = {
        let session = state.session.lock().await;
        if request.generation != session.generation {
            return stale_snapshot("the review generation is stale");
        }
        if let Some(page) = session.page(&request.range).cloned() {
            return secure_json(StatusCode::OK, page);
        }
        (
            Arc::clone(&session.range_loader),
            session.generation,
            session.version.clone(),
            session.generation_shutdown.clone(),
        )
    };

    let current = match (state.status_loader)(shutdown.clone()).await {
        Ok(version) => version,
        Err(ScopeLoadError::Cancelled) => {
            return error_response(
                StatusCode::CONFLICT,
                ErrorCode::OperationCancelled,
                "range validation was cancelled",
                true,
                true,
            );
        }
        Err(ScopeLoadError::Failed(error)) => {
            return error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorCode::WorkspaceChanged,
                error,
                true,
                false,
            );
        }
    };
    if current != version {
        return stale_snapshot("the workspace changed before this range was loaded");
    }

    let validation_shutdown = shutdown.clone();
    let page = match loader(request.range, shutdown).await {
        Ok(page) => page,
        Err(ScopeLoadError::Cancelled) => {
            return error_response(
                StatusCode::CONFLICT,
                ErrorCode::OperationCancelled,
                "range loading was cancelled",
                true,
                true,
            );
        }
        Err(ScopeLoadError::Failed(error)) => {
            return error_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                ErrorCode::InvalidRange,
                error,
                false,
                true,
            );
        }
    };
    let current = match (state.status_loader)(validation_shutdown).await {
        Ok(version) => version,
        Err(ScopeLoadError::Cancelled) => {
            return stale_snapshot("the review changed while this range was loading");
        }
        Err(ScopeLoadError::Failed(error)) => {
            return error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorCode::WorkspaceChanged,
                error,
                true,
                false,
            );
        }
    };
    if current != version {
        return stale_snapshot("the workspace changed while this range was loading");
    }
    let mut session = state.session.lock().await;
    if session.generation != generation {
        return stale_snapshot("the review changed while this range was loading");
    }
    session.insert_page(page.clone());
    secure_json(StatusCode::OK, page)
}

async fn load_overview(
    State(state): State<Arc<ServerState>>,
    Json(request): Json<SnapshotRequest>,
) -> Response<Body> {
    let overview_key = (request.generation, request.range);
    let operation = {
        let mut operations = state.overview_operations.lock().await;
        Arc::clone(
            operations
                .entry(overview_key)
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    };
    let _operation = operation.lock().await;
    let (page, shutdown) = {
        let session = state.session.lock().await;
        let Some(page) = matching_page(&session, &request) else {
            return stale_snapshot("the requested review snapshot is stale");
        };
        if let Some(overview_html) = session.overviews.get(&request.range).cloned() {
            return secure_json(
                StatusCode::OK,
                OverviewResponse {
                    generation: request.generation,
                    snapshot_id: request.snapshot_id,
                    patch_id: request.patch_id,
                    selected_range: request.range,
                    overview_html,
                },
            );
        }
        (page.clone(), session.generation_shutdown.clone())
    };
    let overview_html = match (state.overview_loader)(
        request.range,
        page.diff.scope.clone(),
        page.diff.patch.clone(),
        shutdown,
    )
    .await
    {
        Ok(overview) => overview,
        Err(ScopeLoadError::Cancelled) => {
            return error_response(
                StatusCode::CONFLICT,
                ErrorCode::OperationCancelled,
                "overview generation was cancelled",
                true,
                true,
            );
        }
        Err(ScopeLoadError::Failed(error)) => {
            return error_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                ErrorCode::OverviewFailed,
                error,
                true,
                true,
            );
        }
    };
    let mut session = state.session.lock().await;
    if matching_page(&session, &request).is_none() {
        return stale_snapshot("the review changed while its overview was loading");
    }
    session
        .overviews
        .insert(request.range, overview_html.clone(), overview_html.len());
    state.overview_operations.lock().await.remove(&overview_key);
    secure_json(
        StatusCode::OK,
        OverviewResponse {
            generation: request.generation,
            snapshot_id: request.snapshot_id,
            patch_id: request.patch_id,
            selected_range: request.range,
            overview_html,
        },
    )
}

async fn submit(
    State(state): State<Arc<ServerState>>,
    Json(mut decision): Json<ReviewDecision>,
) -> impl IntoResponse {
    if invalid_decision(&decision) {
        return error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::InvalidCommentAnchor,
            "invalid review decision",
            false,
            true,
        );
    }

    let session = state.session.lock().await;
    let Some(page) = matching_decision_page(&session, &decision) else {
        return stale_snapshot("the submitted review snapshot is stale");
    };
    if decision
        .comments
        .iter()
        .any(|comment| !valid_comment_anchor(&page.diff, comment))
    {
        return error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::InvalidCommentAnchor,
            "a review comment is not anchored to the reviewed patch",
            false,
            true,
        );
    }
    let current = match (state.status_loader)(session.generation_shutdown.clone()).await {
        Ok(version) => version,
        Err(ScopeLoadError::Cancelled) => {
            return error_response(
                StatusCode::CONFLICT,
                ErrorCode::OperationCancelled,
                "submission validation was cancelled",
                true,
                true,
            );
        }
        Err(ScopeLoadError::Failed(error)) => {
            return error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorCode::WorkspaceChanged,
                error,
                true,
                false,
            );
        }
    };
    if current != session.version {
        return stale_snapshot("the workspace changed after this snapshot was captured");
    }
    decision.scope = page.diff.scope.clone();

    let Some(sender) = state.outcome.lock().await.take() else {
        return stale_snapshot("review already submitted");
    };
    if sender.send(ReviewOutcome::Decision(decision)).is_err() {
        return error_response(
            StatusCode::GONE,
            ErrorCode::SessionCancelled,
            "review is no longer active",
            false,
            false,
        );
    }
    StatusCode::NO_CONTENT.into_response()
}

async fn cancel(
    State(state): State<Arc<ServerState>>,
    Json(request): Json<GenerationRequest>,
) -> impl IntoResponse {
    let session = state.session.lock().await;
    if request.generation != session.generation {
        return stale_snapshot("the review generation is stale");
    }
    session.generation_shutdown.cancel();
    state.session_shutdown.cancel();
    drop(session);
    let Some(sender) = state.outcome.lock().await.take() else {
        return StatusCode::NO_CONTENT.into_response();
    };
    let _ = sender.send(ReviewOutcome::Cancelled);
    StatusCode::NO_CONTENT.into_response()
}

fn matching_page<'a>(
    session: &'a ReviewSession,
    request: &SnapshotRequest,
) -> Option<&'a ReviewPage> {
    if request.generation != session.generation || request.snapshot_id != session.snapshot_id {
        return None;
    }
    let page = session.page(&request.range)?;
    if page.patch_id != request.patch_id {
        return None;
    }
    Some(page)
}

fn matching_decision_page<'a>(
    session: &'a ReviewSession,
    decision: &ReviewDecision,
) -> Option<&'a ReviewPage> {
    if decision.generation != session.generation || decision.snapshot_id != session.snapshot_id {
        return None;
    }
    let page = session.page(&decision.range)?;
    if page.patch_id != decision.patch_id {
        return None;
    }
    Some(page)
}

fn valid_comment_anchor(snapshot: &super::diff::DiffSnapshot, comment: &ReviewComment) -> bool {
    let side = match comment.side {
        CommentSide::Additions => super::diff::PatchSide::Additions,
        CommentSide::Deletions => super::diff::PatchSide::Deletions,
    };
    snapshot.contains_anchor(&comment.path, side, comment.start_line, comment.end_line)
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

fn stale_snapshot(message: impl Into<String>) -> Response<Body> {
    error_response(
        StatusCode::CONFLICT,
        ErrorCode::StaleSnapshot,
        message,
        true,
        false,
    )
}

fn error_response(
    status: StatusCode,
    code: ErrorCode,
    error: impl Into<String>,
    retryable: bool,
    snapshot_valid: bool,
) -> Response<Body> {
    secure_json(
        status,
        ScopeError {
            code,
            error: error.into(),
            retryable: Some(retryable),
            snapshot_valid: Some(snapshot_valid),
        },
    )
}

fn secure_json(status: StatusCode, value: impl Serialize) -> Response<Body> {
    let mut response = (status, Json(value)).into_response();
    secure(&mut response);
    response
}

async fn asset(assets: &super::ReviewAssets, request_path: &str) -> Response<Body> {
    let Some(asset) = assets.resolve(request_path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let contents = match tokio::fs::read(asset.path).await {
        Ok(contents) => contents,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    let mut response = Response::new(Body::from(contents));
    let Ok(content_type) = HeaderValue::from_str(&asset.content_type) else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, content_type);
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
            "default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self'; connect-src 'self'; frame-src 'self'",
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
        Decision, OverviewLoader, PROTOCOL_VERSION, PreparedReview, RangeLoader, RefreshLoader,
        ReviewBootstrap, ReviewOutcome, ReviewPage, ReviewServer, ScopeLoadError, StatusLoader,
    };
    use crate::review::diff::{
        DiffSnapshot, PatchId, ReviewRange, ReviewTarget, ReviewTargetKind, SnapshotId,
        WorkspaceVersion,
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
                "generation": 0,
                "snapshot_id": "snapshot-1",
                "patch_id": "patch-1",
                "range": uncommitted_range(),
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
        assert!(
            response
                .headers()
                .get(reqwest::header::CONTENT_SECURITY_POLICY)
                .unwrap()
                .to_str()
                .unwrap()
                .contains("'wasm-unsafe-eval'"),
            "Shiki requires WebAssembly compilation for syntax highlighting"
        );
        assert_eq!(response.text().await.unwrap(), "review page");

        let missing = reqwest::get(server.endpoint_url("unlisted.js"))
            .await
            .unwrap();
        assert_eq!(missing.status(), reqwest::StatusCode::NOT_FOUND);
        server.cancel().await;
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
            crate::review::ReviewAssets::for_test(assets.path().to_owned()),
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
            .json(&serde_json::json!({ "generation": 0 }))
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
            .json(&range_request(uncommitted_range()))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(loads.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn default_page_remains_available_after_range_cache_eviction() {
        let assets = tempfile::tempdir().unwrap();
        for name in ["index.html", "app.js", "app.css"] {
            std::fs::write(assets.path().join(name), "").unwrap();
        }
        let server = start_server(&assets).await;
        let client = reqwest::Client::new();

        for to in 2..=10 {
            let response = client
                .post(server.endpoint_url("api/range"))
                .json(&range_request(ReviewRange { from: 0, to }))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), reqwest::StatusCode::OK);
        }

        let response = client
            .get(server.endpoint_url("api/review"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        server.cancel().await;
    }

    #[tokio::test]
    async fn range_load_rejects_a_changed_working_tree_snapshot() {
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
        let review = prepared_review(loader());
        let refreshed = review.clone();
        let refresh_loader: RefreshLoader = Arc::new(move |_shutdown| {
            let refreshed = refreshed.clone();
            Box::pin(async move { Ok(refreshed) })
        });
        let server = ReviewServer::start(
            review,
            status_loader,
            refresh_loader,
            overview_loader(),
            "test-token".to_owned(),
            crate::review::ReviewAssets::for_test(assets.path().to_owned()),
        )
        .await
        .unwrap();
        version.store(2, Ordering::SeqCst);

        let response = reqwest::Client::new()
            .post(server.endpoint_url("api/range"))
            .json(&range_request(ReviewRange { from: 0, to: 2 }))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), reqwest::StatusCode::CONFLICT);
        server.cancel().await;
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
            .json(&serde_json::json!({ "generation": 0 }))
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
            .json(&range_request(full_range()))
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
            .json(&serde_json::json!({ "generation": 0 }))
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
                .json(&snapshot_request(uncommitted_range()))
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
    async fn interrupted_overview_does_not_cancel_the_review() {
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
            .json(&snapshot_request(uncommitted_range()))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), reqwest::StatusCode::CONFLICT);
        let body = response.json::<serde_json::Value>().await.unwrap();
        assert_eq!(body["code"], "operation_cancelled");

        reqwest::Client::new()
            .post(server.endpoint_url("api/cancel"))
            .json(&serde_json::json!({ "generation": 0 }))
            .send()
            .await
            .unwrap();
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
                    .json(&range_request(full_range()))
                    .send()
                    .await
            }
        });
        started.notified().await;

        reqwest::Client::new()
            .post(server.endpoint_url("api/cancel"))
            .json(&serde_json::json!({ "generation": 0 }))
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
            .json(&serde_json::json!({ "generation": 0 }))
            .send()
            .await
            .unwrap();

        let outcome = tokio::time::timeout(Duration::from_secs(1), server.wait())
            .await
            .expect("server should stop without waiting for the overview")
            .unwrap();
        assert!(matches!(outcome, ReviewOutcome::Cancelled));
        assert_eq!(
            overview_request.await.unwrap(),
            reqwest::StatusCode::CONFLICT
        );
    }

    #[tokio::test]
    async fn review_bootstrap_matches_the_browser_protocol_shape() {
        let assets = tempfile::tempdir().unwrap();
        for name in ["index.html", "app.js", "app.css"] {
            std::fs::write(assets.path().join(name), "").unwrap();
        }
        let server = start_server(&assets).await;

        let response = reqwest::get(server.endpoint_url("api/review"))
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap();

        assert_eq!(response["protocol_version"], PROTOCOL_VERSION);
        assert_eq!(response["generation"], 0);
        assert_eq!(response["page"]["snapshot_id"], "snapshot-1");
        assert!(response.get("bootstrap").is_none());
        server.cancel().await;
    }

    #[tokio::test]
    async fn stale_submission_is_rejected_without_consuming_the_session() {
        let assets = tempfile::tempdir().unwrap();
        for name in ["index.html", "app.js", "app.css"] {
            std::fs::write(assets.path().join(name), "").unwrap();
        }
        let server = start_server(&assets).await;
        let client = reqwest::Client::new();
        let stale = client
            .post(server.endpoint_url("api/decision"))
            .json(&decision_request("wrong-patch", Vec::new()))
            .send()
            .await
            .unwrap();
        assert_eq!(stale.status(), reqwest::StatusCode::CONFLICT);
        assert_eq!(
            stale.json::<serde_json::Value>().await.unwrap()["code"],
            "stale_snapshot"
        );

        let accepted = client
            .post(server.endpoint_url("api/decision"))
            .json(&decision_request("patch-1", Vec::new()))
            .send()
            .await
            .unwrap();
        assert_eq!(accepted.status(), reqwest::StatusCode::NO_CONTENT);
        assert!(matches!(
            server.wait().await.unwrap(),
            ReviewOutcome::Decision(_)
        ));
    }

    #[tokio::test]
    async fn comment_anchor_must_exist_in_the_reviewed_patch() {
        let assets = tempfile::tempdir().unwrap();
        for name in ["index.html", "app.js", "app.css"] {
            std::fs::write(assets.path().join(name), "").unwrap();
        }
        let server = start_server(&assets).await;
        let comment = serde_json::json!({
            "path": "src/not-reviewed.rs",
            "side": "additions",
            "start_line": 1,
            "end_line": 1,
            "body": "This is not in the patch"
        });
        let response = reqwest::Client::new()
            .post(server.endpoint_url("api/decision"))
            .json(&decision_request("patch-1", vec![comment]))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            response.json::<serde_json::Value>().await.unwrap()["code"],
            "invalid_comment_anchor"
        );
        server.cancel().await;
        assert!(matches!(
            server.wait().await.unwrap(),
            ReviewOutcome::Cancelled
        ));
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
            crate::review::ReviewAssets::for_test(assets.path().to_owned()),
        )
        .await
        .unwrap()
    }

    fn prepared_review(loader: RangeLoader) -> PreparedReview {
        PreparedReview {
            bootstrap: ReviewBootstrap {
                protocol_version: 1,
                generation: 0,
                workspace_version: WorkspaceVersion::test(1),
                snapshot_id: SnapshotId::test(1),
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
            .json(&snapshot_request(range))
            .send()
            .await
            .unwrap()
            .status()
    }

    fn range_request(range: ReviewRange) -> serde_json::Value {
        serde_json::json!({ "generation": 0, "range": range })
    }

    fn snapshot_request(range: ReviewRange) -> serde_json::Value {
        serde_json::json!({
            "generation": 0,
            "snapshot_id": "snapshot-1",
            "patch_id": format!("patch-{}", range.from),
            "range": range,
        })
    }

    fn decision_request(patch_id: &str, comments: Vec<serde_json::Value>) -> serde_json::Value {
        serde_json::json!({
            "generation": 0,
            "snapshot_id": "snapshot-1",
            "patch_id": patch_id,
            "range": uncommitted_range(),
            "decision": "approve",
            "summary": "",
            "comments": comments,
        })
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
            generation: 0,
            snapshot_id: SnapshotId::test(1),
            patch_id: PatchId::test(range.from as u8),
            title: "Review".to_owned(),
            selected_range: range,
            diff: DiffSnapshot {
                patch: "patch".to_owned(),
                file_contexts: Vec::new(),
                repository: "repo".to_owned(),
                scope: "Selected range".to_owned(),
                base: "HEAD".to_owned(),
            },
        }
    }
}
