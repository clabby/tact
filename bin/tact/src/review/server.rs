use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderValue, Response, StatusCode, header},
    response::IntoResponse,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    net::SocketAddr,
    sync::{Arc, Mutex as StdMutex},
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
const MAX_OPERATION_ID_BYTES: usize = 128;
const MAX_THREAD_MESSAGES: usize = 64;
const MAX_THREAD_BYTES: usize = 256 * 1024;
const MAX_QUESTION_THREADS: usize = 256;
pub(super) const PROTOCOL_VERSION: u32 = 4;
const MAX_CACHED_PAGES: usize = 8;
const MAX_CACHED_PAGE_BYTES: usize = 64 * 1024 * 1024;
const MAX_CACHED_OVERVIEWS: usize = 8;
const MAX_CACHED_OVERVIEW_BYTES: usize = 8 * 1024 * 1024;

type OverviewOperationKey = (u64, super::diff::ReviewRange);
type OverviewOperations = Mutex<HashMap<OverviewOperationKey, Arc<OverviewOperation>>>;

struct OverviewOperation {
    gate: Arc<Mutex<()>>,
    result: Mutex<Option<OverviewRunResult>>,
}

impl OverviewOperation {
    fn new() -> Self {
        Self {
            gate: Arc::new(Mutex::new(())),
            result: Mutex::new(None),
        }
    }
}

#[derive(Clone, Serialize)]
pub(super) struct ReviewPage {
    pub(super) generation: u64,
    pub(super) selected_range: super::diff::ReviewRange,
    pub(super) full_context: bool,
    #[serde(flatten)]
    pub(super) diff: super::diff::DiffSnapshot,
}

#[derive(Clone, Serialize)]
pub(super) struct ReviewBootstrap {
    pub(super) protocol_version: u32,
    pub(super) generation: u64,
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
    pub(super) context: super::diff::ReviewContext,
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
struct GenerationRequest {
    generation: u64,
}

#[derive(Serialize)]
struct OverviewResponse {
    generation: u64,
    selected_range: super::diff::ReviewRange,
    overview_html: String,
}

#[derive(Clone, Serialize)]
struct StoredOverview {
    selected_range: super::diff::ReviewRange,
    status: OverviewStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    overview_html: Option<String>,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum OverviewStatus {
    Generating,
    Ready,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct QuestionRequest {
    pub(super) thread_id: String,
    pub(super) operation_id: String,
    pub(super) generation: u64,
    pub(super) range: super::diff::ReviewRange,
    pub(super) path: String,
    pub(super) side: CommentSide,
    pub(super) start_line: u32,
    pub(super) end_line: u32,
    pub(super) messages: Vec<ThreadMessage>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QuestionCancelRequest {
    operation_id: String,
    generation: u64,
    range: super::diff::ReviewRange,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ThreadMessage {
    pub(super) role: ThreadRole,
    pub(super) body: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ThreadRole {
    Reviewer,
    Agent,
}

#[derive(Serialize)]
struct QuestionResponse {
    generation: u64,
    selected_range: super::diff::ReviewRange,
    answer: String,
}

#[derive(Clone, Serialize)]
struct StoredQuestion {
    thread_id: String,
    operation_id: String,
    generation: u64,
    range: super::diff::ReviewRange,
    path: String,
    side: CommentSide,
    start_line: u32,
    end_line: u32,
    messages: Vec<ThreadMessage>,
    status: QuestionStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Clone, Copy, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum QuestionStatus {
    Asking,
    Idle,
    Error,
    Cancelled,
}

#[derive(Serialize)]
struct QuestionListResponse {
    generation: u64,
    questions: Vec<StoredQuestion>,
}

#[derive(Serialize)]
struct ReviewStatus {
    generation: u64,
    changed: bool,
}

#[derive(Serialize)]
struct RefreshResponse {
    #[serde(flatten)]
    bootstrap: ReviewBootstrap,
    page: ReviewPage,
    overview: Option<StoredOverview>,
    questions: Vec<StoredQuestion>,
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
    QuestionFailed,
    InvalidThread,
    AgentBusy,
    OperationCancelled,
    SessionCancelled,
    InvalidCommentAnchor,
}

pub(super) enum ScopeLoadError {
    Cancelled,
    Failed(String),
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReviewDecision {
    pub(super) generation: u64,
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

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum CommentSide {
    Additions,
    Deletions,
}

struct ServerState {
    assets: super::ReviewAssets,
    session: Mutex<ReviewSession>,
    backend: Arc<super::ReviewBackend>,
    overview_operations: OverviewOperations,
    agent_operation: Arc<Mutex<()>>,
    active_question: StdMutex<Option<ActiveQuestion>>,
    refresh_generation: Mutex<()>,
    session_shutdown: CancellationToken,
    outcome: Mutex<Option<oneshot::Sender<ReviewOutcome>>>,
}

struct ActiveQuestion {
    operation_id: String,
    generation: u64,
    range: super::diff::ReviewRange,
    cancellation: CancellationToken,
}

struct ActiveQuestionRegistration {
    state: Arc<ServerState>,
    operation_id: String,
    cancellation: CancellationToken,
}

impl Drop for ActiveQuestionRegistration {
    fn drop(&mut self) {
        self.cancellation.cancel();
        let Ok(mut active) = self.state.active_question.lock() else {
            return;
        };
        if active
            .as_ref()
            .is_some_and(|question| question.operation_id == self.operation_id)
        {
            *active = None;
        }
    }
}

struct ReviewSession {
    generation: u64,
    bootstrap: ReviewBootstrap,
    default_page: ReviewPage,
    selected_page: ReviewPage,
    context: super::diff::ReviewContext,
    range_pages: BoundedCache<super::diff::ReviewRange, ReviewPage>,
    overviews: BoundedCache<super::diff::ReviewRange, String>,
    active_overview: Option<OverviewOperationKey>,
    questions: Vec<StoredQuestion>,
    version: super::diff::WorkspaceVersion,
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
        let selected_page = review.initial_page.clone();
        Self {
            generation: 0,
            bootstrap: review.bootstrap,
            default_page: review.initial_page,
            selected_page,
            context: review.context,
            range_pages: BoundedCache::new(MAX_CACHED_PAGES, MAX_CACHED_PAGE_BYTES),
            overviews: BoundedCache::new(MAX_CACHED_OVERVIEWS, MAX_CACHED_OVERVIEW_BYTES),
            active_overview: None,
            questions: Vec::new(),
            version: review.version,
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
        self.selected_page.generation = generation;
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
        let bytes = page.diff.patch.len();
        self.range_pages.insert(page.selected_range, page, bytes);
    }

    fn selected_overview(&self) -> Option<StoredOverview> {
        let range = self.selected_page.selected_range;
        if self.active_overview == Some((self.generation, range)) {
            return Some(StoredOverview {
                selected_range: range,
                status: OverviewStatus::Generating,
                overview_html: None,
            });
        }
        self.overviews
            .get(&range)
            .map(|overview_html| StoredOverview {
                selected_range: range,
                status: OverviewStatus::Ready,
                overview_html: Some(overview_html.clone()),
            })
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
        backend: Arc<super::ReviewBackend>,
        token: String,
        assets: super::ReviewAssets,
    ) -> Result<Self, std::io::Error> {
        crate::install_tls_provider();
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let address = listener.local_addr()?;
        let (outcome_tx, outcome) = oneshot::channel();
        let session_shutdown = CancellationToken::new();
        let state = Arc::new(ServerState {
            assets,
            session: Mutex::new(ReviewSession::new(review, session_shutdown.clone())),
            backend,
            overview_operations: Mutex::new(HashMap::new()),
            agent_operation: Arc::new(Mutex::new(())),
            active_question: StdMutex::new(None),
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
        .route("/api/question", post(ask_question))
        .route("/api/questions", post(list_questions))
        .route("/api/question/cancel", post(cancel_question))
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
    secure_json(
        StatusCode::OK,
        RefreshResponse {
            bootstrap: session.bootstrap.clone(),
            page: session.selected_page.clone(),
            overview: session.selected_overview(),
            questions: session.questions.clone(),
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
    let current = match state.backend.current_version(shutdown).await {
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
    let review = match state
        .backend
        .prepare(shutdown)
        .await
        .map_err(super::scope_load_error)
    {
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
            overview: session.selected_overview(),
            questions: session.questions.clone(),
        },
    )
}

async fn load_range(
    State(state): State<Arc<ServerState>>,
    Json(request): Json<RangeRequest>,
) -> Response<Body> {
    let (context, generation, version, shutdown) = {
        let mut session = state.session.lock().await;
        if request.generation != session.generation {
            return stale_snapshot("the review generation is stale");
        }
        if let Some(page) = session.page(&request.range).cloned() {
            session.selected_page = page.clone();
            return secure_json(StatusCode::OK, page);
        }
        (
            session.context.clone(),
            session.generation,
            session.version.clone(),
            session.generation_shutdown.clone(),
        )
    };

    let current = match state.backend.current_version(shutdown.clone()).await {
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
    let page = match state
        .backend
        .prepare_page(context, request.range, shutdown)
        .await
        .map_err(super::scope_load_error)
    {
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
    let current = match state.backend.current_version(validation_shutdown).await {
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
    session.selected_page = page.clone();
    secure_json(StatusCode::OK, page)
}

async fn load_overview(
    State(state): State<Arc<ServerState>>,
    Json(request): Json<RangeRequest>,
) -> Response<Body> {
    let overview_key = (request.generation, request.range);
    let operation = {
        let mut operations = state.overview_operations.lock().await;
        Arc::clone(
            operations
                .entry(overview_key)
                .or_insert_with(|| Arc::new(OverviewOperation::new())),
        )
    };
    let operation_gate = Arc::clone(&operation.gate).lock_owned().await;
    if let Some(result) = operation.result.lock().await.clone() {
        state.overview_operations.lock().await.remove(&overview_key);
        return overview_response(overview_key, result);
    }
    let (page, version, shutdown) = {
        let session = state.session.lock().await;
        let Some(page) = matching_page(&session, request.generation, &request.range) else {
            return stale_snapshot("the requested review snapshot is stale");
        };
        if let Some(overview_html) = session.overviews.get(&request.range).cloned() {
            return secure_json(
                StatusCode::OK,
                OverviewResponse {
                    generation: request.generation,
                    selected_range: request.range,
                    overview_html,
                },
            );
        }
        (
            page.clone(),
            session.version.clone(),
            session.generation_shutdown.clone(),
        )
    };
    let Ok(agent_operation) = Arc::clone(&state.agent_operation).try_lock_owned() else {
        return agent_busy();
    };
    {
        let mut session = state.session.lock().await;
        if matching_page(&session, request.generation, &request.range).is_none() {
            return stale_snapshot("the requested review snapshot is stale");
        }
        session.active_overview = Some(overview_key);
    }

    let (completion, response) = oneshot::channel();
    let task_state = Arc::clone(&state);
    tokio::spawn(async move {
        let _operation_gate = operation_gate;
        let _agent_operation = agent_operation;
        let result = run_overview(&task_state, &page, version, shutdown).await;
        let result = store_overview_result(&task_state, overview_key, result).await;
        *operation.result.lock().await = Some(result.clone());
        let initiating_browser_is_connected = completion
            .send(overview_response(overview_key, result))
            .is_ok();
        let reloaded_browser_is_waiting = Arc::strong_count(&operation) > 2;
        if initiating_browser_is_connected || !reloaded_browser_is_waiting {
            task_state
                .overview_operations
                .lock()
                .await
                .remove(&overview_key);
        }
    });

    response
        .await
        .unwrap_or_else(|_| internal_error("the overview operation stopped unexpectedly"))
}

#[derive(Clone)]
enum OverviewRunResult {
    Ready(String),
    Cancelled,
    Stale(&'static str),
    Workspace(String),
    Failed(String),
}

async fn run_overview(
    state: &ServerState,
    page: &ReviewPage,
    version: super::diff::WorkspaceVersion,
    shutdown: CancellationToken,
) -> OverviewRunResult {
    let current = match state.backend.current_version(shutdown.clone()).await {
        Ok(version) => version,
        Err(ScopeLoadError::Cancelled) => {
            return OverviewRunResult::Stale(
                "the review changed before its overview was generated",
            );
        }
        Err(ScopeLoadError::Failed(error)) => return OverviewRunResult::Workspace(error),
    };
    if current != version {
        return OverviewRunResult::Stale("the workspace changed before its overview was generated");
    }
    let overview_html = match state
        .backend
        .overview(&page.diff.scope, &page.diff.overview, shutdown.clone())
        .await
    {
        Ok(overview) => overview,
        Err(ScopeLoadError::Cancelled) => return OverviewRunResult::Cancelled,
        Err(ScopeLoadError::Failed(error)) => return OverviewRunResult::Failed(error),
    };
    let current = match state.backend.current_version(shutdown).await {
        Ok(version) => version,
        Err(ScopeLoadError::Cancelled) => {
            return OverviewRunResult::Stale(
                "the workspace changed while its overview was generated",
            );
        }
        Err(ScopeLoadError::Failed(error)) => return OverviewRunResult::Workspace(error),
    };
    if current != version {
        return OverviewRunResult::Stale("the workspace changed while its overview was generated");
    }
    OverviewRunResult::Ready(overview_html)
}

async fn store_overview_result(
    state: &ServerState,
    overview_key: OverviewOperationKey,
    result: OverviewRunResult,
) -> OverviewRunResult {
    let mut session = state.session.lock().await;
    if session.active_overview == Some(overview_key) {
        session.active_overview = None;
    }
    if matching_page(&session, overview_key.0, &overview_key.1).is_none() {
        return OverviewRunResult::Stale("the review changed while its overview was loading");
    }
    if let OverviewRunResult::Ready(overview_html) = &result {
        session
            .overviews
            .insert(overview_key.1, overview_html.clone(), overview_html.len());
    }
    result
}

fn overview_response(
    overview_key: OverviewOperationKey,
    result: OverviewRunResult,
) -> Response<Body> {
    match result {
        OverviewRunResult::Ready(overview_html) => secure_json(
            StatusCode::OK,
            OverviewResponse {
                generation: overview_key.0,
                selected_range: overview_key.1,
                overview_html,
            },
        ),
        OverviewRunResult::Cancelled => error_response(
            StatusCode::CONFLICT,
            ErrorCode::OperationCancelled,
            "overview generation was cancelled",
            true,
            true,
        ),
        OverviewRunResult::Stale(message) => stale_snapshot(message),
        OverviewRunResult::Workspace(error) => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::WorkspaceChanged,
            error,
            true,
            false,
        ),
        OverviewRunResult::Failed(error) => error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::OverviewFailed,
            error,
            true,
            true,
        ),
    }
}

async fn ask_question(
    State(state): State<Arc<ServerState>>,
    Json(request): Json<QuestionRequest>,
) -> Response<Body> {
    if invalid_question(&request) {
        return invalid_thread("the question thread is invalid");
    }
    let (page, version, shutdown) = {
        let session = state.session.lock().await;
        let Some(page) = matching_page(&session, request.generation, &request.range) else {
            return stale_snapshot("the question's review snapshot is stale");
        };
        if !valid_anchor(
            &page.diff,
            &request.path,
            request.side,
            request.start_line,
            request.end_line,
        ) {
            return invalid_thread("the question is not anchored to the reviewed patch");
        }
        (
            page.clone(),
            session.version.clone(),
            session.generation_shutdown.clone(),
        )
    };
    let Ok(agent_operation) = Arc::clone(&state.agent_operation).try_lock_owned() else {
        return agent_busy();
    };
    {
        let mut session = state.session.lock().await;
        if !begin_stored_question(&mut session, &request) {
            return invalid_thread("the question does not continue its stored thread");
        }
    }

    let operation_shutdown = shutdown.child_token();
    {
        let Ok(mut active) = state.active_question.lock() else {
            return internal_error("the active question state is unavailable");
        };
        *active = Some(ActiveQuestion {
            operation_id: request.operation_id.clone(),
            generation: request.generation,
            range: request.range,
            cancellation: operation_shutdown.clone(),
        });
    }
    let (completion, response) = oneshot::channel();
    let task_state = Arc::clone(&state);
    tokio::spawn(async move {
        let _agent_operation = agent_operation;
        let completion_shutdown = operation_shutdown.clone();
        let _registration = ActiveQuestionRegistration {
            state: Arc::clone(&task_state),
            operation_id: request.operation_id.clone(),
            cancellation: operation_shutdown.clone(),
        };
        let mut result =
            run_question(&task_state, &request, &page, version, operation_shutdown).await;
        if completion_shutdown.is_cancelled() {
            result = QuestionRunResult::Cancelled;
        }
        store_question_result(&task_state, &request, &result).await;
        let _ = completion.send(question_response(&request, result));
    });

    response
        .await
        .unwrap_or_else(|_| internal_error("the question operation stopped unexpectedly"))
}

async fn list_questions(
    State(state): State<Arc<ServerState>>,
    Json(request): Json<GenerationRequest>,
) -> Response<Body> {
    let session = state.session.lock().await;
    if session.generation != request.generation {
        return stale_snapshot("the question list belongs to an older review generation");
    }
    secure_json(
        StatusCode::OK,
        QuestionListResponse {
            generation: session.generation,
            questions: session.questions.clone(),
        },
    )
}

enum QuestionRunResult {
    Answer(String),
    Cancelled,
    Stale(&'static str),
    Workspace(String),
    Failed(String),
}

async fn run_question(
    state: &ServerState,
    request: &QuestionRequest,
    page: &ReviewPage,
    version: super::diff::WorkspaceVersion,
    shutdown: CancellationToken,
) -> QuestionRunResult {
    let current = match state.backend.current_version(shutdown.clone()).await {
        Ok(version) => version,
        Err(ScopeLoadError::Cancelled) => return QuestionRunResult::Cancelled,
        Err(ScopeLoadError::Failed(error)) => return QuestionRunResult::Workspace(error),
    };
    if current != version {
        return QuestionRunResult::Stale("the workspace changed before the question was answered");
    }
    let answer = match state
        .backend
        .answer_question(
            &page.diff.scope,
            &page.diff.overview,
            request,
            shutdown.clone(),
        )
        .await
    {
        Ok(answer) => answer,
        Err(ScopeLoadError::Cancelled) => return QuestionRunResult::Cancelled,
        Err(ScopeLoadError::Failed(error)) => return QuestionRunResult::Failed(error),
    };
    let current = match state.backend.current_version(shutdown).await {
        Ok(version) => version,
        Err(ScopeLoadError::Cancelled) => return QuestionRunResult::Cancelled,
        Err(ScopeLoadError::Failed(error)) => return QuestionRunResult::Workspace(error),
    };
    if current != version {
        return QuestionRunResult::Stale("the workspace changed while the question was answered");
    }
    let session = state.session.lock().await;
    if matching_page(&session, request.generation, &request.range).is_none() {
        return QuestionRunResult::Stale("the review changed while the question was answered");
    }
    QuestionRunResult::Answer(answer)
}

async fn store_question_result(
    state: &ServerState,
    request: &QuestionRequest,
    result: &QuestionRunResult,
) {
    let mut session = state.session.lock().await;
    let Some(thread) = session.questions.iter_mut().find(|thread| {
        thread.thread_id == request.thread_id
            && thread.operation_id == request.operation_id
            && thread.generation == request.generation
            && thread.range == request.range
    }) else {
        return;
    };
    match result {
        QuestionRunResult::Answer(answer) => {
            thread.messages.push(ThreadMessage {
                role: ThreadRole::Agent,
                body: answer.clone(),
            });
            thread.status = QuestionStatus::Idle;
            thread.error = None;
        }
        QuestionRunResult::Cancelled => {
            thread.status = QuestionStatus::Cancelled;
            thread.error = None;
        }
        QuestionRunResult::Stale(message) => {
            thread.status = QuestionStatus::Error;
            thread.error = Some((*message).to_owned());
        }
        QuestionRunResult::Workspace(error) | QuestionRunResult::Failed(error) => {
            thread.status = QuestionStatus::Error;
            thread.error = Some(error.clone());
        }
    }
}

fn question_response(request: &QuestionRequest, result: QuestionRunResult) -> Response<Body> {
    match result {
        QuestionRunResult::Answer(answer) => secure_json(
            StatusCode::OK,
            QuestionResponse {
                generation: request.generation,
                selected_range: request.range,
                answer,
            },
        ),
        QuestionRunResult::Cancelled => operation_cancelled("question answering was cancelled"),
        QuestionRunResult::Stale(message) => stale_snapshot(message),
        QuestionRunResult::Workspace(error) => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::WorkspaceChanged,
            error,
            true,
            false,
        ),
        QuestionRunResult::Failed(error) => error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::QuestionFailed,
            error,
            true,
            true,
        ),
    }
}

async fn cancel_question(
    State(state): State<Arc<ServerState>>,
    Json(request): Json<QuestionCancelRequest>,
) -> impl IntoResponse {
    if invalid_operation_id(&request.operation_id) {
        return error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::InvalidThread,
            "the question operation identifier is invalid",
            false,
            true,
        );
    }
    let Ok(active) = state.active_question.lock() else {
        return internal_error("the active question state is unavailable");
    };
    if let Some(question) = active.as_ref()
        && question.operation_id == request.operation_id
        && question.generation == request.generation
        && question.range == request.range
    {
        question.cancellation.cancel();
    }
    StatusCode::NO_CONTENT.into_response()
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

    let (page, version, shutdown) = {
        let session = state.session.lock().await;
        let Some(page) = matching_page(&session, decision.generation, &decision.range) else {
            return stale_snapshot("the submitted review snapshot is stale");
        };
        (
            page.clone(),
            session.version.clone(),
            session.generation_shutdown.clone(),
        )
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
    let current = match state.backend.current_version(shutdown).await {
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
    if current != version {
        return stale_snapshot("the workspace changed after this snapshot was captured");
    }
    let session = state.session.lock().await;
    if matching_page(&session, decision.generation, &decision.range).is_none() {
        return stale_snapshot("the review changed while submission was validated");
    }
    decision.scope = page.diff.scope.clone();

    let Some(sender) = state.outcome.lock().await.take() else {
        return stale_snapshot("review already submitted");
    };
    drop(session);

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
    generation: u64,
    range: &super::diff::ReviewRange,
) -> Option<&'a ReviewPage> {
    if generation != session.generation {
        return None;
    }
    session.page(range)
}

fn valid_anchor(
    snapshot: &super::diff::DiffSnapshot,
    path: &str,
    side: CommentSide,
    start_line: u32,
    end_line: u32,
) -> bool {
    let side = match side {
        CommentSide::Additions => super::diff::PatchSide::Additions,
        CommentSide::Deletions => super::diff::PatchSide::Deletions,
    };
    snapshot.contains_anchor(path, side, start_line, end_line)
}

fn valid_comment_anchor(snapshot: &super::diff::DiffSnapshot, comment: &ReviewComment) -> bool {
    valid_anchor(
        snapshot,
        &comment.path,
        comment.side,
        comment.start_line,
        comment.end_line,
    )
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

fn invalid_question(question: &QuestionRequest) -> bool {
    if invalid_operation_id(&question.thread_id)
        || invalid_operation_id(&question.operation_id)
        || question.path.trim().is_empty()
        || question.path.len() > MAX_PATH_BYTES
        || question.start_line == 0
        || question.end_line < question.start_line
        || question.messages.is_empty()
        || question.messages.len() > MAX_THREAD_MESSAGES
    {
        return true;
    }
    let mut bytes = 0_usize;
    for (index, message) in question.messages.iter().enumerate() {
        let expected = if index.is_multiple_of(2) {
            ThreadRole::Reviewer
        } else {
            ThreadRole::Agent
        };
        if message.role != expected || message.body.trim().is_empty() {
            return true;
        }
        bytes = bytes.saturating_add(message.body.len());
    }
    question.messages.last().map(|message| message.role) != Some(ThreadRole::Reviewer)
        || bytes > MAX_THREAD_BYTES
}

fn begin_stored_question(session: &mut ReviewSession, request: &QuestionRequest) -> bool {
    if matching_page(session, request.generation, &request.range).is_none() {
        return false;
    }
    if let Some(thread) = session
        .questions
        .iter_mut()
        .find(|thread| thread.thread_id == request.thread_id)
    {
        let same_anchor = thread.generation == request.generation
            && thread.range == request.range
            && thread.path == request.path
            && thread.side == request.side
            && thread.start_line == request.start_line
            && thread.end_line == request.end_line;
        if !same_anchor || thread.status == QuestionStatus::Asking {
            return false;
        }
        let retries_last_question = matches!(
            thread.status,
            QuestionStatus::Error | QuestionStatus::Cancelled
        ) && request.messages == thread.messages;
        let adds_follow_up = thread.status == QuestionStatus::Idle
            && request.messages.len() == thread.messages.len() + 1
            && request.messages.starts_with(&thread.messages)
            && request.messages.last().map(|message| message.role) == Some(ThreadRole::Reviewer);
        if !retries_last_question && !adds_follow_up {
            return false;
        }
        thread.operation_id.clone_from(&request.operation_id);
        thread.messages.clone_from(&request.messages);
        thread.status = QuestionStatus::Asking;
        thread.error = None;
        return true;
    }
    if session.questions.len() >= MAX_QUESTION_THREADS {
        return false;
    }
    session.questions.push(StoredQuestion {
        thread_id: request.thread_id.clone(),
        operation_id: request.operation_id.clone(),
        generation: request.generation,
        range: request.range,
        path: request.path.clone(),
        side: request.side,
        start_line: request.start_line,
        end_line: request.end_line,
        messages: request.messages.clone(),
        status: QuestionStatus::Asking,
        error: None,
    });
    true
}

fn invalid_operation_id(operation_id: &str) -> bool {
    operation_id.trim().is_empty()
        || operation_id.len() > MAX_OPERATION_ID_BYTES
        || !operation_id.is_ascii()
}

fn agent_busy() -> Response<Body> {
    error_response(
        StatusCode::CONFLICT,
        ErrorCode::AgentBusy,
        "another review agent operation is already running",
        true,
        true,
    )
}

fn invalid_thread(message: impl Into<String>) -> Response<Body> {
    error_response(
        StatusCode::UNPROCESSABLE_ENTITY,
        ErrorCode::InvalidThread,
        message,
        false,
        true,
    )
}

fn operation_cancelled(message: impl Into<String>) -> Response<Body> {
    error_response(
        StatusCode::CONFLICT,
        ErrorCode::OperationCancelled,
        message,
        true,
        true,
    )
}

fn internal_error(message: impl Into<String>) -> Response<Body> {
    error_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        ErrorCode::QuestionFailed,
        message,
        true,
        true,
    )
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
    use super::{Decision, PROTOCOL_VERSION, ReviewOutcome, ReviewServer};
    use crate::review::{ReviewAgent, ReviewAgentError, ReviewBackend, diff::ReviewRange};
    use std::{
        fs,
        path::Path,
        process::Command,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::Duration,
    };
    use tempfile::TempDir;
    use tokio::sync::Notify;
    use tokio_util::sync::CancellationToken;

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
        let server = start_server(&assets).await;
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

        fs::write(
            server.state.backend.workspace.join("working.txt"),
            "changed again\n",
        )
        .unwrap();
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
        assert!(
            refreshed["page"]["patch"]
                .as_str()
                .unwrap()
                .contains("changed again")
        );
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
        let server = start_server(&assets).await;

        let response = reqwest::Client::new()
            .post(server.endpoint_url("api/range"))
            .json(&range_request(uncommitted_range()))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), reqwest::StatusCode::OK);
        server.cancel().await;
    }

    #[tokio::test]
    async fn default_page_remains_available_after_range_cache_eviction() {
        let assets = tempfile::tempdir().unwrap();
        for name in ["index.html", "app.js", "app.css"] {
            std::fs::write(assets.path().join(name), "").unwrap();
        }
        let server = start_server(&assets).await;
        let client = reqwest::Client::new();

        let response = client
            .post(server.endpoint_url("api/range"))
            .json(&range_request(full_range()))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);

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
        let server = start_server(&assets).await;
        fs::write(
            server.state.backend.workspace.join("working.txt"),
            "changed again\n",
        )
        .unwrap();

        let response = reqwest::Client::new()
            .post(server.endpoint_url("api/range"))
            .json(&range_request(working_tree_range()))
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
            .json(&range_request(working_tree_range()))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(
            response.json::<serde_json::Value>().await.unwrap()["selected_range"],
            serde_json::json!({ "from": 1, "to": 2 })
        );
        let reloaded = reqwest::get(server.endpoint_url("api/review"))
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap();
        assert_eq!(
            reloaded["page"]["selected_range"],
            serde_json::json!({ "from": 1, "to": 2 })
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
        let generator: ReviewAgent = Arc::new({
            let loads = Arc::clone(&loads);
            move |_prompt, _shutdown| {
                loads.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move { Ok("<p>Overview</p>".to_owned()) })
            }
        });
        let server = start_server_with_generator(&assets, generator).await;

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
                "<p>Overview</p>"
            );
        }
        assert_eq!(loads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn overview_agent_receives_repository_ranges() {
        let assets = tempfile::tempdir().unwrap();
        for name in ["index.html", "app.js", "app.css"] {
            std::fs::write(assets.path().join(name), "").unwrap();
        }
        let prompts = Arc::new(Mutex::new(Vec::new()));
        let generator: ReviewAgent = Arc::new({
            let prompts = Arc::clone(&prompts);
            move |prompt, _shutdown| {
                prompts.lock().unwrap().push(prompt);
                Box::pin(async move { Ok("<p>Overview</p>".to_owned()) })
            }
        });
        let server = start_server_with_generator(&assets, generator).await;
        let workspace = &server.state.backend.workspace;
        let trunk = git_stdout(workspace, ["rev-parse", "main"]);
        let head = git_stdout(workspace, ["rev-parse", "HEAD"]);
        let client = reqwest::Client::new();

        for range in [ReviewRange { from: 0, to: 1 }, working_tree_range()] {
            let response = client
                .post(server.endpoint_url("api/range"))
                .json(&range_request(range))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), reqwest::StatusCode::OK);
            assert_eq!(
                request_overview(server.endpoint_url("api/overview"), range).await,
                reqwest::StatusCode::OK
            );
        }

        let prompts = prompts.lock().unwrap();
        assert_eq!(prompts.len(), 2);
        assert!(prompts[0].contains(&workspace.to_string_lossy().into_owned()));
        assert!(prompts[0].contains(&format!("{trunk}..{head}")));
        assert!(prompts[0].contains("surrounding"));
        assert!(prompts[1].contains(&head));
        assert!(prompts[1].contains("working tree"));
        assert!(!prompts[0].contains("immutable Git patch"));
        assert!(!prompts[1].contains("immutable Git patch"));
    }

    #[tokio::test]
    async fn question_agent_receives_the_anchor_range_and_complete_thread() {
        let assets = tempfile::tempdir().unwrap();
        for name in ["index.html", "app.js", "app.css"] {
            std::fs::write(assets.path().join(name), "").unwrap();
        }
        let prompt = Arc::new(Mutex::new(String::new()));
        let generator: ReviewAgent = Arc::new({
            let prompt = Arc::clone(&prompt);
            move |value, _shutdown| {
                *prompt.lock().unwrap() = value;
                Box::pin(async move { Ok("The value comes from `tracked.txt:1`.".to_owned()) })
            }
        });
        let server = start_server_with_generator(&assets, generator).await;
        let trunk = git_stdout(&server.state.backend.workspace, ["rev-parse", "main"]);

        let response = reqwest::Client::new()
            .post(server.endpoint_url("api/question"))
            .json(&question_request())
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let response = response.json::<serde_json::Value>().await.unwrap();
        assert_eq!(response["answer"], "The value comes from `tracked.txt:1`.");
        let prompt = prompt.lock().unwrap();
        assert!(prompt.contains("Delegate this task to a sub-agent"));
        assert!(prompt.contains("`tracked.txt:1`"));
        assert!(prompt.contains("new side"));
        assert!(prompt.contains(&trunk));
        assert!(prompt.contains(r#""body":"Why was this changed?""#));
        assert!(prompt.contains(r#""body":"It supports the feature.""#));
        assert!(prompt.contains(r#""body":"Where is that used?""#));
    }

    #[tokio::test]
    async fn question_rejects_an_anchor_outside_the_reviewed_patch() {
        let assets = tempfile::tempdir().unwrap();
        for name in ["index.html", "app.js", "app.css"] {
            std::fs::write(assets.path().join(name), "").unwrap();
        }
        let loads = Arc::new(AtomicUsize::new(0));
        let generator: ReviewAgent = Arc::new({
            let loads = Arc::clone(&loads);
            move |_prompt, _shutdown| {
                loads.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move { Ok("answer".to_owned()) })
            }
        });
        let server = start_server_with_generator(&assets, generator).await;
        let mut request = question_request();
        request["path"] = "not-in-the-review.rs".into();

        let response = reqwest::Client::new()
            .post(server.endpoint_url("api/question"))
            .json(&request)
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(loads.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn overview_and_question_agent_operations_are_mutually_exclusive() {
        let assets = tempfile::tempdir().unwrap();
        for name in ["index.html", "app.js", "app.css"] {
            std::fs::write(assets.path().join(name), "").unwrap();
        }
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let generator: ReviewAgent = Arc::new({
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            move |prompt, _shutdown| {
                let started = Arc::clone(&started);
                let release = Arc::clone(&release);
                Box::pin(async move {
                    started.notify_one();
                    release.notified().await;
                    if prompt.contains("self-contained HTML") {
                        Ok("<p>Overview</p>".to_owned())
                    } else {
                        Ok("Answer".to_owned())
                    }
                })
            }
        });
        let server = start_server_with_generator(&assets, generator).await;
        let client = reqwest::Client::new();
        let overview = tokio::spawn({
            let url = server.endpoint_url("api/overview");
            async move { request_overview(url, uncommitted_range()).await }
        });
        started.notified().await;

        let busy = client
            .post(server.endpoint_url("api/question"))
            .json(&question_request())
            .send()
            .await
            .unwrap();
        assert_eq!(busy.status(), reqwest::StatusCode::CONFLICT);
        assert_eq!(
            busy.json::<serde_json::Value>().await.unwrap()["code"],
            "agent_busy"
        );
        release.notify_one();
        assert_eq!(overview.await.unwrap(), reqwest::StatusCode::OK);

        let range = working_tree_range();
        let loaded = client
            .post(server.endpoint_url("api/range"))
            .json(&range_request(range))
            .send()
            .await
            .unwrap();
        assert_eq!(loaded.status(), reqwest::StatusCode::OK);
        let question = tokio::spawn({
            let url = server.endpoint_url("api/question");
            async move {
                reqwest::Client::new()
                    .post(url)
                    .json(&question_request())
                    .send()
                    .await
                    .unwrap()
                    .status()
            }
        });
        started.notified().await;

        assert_eq!(
            request_overview(server.endpoint_url("api/overview"), range).await,
            reqwest::StatusCode::CONFLICT
        );
        release.notify_one();
        assert_eq!(question.await.unwrap(), reqwest::StatusCode::OK);
    }

    #[tokio::test]
    async fn cancelling_a_question_releases_the_agent_operation() {
        let assets = tempfile::tempdir().unwrap();
        for name in ["index.html", "app.js", "app.css"] {
            std::fs::write(assets.path().join(name), "").unwrap();
        }
        let started = Arc::new(Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let generator: ReviewAgent = Arc::new({
            let started = Arc::clone(&started);
            let calls = Arc::clone(&calls);
            move |_prompt, shutdown| {
                let started = Arc::clone(&started);
                let call = calls.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move {
                    if call == 0 {
                        started.notify_one();
                        shutdown.cancelled().await;
                        return Err(ReviewAgentError::Cancelled);
                    }
                    Ok("<p>Overview</p>".to_owned())
                })
            }
        });
        let server = start_server_with_generator(&assets, generator).await;
        let question = tokio::spawn({
            let url = server.endpoint_url("api/question");
            async move {
                reqwest::Client::new()
                    .post(url)
                    .json(&question_request())
                    .send()
                    .await
                    .unwrap()
            }
        });
        started.notified().await;

        let cancelled = reqwest::Client::new()
            .post(server.endpoint_url("api/question/cancel"))
            .json(&serde_json::json!({
                "operation_id": "question-1",
                "generation": 0,
                "range": uncommitted_range(),
            }))
            .send()
            .await
            .unwrap();

        assert_eq!(cancelled.status(), reqwest::StatusCode::NO_CONTENT);
        let response = question.await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::CONFLICT);
        assert_eq!(
            response.json::<serde_json::Value>().await.unwrap()["code"],
            "operation_cancelled"
        );
        assert_eq!(
            request_overview(server.endpoint_url("api/overview"), uncommitted_range()).await,
            reqwest::StatusCode::OK
        );
    }

    #[tokio::test]
    async fn question_survives_a_browser_reload_and_remains_visible() {
        let assets = tempfile::tempdir().unwrap();
        for name in ["index.html", "app.js", "app.css"] {
            std::fs::write(assets.path().join(name), "").unwrap();
        }
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let cancelled = Arc::new(AtomicBool::new(false));
        let generator: ReviewAgent = Arc::new({
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            let cancelled = Arc::clone(&cancelled);
            move |_prompt, shutdown| {
                let started = Arc::clone(&started);
                let release = Arc::clone(&release);
                let cancelled = Arc::clone(&cancelled);
                Box::pin(async move {
                    started.notify_one();
                    tokio::select! {
                        () = release.notified() => Ok("Persistent answer".to_owned()),
                        () = shutdown.cancelled() => {
                            cancelled.store(true, Ordering::SeqCst);
                            Err(ReviewAgentError::Cancelled)
                        }
                    }
                })
            }
        });
        let server = start_server_with_generator(&assets, generator).await;
        let request = tokio::spawn({
            let url = server.endpoint_url("api/question");
            async move {
                reqwest::Client::new()
                    .post(url)
                    .json(&question_request())
                    .send()
                    .await
            }
        });
        started.notified().await;

        let reloaded = reqwest::get(server.endpoint_url("api/review"))
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap();
        assert_eq!(reloaded["questions"][0]["status"], "asking");
        request.abort();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!cancelled.load(Ordering::SeqCst));
        assert_eq!(
            request_overview(server.endpoint_url("api/overview"), uncommitted_range()).await,
            reqwest::StatusCode::CONFLICT
        );

        release.notify_one();
        let completed = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let review = reqwest::get(server.endpoint_url("api/review"))
                    .await
                    .unwrap()
                    .json::<serde_json::Value>()
                    .await
                    .unwrap();
                if review["questions"][0]["status"] == "idle" {
                    break review;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("the detached question should finish");
        assert_eq!(completed["questions"][0]["status"], "idle");
        assert_eq!(
            completed["questions"][0]["messages"]
                .as_array()
                .unwrap()
                .last()
                .unwrap()["body"],
            "Persistent answer"
        );
    }

    #[tokio::test]
    async fn overview_survives_a_browser_reload_and_remains_visible() {
        let assets = tempfile::tempdir().unwrap();
        for name in ["index.html", "app.js", "app.css"] {
            std::fs::write(assets.path().join(name), "").unwrap();
        }
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let cancelled = Arc::new(AtomicBool::new(false));
        let generator: ReviewAgent = Arc::new({
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            let cancelled = Arc::clone(&cancelled);
            move |_prompt, shutdown| {
                let started = Arc::clone(&started);
                let release = Arc::clone(&release);
                let cancelled = Arc::clone(&cancelled);
                Box::pin(async move {
                    started.notify_one();
                    tokio::select! {
                        () = release.notified() => Ok("<p>Persistent overview</p>".to_owned()),
                        () = shutdown.cancelled() => {
                            cancelled.store(true, Ordering::SeqCst);
                            Err(ReviewAgentError::Cancelled)
                        }
                    }
                })
            }
        });
        let server = start_server_with_generator(&assets, generator).await;
        let client = reqwest::Client::new();
        let range = working_tree_range();
        let loaded = client
            .post(server.endpoint_url("api/range"))
            .json(&range_request(range))
            .send()
            .await
            .unwrap();
        assert_eq!(loaded.status(), reqwest::StatusCode::OK);

        let request = tokio::spawn({
            let url = server.endpoint_url("api/overview");
            async move { request_overview(url, range).await }
        });
        started.notified().await;

        let generating = reqwest::get(server.endpoint_url("api/review"))
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap();
        assert_eq!(
            generating["page"]["selected_range"],
            serde_json::json!({ "from": 1, "to": 2 })
        );
        assert_eq!(generating["overview"]["status"], "generating");
        request.abort();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!cancelled.load(Ordering::SeqCst));

        release.notify_one();
        let completed = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let review = reqwest::get(server.endpoint_url("api/review"))
                    .await
                    .unwrap()
                    .json::<serde_json::Value>()
                    .await
                    .unwrap();
                if review["overview"]["status"] == "ready" {
                    break review;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("the detached overview should finish");
        assert_eq!(
            completed["overview"]["overview_html"],
            "<p>Persistent overview</p>"
        );
    }

    #[tokio::test]
    async fn reloaded_browser_observes_the_same_overview_failure() {
        let assets = tempfile::tempdir().unwrap();
        for name in ["index.html", "app.js", "app.css"] {
            std::fs::write(assets.path().join(name), "").unwrap();
        }
        let loads = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let generator: ReviewAgent = Arc::new({
            let loads = Arc::clone(&loads);
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            move |_prompt, _shutdown| {
                let loads = Arc::clone(&loads);
                let started = Arc::clone(&started);
                let release = Arc::clone(&release);
                Box::pin(async move {
                    loads.fetch_add(1, Ordering::SeqCst);
                    started.notify_one();
                    release.notified().await;
                    Err(ReviewAgentError::Failed("overview failed".to_owned()))
                })
            }
        });
        let server = start_server_with_generator(&assets, generator).await;
        let first = tokio::spawn({
            let url = server.endpoint_url("api/overview");
            async move { request_overview(url, uncommitted_range()).await }
        });
        started.notified().await;
        let reloaded = tokio::spawn({
            let url = server.endpoint_url("api/overview");
            async move { request_overview(url, uncommitted_range()).await }
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let operations = server.state.overview_operations.lock().await;
                let reloaded_browser_is_waiting = operations
                    .get(&(0, uncommitted_range()))
                    .is_some_and(|operation| Arc::strong_count(operation) > 2);
                drop(operations);

                if reloaded_browser_is_waiting {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the reloaded browser should join the in-flight operation");
        first.abort();
        release.notify_one();

        let reloaded_status = tokio::time::timeout(Duration::from_secs(2), reloaded)
            .await
            .expect("the reloaded browser should observe the completed operation")
            .unwrap();
        assert_eq!(reloaded_status, reqwest::StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(loads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn overview_rejects_a_changed_workspace_before_starting_the_agent() {
        let assets = tempfile::tempdir().unwrap();
        for name in ["index.html", "app.js", "app.css"] {
            std::fs::write(assets.path().join(name), "").unwrap();
        }
        let loads = Arc::new(AtomicUsize::new(0));
        let generator: ReviewAgent = Arc::new({
            let loads = Arc::clone(&loads);
            move |_prompt, _shutdown| {
                loads.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move { Ok("<p>Overview</p>".to_owned()) })
            }
        });
        let server = start_server_with_generator(&assets, generator).await;
        fs::write(
            server.state.backend.workspace.join("working.txt"),
            "changed after snapshot\n",
        )
        .unwrap();

        let response = reqwest::Client::new()
            .post(server.endpoint_url("api/overview"))
            .json(&snapshot_request(uncommitted_range()))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), reqwest::StatusCode::CONFLICT);
        assert_eq!(loads.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn overview_rejects_workspace_changes_during_generation() {
        let assets = tempfile::tempdir().unwrap();
        for name in ["index.html", "app.js", "app.css"] {
            std::fs::write(assets.path().join(name), "").unwrap();
        }
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let generator: ReviewAgent = Arc::new({
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            move |_prompt, _shutdown| {
                let started = Arc::clone(&started);
                let release = Arc::clone(&release);
                Box::pin(async move {
                    started.notify_one();
                    release.notified().await;
                    Ok("<p>Stale overview</p>".to_owned())
                })
            }
        });
        let server = start_server_with_generator(&assets, generator).await;
        let request = tokio::spawn({
            let url = server.endpoint_url("api/overview");
            async move {
                reqwest::Client::new()
                    .post(url)
                    .json(&snapshot_request(uncommitted_range()))
                    .send()
                    .await
                    .unwrap()
            }
        });
        started.notified().await;
        fs::write(
            server.state.backend.workspace.join("working.txt"),
            "changed during generation\n",
        )
        .unwrap();
        release.notify_one();

        assert_eq!(
            request.await.unwrap().status(),
            reqwest::StatusCode::CONFLICT
        );
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
        let generator: ReviewAgent = Arc::new({
            let loads = Arc::clone(&loads);
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            move |_prompt, _shutdown| {
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
        let server = start_server_with_generator(&assets, generator).await;
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
        let generator: ReviewAgent = Arc::new({
            let loads = Arc::clone(&loads);
            move |_prompt, _shutdown| {
                loads.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok("<p>Overview</p>".to_owned()) })
            }
        });
        let server = start_server_with_generator(&assets, generator).await;

        let status =
            request_overview(server.endpoint_url("api/overview"), working_tree_range()).await;

        assert_eq!(status, reqwest::StatusCode::CONFLICT);
        assert_eq!(loads.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn interrupted_overview_does_not_cancel_the_review() {
        let assets = tempfile::tempdir().unwrap();
        for name in ["index.html", "app.js", "app.css"] {
            std::fs::write(assets.path().join(name), "").unwrap();
        }
        let generator: ReviewAgent =
            Arc::new(|_prompt, _shutdown| Box::pin(async { Err(ReviewAgentError::Cancelled) }));
        let server = start_server_with_generator(&assets, generator).await;

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
    async fn cancelling_stops_an_active_overview_load() {
        let assets = tempfile::tempdir().unwrap();
        for name in ["index.html", "app.js", "app.css"] {
            std::fs::write(assets.path().join(name), "").unwrap();
        }
        let started = Arc::new(Notify::new());
        let generator: ReviewAgent = Arc::new({
            let started = Arc::clone(&started);
            move |_prompt, shutdown| {
                let started = Arc::clone(&started);
                Box::pin(async move {
                    started.notify_one();
                    shutdown.cancelled().await;
                    Err(ReviewAgentError::Cancelled)
                })
            }
        });
        let server = start_server_with_generator(&assets, generator).await;
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
        assert_eq!(
            response["page"]["selected_range"],
            serde_json::json!({ "from": 0, "to": 2 })
        );
        assert_eq!(response["page"]["full_context"], true);
        assert!(response.get("workspace_version").is_none());
        assert!(response.get("snapshot_id").is_none());
        assert!(response["page"].get("patch_id").is_none());
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
        let refreshed = client
            .post(server.endpoint_url("api/refresh"))
            .json(&serde_json::json!({ "generation": 0 }))
            .send()
            .await
            .unwrap();
        assert_eq!(refreshed.status(), reqwest::StatusCode::OK);
        let stale = client
            .post(server.endpoint_url("api/decision"))
            .json(&decision_request(0, Vec::new()))
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
            .json(&decision_request(1, Vec::new()))
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
    async fn accepted_submission_cannot_race_with_refresh() {
        let assets = tempfile::tempdir().unwrap();
        for name in ["index.html", "app.js", "app.css"] {
            std::fs::write(assets.path().join(name), "").unwrap();
        }
        let server = start_server(&assets).await;
        let outcome = server.state.outcome.lock().await;
        let client = reqwest::Client::new();
        let submit = tokio::spawn({
            let client = client.clone();
            let url = server.endpoint_url("api/decision");
            async move {
                client
                    .post(url)
                    .json(&decision_request(0, Vec::new()))
                    .send()
                    .await
                    .unwrap()
            }
        });

        tokio::time::timeout(Duration::from_secs(2), async {
            while server.state.session.try_lock().is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("submission did not reach final session validation");
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(
            server.state.session.try_lock().is_err(),
            "submission must retain the validated session until it consumes the outcome"
        );

        let refresh = tokio::spawn({
            let url = server.endpoint_url("api/refresh");
            async move {
                reqwest::Client::new()
                    .post(url)
                    .json(&serde_json::json!({ "generation": 0 }))
                    .send()
                    .await
                    .unwrap()
            }
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(50), async {
                while !refresh.is_finished() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .is_err(),
            "refresh must wait for the accepted submission"
        );

        drop(outcome);
        assert_eq!(
            submit.await.unwrap().status(),
            reqwest::StatusCode::NO_CONTENT
        );
        let _ = refresh.await.unwrap();
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
            .json(&decision_request(0, vec![comment]))
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
        start_server_with_generator(assets, review_agent()).await
    }

    async fn start_server_with_generator(
        assets: &tempfile::TempDir,
        review_agent: ReviewAgent,
    ) -> ReviewServer {
        let backend = Arc::new(ReviewBackend {
            workspace: repository().keep(),
            review_agent,
            current_version_error: None,
        });
        let review = backend.prepare(CancellationToken::new()).await.unwrap();
        ReviewServer::start(
            review,
            backend,
            "test-token".to_owned(),
            crate::review::ReviewAssets::for_test(assets.path().to_owned()),
        )
        .await
        .unwrap()
    }

    fn review_agent() -> ReviewAgent {
        Arc::new(|_prompt, _shutdown| Box::pin(async { Ok("<p>Overview</p>".to_owned()) }))
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
            "range": range,
        })
    }

    fn question_request() -> serde_json::Value {
        serde_json::json!({
            "thread_id": "thread-1",
            "operation_id": "question-1",
            "generation": 0,
            "range": uncommitted_range(),
            "path": "tracked.txt",
            "side": "additions",
            "start_line": 1,
            "end_line": 1,
            "messages": [
                { "role": "reviewer", "body": "Why was this changed?" },
                { "role": "agent", "body": "It supports the feature." },
                { "role": "reviewer", "body": "Where is that used?" }
            ]
        })
    }

    fn decision_request(generation: u64, comments: Vec<serde_json::Value>) -> serde_json::Value {
        serde_json::json!({
            "generation": generation,
            "range": uncommitted_range(),
            "decision": "approve",
            "summary": "",
            "comments": comments,
        })
    }

    fn uncommitted_range() -> ReviewRange {
        full_range()
    }

    fn working_tree_range() -> ReviewRange {
        ReviewRange { from: 1, to: 2 }
    }

    fn full_range() -> ReviewRange {
        ReviewRange { from: 0, to: 2 }
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
        git(directory.path(), ["checkout", "--quiet", "-b", "feature"]);
        fs::write(directory.path().join("tracked.txt"), "feature\n").unwrap();
        git(directory.path(), ["add", "tracked.txt"]);
        git(directory.path(), ["commit", "--quiet", "-m", "feature"]);
        fs::write(directory.path().join("working.txt"), "working\n").unwrap();
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

    fn git_stdout<const N: usize>(root: &Path, arguments: [&str; N]) -> String {
        let output = Command::new("git")
            .args(arguments)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }
}
