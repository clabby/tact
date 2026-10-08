//! The review engine: diff capture, ranges, overviews, AI review, and inline questions.
//!
//! One diff context exists per workspace. It is prepared lazily by the first request and replaced
//! when the browser refreshes it; the replacement bumps a generation that stales every request that
//! still refers to the old snapshot. Overviews, AI reviews, and question threads belong to a live
//! session and run on that session's worker as clean-context auxiliary prompts, so they stop when
//! the session closes.

use super::{
    api::secure_json,
    bridge::{AuxiliaryError, AuxiliaryRequest},
    checkout::CheckoutKind,
    diff::{self, ReviewRange},
    hub::Hub,
    wire::PROTOCOL_VERSION,
    workspaces::{Target, WorkspaceError, Workspaces},
};
use axum::{
    Json, Router,
    body::Body,
    extract::{Query, State},
    http::{Response, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};
use tokio::sync::{MappedMutexGuard, Mutex, MutexGuard, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

const MAX_COMMENTS: usize = 256;
const MAX_COMMENT_BYTES: usize = 64 * 1024;
const MAX_SUMMARY_BYTES: usize = 64 * 1024;
const MAX_PATH_BYTES: usize = 4096;
const MAX_OPERATION_ID_BYTES: usize = 128;
const MAX_THREAD_MESSAGES: usize = 64;
const MAX_THREAD_BYTES: usize = 256 * 1024;
const MAX_QUESTION_THREADS: usize = 256;
const MAX_CACHED_PAGES: usize = 8;
const MAX_CACHED_PAGE_BYTES: usize = 64 * 1024 * 1024;
const MAX_CACHED_OVERVIEWS: usize = 8;
const MAX_CACHED_OVERVIEW_BYTES: usize = 8 * 1024 * 1024;
const MAX_OVERVIEW_INSTRUCTIONS_BYTES: usize = 8 * 1024;

/// Range, instructions, and the owning session.
type OverviewCacheKey = (diff::ReviewRange, Option<String>, String);
/// Generation, range, instructions, and the owning session.
type OverviewOperationKey = (u64, diff::ReviewRange, Option<String>, String);
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
    pub(super) selected_range: diff::ReviewRange,
    pub(super) full_context: bool,
    #[serde(flatten)]
    pub(super) diff: diff::DiffSnapshot,
}

#[derive(Clone, Serialize)]
pub(super) struct ReviewBootstrap {
    pub(super) protocol_version: u32,
    pub(super) generation: u64,
    pub(super) title: String,
    pub(super) repository: String,
    /// The checkout this review reads.
    pub(super) checkout: CheckoutIdentity,
    pub(super) trunk: String,
    pub(super) range_targets: Vec<diff::ReviewTarget>,
    pub(super) default_range: diff::ReviewRange,
}

#[derive(Clone, Serialize)]
pub(super) struct CheckoutIdentity {
    path: PathBuf,
    name: String,
    label: String,
    kind: CheckoutKind,
}

#[derive(Clone)]
pub(super) struct PreparedReview {
    pub(super) bootstrap: ReviewBootstrap,
    pub(super) initial_page: ReviewPage,
    pub(super) context: diff::ReviewContext,
    pub(super) version: diff::WorkspaceVersion,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RangeRequest {
    generation: u64,
    range: diff::ReviewRange,
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    checkout: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OverviewRequest {
    session: String,
    #[serde(default)]
    checkout: Option<String>,
    generation: u64,
    range: diff::ReviewRange,
    instructions: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AiReviewRequest {
    session: String,
    #[serde(default)]
    checkout: Option<String>,
    generation: u64,
    range: diff::ReviewRange,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GenerationRequest {
    generation: u64,
    /// The session whose overview and questions the response includes.
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    checkout: Option<String>,
}

#[derive(Deserialize)]
struct SessionQuery {
    session: Option<String>,
    checkout: Option<String>,
}

#[derive(Serialize)]
struct OverviewResponse {
    generation: u64,
    selected_range: diff::ReviewRange,
    overview_mdx: String,
    instructions: Option<String>,
}

#[derive(Serialize)]
struct AiReviewResponse {
    generation: u64,
    selected_range: diff::ReviewRange,
    comments: Vec<AiReviewComment>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AiReviewResult {
    pub(super) comments: Vec<AiReviewComment>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AiReviewComment {
    path: String,
    side: CommentSide,
    start_line: u32,
    end_line: u32,
    body: String,
}

#[derive(Clone, Serialize)]
struct StoredOverview {
    selected_range: diff::ReviewRange,
    status: OverviewStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    overview_mdx: Option<String>,
    instructions: Option<String>,
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
    pub(super) session: String,
    #[serde(default)]
    pub(super) checkout: Option<String>,
    pub(super) thread_id: String,
    pub(super) operation_id: String,
    pub(super) generation: u64,
    pub(super) range: diff::ReviewRange,
    pub(super) path: String,
    pub(super) side: CommentSide,
    pub(super) start_line: u32,
    pub(super) end_line: u32,
    pub(super) messages: Vec<ThreadMessage>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QuestionCancelRequest {
    /// Accepted for symmetry with the other session-scoped requests; operations are looked up by id.
    #[serde(default, rename = "session")]
    _session: serde::de::IgnoredAny,
    operation_id: String,
    generation: u64,
    range: diff::ReviewRange,
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
    selected_range: diff::ReviewRange,
    answer: String,
}

#[derive(Clone, Serialize)]
struct StoredQuestion {
    #[serde(skip)]
    session: String,
    thread_id: String,
    operation_id: String,
    generation: u64,
    range: diff::ReviewRange,
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
struct RefreshResponse {
    #[serde(flatten)]
    bootstrap: ReviewBootstrap,
    page: ReviewPage,
    overview: Option<StoredOverview>,
    questions: Vec<StoredQuestion>,
    turn_running: bool,
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
    InvalidOverviewInstructions,
    WorkspaceChanged,
    OverviewFailed,
    AiReviewFailed,
    QuestionFailed,
    InvalidThread,
    TurnRunning,
    OperationCancelled,
    SessionCancelled,
    InvalidCommentAnchor,
    UnknownSession,
    InvalidCheckout,
}

pub(super) enum ScopeLoadError {
    Cancelled,
    Failed(String),
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReviewDecision {
    #[serde(default)]
    pub(super) session: Option<String>,
    #[serde(default)]
    pub(super) checkout: Option<String>,
    /// The reviewed checkout when it is not the session's workspace; the composed review names it.
    #[serde(skip_deserializing, default)]
    pub(super) reviewed_in: Option<PathBuf>,
    pub(super) generation: u64,
    pub(super) range: diff::ReviewRange,
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

const WORKSPACE_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Tells connected browsers when the working tree changes so they can mark their diff stale.
///
/// The workspace is only inspected while a stream is connected. Failed inspections are skipped:
/// the next poll retries, and a transient git error must not look like a change.
pub(super) async fn watch_workspace(state: Arc<ReviewState>, shutdown: CancellationToken) {
    #[derive(Serialize)]
    struct WorkspaceEvent<'a> {
        version: String,
        /// The checkout whose files changed, so a client reviewing another one can ignore it.
        checkout: &'a Path,
    }

    let mut seen: Option<diff::WorkspaceVersion> = None;
    let mut poll = tokio::time::interval(WORKSPACE_POLL_INTERVAL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return,
            _ = poll.tick() => {}
        }
        if state.hub.connected_clients() == 0 {
            seen = None;
            continue;
        }
        let Ok(version) = state.backend.current_version(shutdown.clone()).await else {
            continue;
        };
        if seen.as_ref().is_some_and(|seen| *seen != version) {
            state.hub.broadcast(
                "workspace",
                &WorkspaceEvent {
                    version: version.to_hex(),
                    checkout: &state.backend.workspace,
                },
            );
        }
        seen = Some(version);
    }
}

/// Routes for the review engine. The caller supplies authentication and request limits.
pub(super) fn router<S>(registry: Arc<ReviewRegistry>) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::<Arc<ReviewRegistry>>::new()
        .route("/api/review", get(review))
        .route("/api/review/compose", post(compose))
        .route("/api/refresh", post(refresh_review))
        .route("/api/range", post(load_range))
        .route("/api/overview", post(load_overview))
        .route("/api/ai-review", post(run_ai_review))
        .route("/api/question", post(ask_question))
        .route("/api/questions", post(list_questions))
        .route("/api/question/cancel", post(cancel_question))
        .with_state(registry)
}

/// The most checkouts whose review is kept at once. A checkout reviewed longer ago is dropped, with
/// its cached pages and watcher, and prepared again when it is next opened.
const MAX_REVIEWED_CHECKOUTS: usize = 4;

struct ReviewedCheckout {
    path: PathBuf,
    state: Arc<ReviewState>,
    /// Stops the checkout's watcher and its operations.
    stop: CancellationToken,
}

/// The review of every checkout clients have opened, most recently used first.
///
/// A request names its checkout, or its session, whose workspace is the default; the registry
/// resolves that to one of the repository's checkouts and returns the review kept for it.
pub(super) struct ReviewRegistry {
    workspaces: Arc<Workspaces>,
    hub: Hub,
    review_agent: ReviewAgent,
    shutdown: CancellationToken,
    reviewed: Mutex<Vec<ReviewedCheckout>>,
}

impl ReviewRegistry {
    pub(super) fn new(
        workspaces: Arc<Workspaces>,
        hub: Hub,
        review_agent: ReviewAgent,
        shutdown: CancellationToken,
    ) -> Arc<Self> {
        Arc::new(Self {
            workspaces,
            hub,
            review_agent,
            shutdown,
            reviewed: Mutex::new(Vec::new()),
        })
    }

    /// The review for the checkout a request names, with the checkout it resolved to.
    async fn resolve(
        &self,
        session: Option<&str>,
        checkout: Option<&str>,
    ) -> Result<(Arc<ReviewState>, Target), Box<Response<Body>>> {
        let target = self
            .workspaces
            .resolve(session, checkout)
            .await
            .map_err(|error| Box::new(checkout_error(&error)))?;
        let mut reviewed = self.reviewed.lock().await;
        if let Some(index) = reviewed.iter().position(|kept| kept.path == target.path) {
            let kept = reviewed.remove(index);
            let state = Arc::clone(&kept.state);
            reviewed.insert(0, kept);
            return Ok((state, target));
        }
        let stop = self.shutdown.child_token();
        let state = ReviewState::new(
            target.clone(),
            self.hub.clone(),
            Arc::clone(&self.review_agent),
            stop.clone(),
        );
        tokio::spawn(watch_workspace(Arc::clone(&state), stop.clone()));
        reviewed.insert(
            0,
            ReviewedCheckout {
                path: target.path.clone(),
                state: Arc::clone(&state),
                stop,
            },
        );
        let kept = MAX_REVIEWED_CHECKOUTS.min(reviewed.len());
        for evicted in reviewed.drain(kept..) {
            evicted.stop.cancel();
        }
        Ok((state, target))
    }

    /// Every review currently kept, for requests that identify their operation instead of a
    /// checkout.
    async fn all(&self) -> Vec<Arc<ReviewState>> {
        self.reviewed
            .lock()
            .await
            .iter()
            .map(|kept| Arc::clone(&kept.state))
            .collect()
    }
}

fn checkout_error(error: &WorkspaceError) -> Response<Body> {
    match error {
        WorkspaceError::UnknownSession => unknown_session(),
        WorkspaceError::NotACheckout(_) => error_response(
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidCheckout,
            error.to_string(),
            false,
            true,
        ),
    }
}

macro_rules! resolve {
    ($registry:expr, $session:expr, $checkout:expr) => {
        match $registry.resolve($session, $checkout).await {
            Ok(resolved) => resolved,
            Err(response) => return *response,
        }
    };
}

/// The review engine's shared state, one per process.
pub(super) struct ReviewState {
    /// `None` until the first request prepares the workspace's diff context.
    session: Mutex<Option<ReviewSession>>,
    backend: ReviewBackend,
    hub: Hub,
    overview_operations: OverviewOperations,
    active_questions: StdMutex<HashMap<String, ActiveQuestion>>,
    /// Serializes preparation so concurrent first requests load the diff once.
    refresh_generation: Mutex<()>,
    shutdown: CancellationToken,
}

macro_rules! loaded {
    ($state:expr) => {
        match $state.session().await {
            Ok(session) => session,
            Err(response) => return *response,
        }
    };
}

impl ReviewState {
    fn new(
        target: Target,
        hub: Hub,
        review_agent: ReviewAgent,
        shutdown: CancellationToken,
    ) -> Arc<Self> {
        Arc::new(Self {
            session: Mutex::new(None),
            backend: ReviewBackend {
                workspace: target.path.clone(),
                identity: CheckoutIdentity {
                    path: target.path,
                    name: target.name,
                    label: target.label,
                    kind: target.kind,
                },
                hub: hub.clone(),
                review_agent,
                #[cfg(test)]
                current_version_error: None,
            },
            hub,
            overview_operations: Mutex::new(HashMap::new()),
            active_questions: StdMutex::new(HashMap::new()),
            refresh_generation: Mutex::new(()),
            shutdown,
        })
    }

    fn turn_running(&self) -> bool {
        self.hub.any_busy()
    }

    /// The prepared review, or a stale-snapshot response if none has been loaded yet.
    async fn session(&self) -> Result<MappedMutexGuard<'_, ReviewSession>, Box<Response<Body>>> {
        MutexGuard::try_map(self.session.lock().await, Option::as_mut)
            .map_err(|_| Box::new(stale_snapshot("the review has not been loaded")))
    }
}

struct ActiveQuestion {
    generation: u64,
    range: diff::ReviewRange,
    cancellation: CancellationToken,
}

struct ActiveQuestionRegistration {
    state: Arc<ReviewState>,
    operation_id: String,
    cancellation: CancellationToken,
}

impl Drop for ActiveQuestionRegistration {
    fn drop(&mut self) {
        self.cancellation.cancel();
        let Ok(mut active) = self.state.active_questions.lock() else {
            return;
        };
        active.remove(&self.operation_id);
    }
}

struct ReviewSession {
    generation: u64,
    bootstrap: ReviewBootstrap,
    default_page: ReviewPage,
    selected_page: ReviewPage,
    context: diff::ReviewContext,
    range_pages: BoundedCache<diff::ReviewRange, ReviewPage>,
    overviews: BoundedCache<OverviewCacheKey, String>,
    /// Per session: the overview currently being generated and the one the browser has selected.
    active_overview: HashMap<String, OverviewOperationKey>,
    selected_overview_key: HashMap<String, OverviewCacheKey>,
    questions: Vec<StoredQuestion>,
    version: diff::WorkspaceVersion,
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

    fn touch(&mut self, key: &K) -> Option<&V> {
        if !self.values.contains_key(key) {
            return None;
        }
        self.order.retain(|existing| existing != key);
        self.order.push_back(key.clone());
        self.get(key)
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
            active_overview: HashMap::new(),
            selected_overview_key: HashMap::new(),
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

    fn page(&self, range: &diff::ReviewRange) -> Option<&ReviewPage> {
        if range == &self.bootstrap.default_range {
            return Some(&self.default_page);
        }
        self.range_pages.get(range)
    }

    fn insert_page(&mut self, page: ReviewPage) {
        let bytes = page.diff.patch.len();
        self.range_pages.insert(page.selected_range, page, bytes);
    }

    fn questions_for(&self, session: &str) -> Vec<StoredQuestion> {
        self.questions
            .iter()
            .filter(|question| question.session == session)
            .cloned()
            .collect()
    }

    fn finish_overview(&mut self, key: &OverviewOperationKey) {
        if self.active_overview.get(&key.3) == Some(key) {
            self.active_overview.remove(&key.3);
        }
    }

    fn selected_overview(&self, session: &str) -> Option<StoredOverview> {
        let range = self.selected_page.selected_range;
        let selected = self.selected_overview_key.get(session);
        if let Some((_, active_range, instructions, _)) = self.active_overview.get(session)
            && *active_range == range
            && selected.is_some_and(|key| key.0 == range && key.1 == *instructions)
        {
            return Some(StoredOverview {
                selected_range: range,
                status: OverviewStatus::Generating,
                overview_mdx: None,
                instructions: instructions.clone(),
            });
        }
        let key = selected
            .filter(|key| key.0 == range && self.overviews.get(key).is_some())
            .or_else(|| {
                self.overviews
                    .order
                    .iter()
                    .rev()
                    .find(|key| key.0 == range && key.2 == session)
            })?;
        self.overviews.get(key).map(|overview_mdx| StoredOverview {
            selected_range: range,
            status: OverviewStatus::Ready,
            overview_mdx: Some(overview_mdx.clone()),
            instructions: key.1.clone(),
        })
    }
}

/// Prepares the workspace's review on first use.
async fn ensure_loaded(state: &ReviewState) -> Result<(), Box<Response<Body>>> {
    let _preparing = state.refresh_generation.lock().await;
    if state.session.lock().await.is_some() {
        return Ok(());
    }
    let review = state
        .backend
        .prepare(state.shutdown.child_token())
        .await
        .map_err(|error| Box::new(preparation_failure(error)))?;
    *state.session.lock().await = Some(ReviewSession::new(review, state.shutdown.clone()));
    Ok(())
}

fn preparation_failure(error: ReviewError) -> Response<Body> {
    match error {
        ReviewError::Cancelled => operation_cancelled("review preparation was cancelled"),
        ReviewError::Diff(diff::DiffError::NotRepository(_)) => error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::WorkspaceChanged,
            "The folder must be a git repository.",
            false,
            false,
        ),
        error => error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::WorkspaceChanged,
            error.to_string(),
            true,
            false,
        ),
    }
}

async fn review(
    State(registry): State<Arc<ReviewRegistry>>,
    Query(query): Query<SessionQuery>,
) -> Response<Body> {
    let (state, _) = resolve!(
        registry,
        query.session.as_deref(),
        query.checkout.as_deref()
    );
    if let Err(response) = ensure_loaded(&state).await {
        return *response;
    }
    let session_id = query.session.as_deref().unwrap_or_default();
    let session = loaded!(state);
    secure_json(
        StatusCode::OK,
        RefreshResponse {
            bootstrap: session.bootstrap.clone(),
            page: session.selected_page.clone(),
            overview: session.selected_overview(session_id),
            questions: session.questions_for(session_id),
            turn_running: state.turn_running(),
        },
    )
}

/// Renders a review decision as the Markdown a browser inserts into a session's draft.
async fn compose(
    State(registry): State<Arc<ReviewRegistry>>,
    Json(mut decision): Json<ReviewDecision>,
) -> Response<Body> {
    let (state, target) = resolve!(
        registry,
        decision.session.as_deref(),
        decision.checkout.as_deref()
    );
    decision.reviewed_in = (target.path != target.session_workspace).then(|| target.path.clone());
    if invalid_decision(&decision) {
        return error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::InvalidCommentAnchor,
            "invalid review decision",
            false,
            true,
        );
    }
    let session = loaded!(state);
    let Some(page) = matching_page(&session, decision.generation, &decision.range) else {
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
    decision.scope.clone_from(&page.diff.scope);
    secure_json(
        StatusCode::OK,
        ComposeResponse {
            markdown: decision.to_markdown(),
        },
    )
}

#[derive(Serialize)]
struct ComposeResponse {
    markdown: String,
}

async fn refresh_review(
    State(registry): State<Arc<ReviewRegistry>>,
    Json(request): Json<GenerationRequest>,
) -> Response<Body> {
    let (state, _) = resolve!(
        registry,
        request.session.as_deref(),
        request.checkout.as_deref()
    );
    let session_id = request.session.as_deref().unwrap_or_default();
    let _refresh = state.refresh_generation.lock().await;
    let shutdown = {
        let session = loaded!(state);
        if request.generation != session.generation {
            return stale_snapshot("the review generation is stale");
        }
        session.generation_shutdown.clone()
    };
    let review = match state.backend.prepare(shutdown).await {
        Ok(review) => review,
        Err(error) => return preparation_failure(error),
    };
    let mut session = loaded!(state);
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
            overview: session.selected_overview(session_id),
            questions: session.questions_for(session_id),
            turn_running: state.turn_running(),
        },
    )
}

async fn load_range(
    State(registry): State<Arc<ReviewRegistry>>,
    Json(request): Json<RangeRequest>,
) -> Response<Body> {
    let (state, _) = resolve!(
        registry,
        request.session.as_deref(),
        request.checkout.as_deref()
    );
    let (context, generation, version, shutdown) = {
        let mut session = loaded!(state);
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
        .map_err(scope_load_error)
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
    let mut session = loaded!(state);
    if session.generation != generation {
        return stale_snapshot("the review changed while this range was loading");
    }
    let page = ReviewPage { generation, ..page };
    session.insert_page(page.clone());
    session.selected_page = page.clone();
    secure_json(StatusCode::OK, page)
}

async fn load_overview(
    State(registry): State<Arc<ReviewRegistry>>,
    Json(mut request): Json<OverviewRequest>,
) -> Response<Body> {
    let (state, _) = resolve!(
        registry,
        Some(&request.session),
        request.checkout.as_deref()
    );
    if state.turn_running() {
        return turn_running();
    }
    if !state.hub.is_live(&request.session) {
        return unknown_session();
    }
    request.instructions = request
        .instructions
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    if request
        .instructions
        .as_ref()
        .is_some_and(|value| value.len() > MAX_OVERVIEW_INSTRUCTIONS_BYTES)
    {
        return error_response(
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidOverviewInstructions,
            "overview instructions exceed 8 KiB",
            true,
            true,
        );
    }
    let cache_key = (
        request.range,
        request.instructions.clone(),
        request.session.clone(),
    );
    let overview_key = (
        request.generation,
        request.range,
        request.instructions.clone(),
        request.session.clone(),
    );
    let (operation, joined) = {
        let mut operations = state.overview_operations.lock().await;
        match operations.entry(overview_key.clone()) {
            std::collections::hash_map::Entry::Occupied(entry) => (Arc::clone(entry.get()), true),
            std::collections::hash_map::Entry::Vacant(entry) => (
                Arc::clone(entry.insert(Arc::new(OverviewOperation::new()))),
                false,
            ),
        }
    };
    if joined {
        let mut session = loaded!(state);
        if state.turn_running() {
            return turn_running();
        }
        if matching_page(&session, request.generation, &request.range).is_none() {
            return stale_snapshot("the requested review snapshot is stale");
        }
        session
            .selected_overview_key
            .insert(request.session.clone(), cache_key.clone());
        session
            .active_overview
            .insert(request.session.clone(), overview_key.clone());
    }
    let operation_gate = Arc::clone(&operation.gate).lock_owned().await;
    if let Some(result) = operation.result.lock().await.clone() {
        let mut session = loaded!(state);
        session.finish_overview(&overview_key);
        drop(session);
        state.overview_operations.lock().await.remove(&overview_key);
        return overview_response(&state, overview_key, result);
    }
    let (page, version, shutdown) = {
        let mut session = loaded!(state);
        if state.turn_running() {
            drop(session);
            discard_idle_overview_operation(&state, &overview_key, &operation).await;
            return turn_running();
        }
        let Some(page) = matching_page(&session, request.generation, &request.range).cloned()
        else {
            drop(session);
            discard_idle_overview_operation(&state, &overview_key, &operation).await;
            return stale_snapshot("the requested review snapshot is stale");
        };
        if let Some(overview_mdx) = session.overviews.touch(&cache_key).cloned() {
            session
                .selected_overview_key
                .insert(request.session.clone(), cache_key.clone());
            drop(session);
            discard_idle_overview_operation(&state, &overview_key, &operation).await;
            return secure_json(
                StatusCode::OK,
                OverviewResponse {
                    generation: request.generation,
                    selected_range: request.range,
                    overview_mdx,
                    instructions: request.instructions,
                },
            );
        }
        (
            page,
            session.version.clone(),
            session.generation_shutdown.clone(),
        )
    };
    {
        let mut session = loaded!(state);
        if matching_page(&session, request.generation, &request.range).is_none() {
            drop(session);
            discard_idle_overview_operation(&state, &overview_key, &operation).await;
            return stale_snapshot("the requested review snapshot is stale");
        }
        session
            .active_overview
            .insert(request.session.clone(), overview_key.clone());
        session
            .selected_overview_key
            .insert(request.session.clone(), cache_key);
    }

    let (completion, response) = oneshot::channel();
    let task_state = Arc::clone(&state);
    tokio::spawn(async move {
        let _operation_gate = operation_gate;
        let result = run_overview(
            &task_state,
            &overview_key.3,
            &page,
            version,
            overview_key.2.as_deref(),
            shutdown,
        )
        .await;
        let result = store_overview_result(&task_state, &overview_key, result).await;
        *operation.result.lock().await = Some(result.clone());
        let initiating_browser_is_connected = completion
            .send(overview_response(&task_state, overview_key.clone(), result))
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

async fn discard_idle_overview_operation(
    state: &ReviewState,
    key: &OverviewOperationKey,
    operation: &Arc<OverviewOperation>,
) {
    let mut operations = state.overview_operations.lock().await;
    if Arc::strong_count(operation) == 2
        && operations
            .get(key)
            .is_some_and(|current| Arc::ptr_eq(current, operation))
    {
        operations.remove(key);
    }
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
    state: &ReviewState,
    session: &str,
    page: &ReviewPage,
    version: diff::WorkspaceVersion,
    instructions: Option<&str>,
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
    let overview_mdx = match state
        .backend
        .overview(
            session,
            &page.diff.scope,
            &page.diff.overview,
            instructions,
            shutdown.clone(),
        )
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
    OverviewRunResult::Ready(overview_mdx)
}

async fn store_overview_result(
    state: &ReviewState,
    overview_key: &OverviewOperationKey,
    result: OverviewRunResult,
) -> OverviewRunResult {
    let Ok(mut session) = state.session().await else {
        return OverviewRunResult::Stale("the review is no longer loaded");
    };
    session.finish_overview(overview_key);
    if state.turn_running() {
        return OverviewRunResult::Stale(
            "the agent started another turn during overview generation",
        );
    }
    if matching_page(&session, overview_key.0, &overview_key.1).is_none() {
        return OverviewRunResult::Stale("the review changed while its overview was loading");
    }
    if let OverviewRunResult::Ready(overview_mdx) = &result {
        session.overviews.insert(
            (
                overview_key.1,
                overview_key.2.clone(),
                overview_key.3.clone(),
            ),
            overview_mdx.clone(),
            overview_mdx.len(),
        );
    }
    result
}

fn overview_response(
    state: &ReviewState,
    overview_key: OverviewOperationKey,
    result: OverviewRunResult,
) -> Response<Body> {
    match result {
        OverviewRunResult::Ready(overview_mdx) => secure_json(
            StatusCode::OK,
            OverviewResponse {
                generation: overview_key.0,
                selected_range: overview_key.1,
                overview_mdx,
                instructions: overview_key.2,
            },
        ),
        OverviewRunResult::Cancelled => {
            cancelled(state, &overview_key.3, "overview generation was cancelled")
        }
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

async fn run_ai_review(
    State(registry): State<Arc<ReviewRegistry>>,
    Json(request): Json<AiReviewRequest>,
) -> Response<Body> {
    let (state, _) = resolve!(
        registry,
        Some(&request.session),
        request.checkout.as_deref()
    );
    if state.turn_running() {
        return turn_running();
    }
    if !state.hub.is_live(&request.session) {
        return unknown_session();
    }
    let (page, version, shutdown) = {
        let session = loaded!(state);
        let Some(page) = matching_page(&session, request.generation, &request.range) else {
            return stale_snapshot("the requested review snapshot is stale");
        };
        (
            page.clone(),
            session.version.clone(),
            session.generation_shutdown.clone(),
        )
    };
    let current = match state.backend.current_version(shutdown.clone()).await {
        Ok(current) => current,
        Err(ScopeLoadError::Cancelled) => {
            return stale_snapshot("the review changed before AI review started");
        }
        Err(ScopeLoadError::Failed(error)) => return workspace_error(error),
    };
    if current != version {
        return stale_snapshot("the workspace changed before AI review started");
    }

    let comments = match state
        .backend
        .ai_review(
            &request.session,
            &page.diff.scope,
            &page.diff.overview,
            shutdown.clone(),
        )
        .await
    {
        Ok(comments) => comments,
        Err(ScopeLoadError::Cancelled) => {
            return cancelled(&state, &request.session, "AI review was cancelled");
        }
        Err(ScopeLoadError::Failed(error)) => {
            return error_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                ErrorCode::AiReviewFailed,
                error,
                true,
                true,
            );
        }
    };
    let current = match state.backend.current_version(shutdown).await {
        Ok(current) => current,
        Err(ScopeLoadError::Cancelled) => {
            return stale_snapshot("the review changed during AI review");
        }
        Err(ScopeLoadError::Failed(error)) => return workspace_error(error),
    };
    if current != version {
        return stale_snapshot("the workspace changed during AI review");
    }
    let session = loaded!(state);
    if state.turn_running() {
        return turn_running();
    }
    if matching_page(&session, request.generation, &request.range).is_none() {
        return stale_snapshot("the review changed during AI review");
    }
    if comments.len() > MAX_COMMENTS
        || comments.iter().any(|comment| {
            invalid_ai_comment(comment)
                || !valid_anchor(
                    &page.diff,
                    &comment.path,
                    comment.side,
                    comment.start_line,
                    comment.end_line,
                )
        })
    {
        return error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::AiReviewFailed,
            "the agent returned invalid or unanchored review comments",
            true,
            true,
        );
    }
    secure_json(
        StatusCode::OK,
        AiReviewResponse {
            generation: request.generation,
            selected_range: request.range,
            comments,
        },
    )
}

fn invalid_ai_comment(comment: &AiReviewComment) -> bool {
    comment.path.trim().is_empty()
        || comment.path.len() > MAX_PATH_BYTES
        || comment.body.trim().is_empty()
        || comment.body.len() > MAX_COMMENT_BYTES
        || !["[P0] ", "[P1] ", "[P2] ", "[P3] "]
            .iter()
            .any(|prefix| comment.body.starts_with(prefix) && comment.body.len() > prefix.len())
        || comment.start_line == 0
        || comment.end_line < comment.start_line
}

fn workspace_error(error: String) -> Response<Body> {
    error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        ErrorCode::WorkspaceChanged,
        error,
        true,
        false,
    )
}

async fn ask_question(
    State(registry): State<Arc<ReviewRegistry>>,
    Json(request): Json<QuestionRequest>,
) -> Response<Body> {
    let (state, _) = resolve!(
        registry,
        Some(&request.session),
        request.checkout.as_deref()
    );
    if state.turn_running() {
        return turn_running();
    }
    if !state.hub.is_live(&request.session) {
        return unknown_session();
    }
    if invalid_question(&request) {
        return invalid_thread("the question thread is invalid");
    }
    let (page, version, shutdown) = {
        let session = loaded!(state);
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
    let operation_shutdown = {
        let mut session = loaded!(state);
        if state.turn_running() {
            return turn_running();
        }
        let Ok(mut active) = state.active_questions.lock() else {
            return internal_error("the active question state is unavailable");
        };
        if active.contains_key(&request.operation_id) {
            return invalid_thread("the question operation identifier is already in use");
        }
        if !begin_stored_question(&mut session, &request) {
            return invalid_thread("the question does not continue its stored thread");
        }
        let cancellation = shutdown.child_token();
        active.insert(
            request.operation_id.clone(),
            ActiveQuestion {
                generation: request.generation,
                range: request.range,
                cancellation: cancellation.clone(),
            },
        );
        cancellation
    };
    let (completion, response) = oneshot::channel();
    let task_state = Arc::clone(&state);
    tokio::spawn(async move {
        let completion_shutdown = operation_shutdown.clone();
        let _registration = ActiveQuestionRegistration {
            state: Arc::clone(&task_state),
            operation_id: request.operation_id.clone(),
            cancellation: operation_shutdown.clone(),
        };
        let mut result =
            run_question(&task_state, &request, &page, version, operation_shutdown).await;
        if completion_shutdown.is_cancelled() || task_state.turn_running() {
            result = QuestionRunResult::Cancelled;
        }
        store_question_result(&task_state, &request, &mut result).await;
        let _ = completion.send(question_response(&task_state, &request, result));
    });

    response
        .await
        .unwrap_or_else(|_| internal_error("the question operation stopped unexpectedly"))
}

async fn list_questions(
    State(registry): State<Arc<ReviewRegistry>>,
    Json(request): Json<GenerationRequest>,
) -> Response<Body> {
    let Some(session_id) = request.session.clone() else {
        return invalid_thread("the question list requires a session");
    };
    // Listing is allowed for a session that has since closed: it finds nothing.
    let live = registry
        .hub
        .is_live(&session_id)
        .then_some(session_id.as_str());
    let (state, _) = resolve!(registry, live, request.checkout.as_deref());
    let session = loaded!(state);
    if session.generation != request.generation {
        return stale_snapshot("the question list belongs to an older review generation");
    }
    secure_json(
        StatusCode::OK,
        QuestionListResponse {
            generation: session.generation,
            questions: session.questions_for(&session_id),
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
    state: &ReviewState,
    request: &QuestionRequest,
    page: &ReviewPage,
    version: diff::WorkspaceVersion,
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
            &request.session,
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
    let Ok(session) = state.session().await else {
        return QuestionRunResult::Stale("the review is no longer loaded");
    };
    if matching_page(&session, request.generation, &request.range).is_none() {
        return QuestionRunResult::Stale("the review changed while the question was answered");
    }
    QuestionRunResult::Answer(answer)
}

async fn store_question_result(
    state: &ReviewState,
    request: &QuestionRequest,
    result: &mut QuestionRunResult,
) {
    let Ok(mut session) = state.session().await else {
        return;
    };
    if state.turn_running() {
        *result = QuestionRunResult::Cancelled;
    }
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

fn question_response(
    state: &ReviewState,
    request: &QuestionRequest,
    result: QuestionRunResult,
) -> Response<Body> {
    match result {
        QuestionRunResult::Answer(answer) => secure_json(
            StatusCode::OK,
            QuestionResponse {
                generation: request.generation,
                selected_range: request.range,
                answer,
            },
        ),
        QuestionRunResult::Cancelled => {
            cancelled(state, &request.session, "question answering was cancelled")
        }
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
    State(registry): State<Arc<ReviewRegistry>>,
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
    // The operation identifier names the question; which checkout it belongs to does not matter.
    for state in registry.all().await {
        let Ok(active) = state.active_questions.lock() else {
            return internal_error("the active question state is unavailable");
        };
        if let Some(question) = active.get(&request.operation_id)
            && question.generation == request.generation
            && question.range == request.range
        {
            question.cancellation.cancel();
        }
    }
    StatusCode::NO_CONTENT.into_response()
}

fn matching_page<'a>(
    session: &'a ReviewSession,
    generation: u64,
    range: &diff::ReviewRange,
) -> Option<&'a ReviewPage> {
    if generation != session.generation {
        return None;
    }
    session.page(range)
}

fn valid_anchor(
    snapshot: &diff::DiffSnapshot,
    path: &str,
    side: CommentSide,
    start_line: u32,
    end_line: u32,
) -> bool {
    let side = match side {
        CommentSide::Additions => diff::PatchSide::Additions,
        CommentSide::Deletions => diff::PatchSide::Deletions,
    };
    snapshot.contains_anchor(path, side, start_line, end_line)
}

fn valid_comment_anchor(snapshot: &diff::DiffSnapshot, comment: &ReviewComment) -> bool {
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
        .find(|thread| thread.thread_id == request.thread_id && thread.session == request.session)
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
        session: request.session.clone(),
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
fn turn_running() -> Response<Body> {
    error_response(
        StatusCode::CONFLICT,
        ErrorCode::TurnRunning,
        "The agent turn is still running. Review actions are available when it finishes.",
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

fn unknown_session() -> Response<Body> {
    error_response(
        StatusCode::NOT_FOUND,
        ErrorCode::UnknownSession,
        "the session is not live",
        false,
        true,
    )
}

/// A cancellation caused by the session closing is reported distinctly from a user cancel.
fn cancelled(state: &ReviewState, session: &str, message: &str) -> Response<Body> {
    if state.hub.is_live(session) {
        return operation_cancelled(message);
    }
    error_response(
        StatusCode::CONFLICT,
        ErrorCode::SessionCancelled,
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

const MAX_OVERVIEW_BYTES: usize = 1024 * 1024;
const MAX_QUESTION_ANSWER_BYTES: usize = 256 * 1024;
const MAX_AI_REVIEW_BYTES: usize = 1024 * 1024;
const MAX_PREPARATION_ATTEMPTS: usize = 3;

/// Runs one clean-context prompt on a session's worker and returns the agent's final text.
pub(super) type ReviewAgent = Arc<
    dyn Fn(String, String, CancellationToken) -> BoxFuture<'static, Result<String, AuxiliaryError>>
        + Send
        + Sync,
>;

/// The production [`ReviewAgent`]: auxiliary requests handled by the terminal event loop.
pub(super) fn bridge_agent(requests: mpsc::UnboundedSender<AuxiliaryRequest>) -> ReviewAgent {
    Arc::new(move |session, prompt, shutdown| {
        let requests = requests.clone();
        Box::pin(async move {
            let (completion, result) = oneshot::channel();
            let request = AuxiliaryRequest {
                session,
                prompt,
                shutdown,
                completion,
            };
            requests
                .send(request)
                .map_err(|_| AuxiliaryError::Failed("the agent worker stopped".to_owned()))?;
            result
                .await
                .map_err(|_| AuxiliaryError::Failed("the agent worker stopped".to_owned()))?
        })
    })
}

struct ReviewBackend {
    workspace: PathBuf,
    identity: CheckoutIdentity,
    hub: Hub,
    review_agent: ReviewAgent,
    #[cfg(test)]
    current_version_error: Option<String>,
}

fn scope_load_error(error: ReviewError) -> ScopeLoadError {
    match error {
        ReviewError::Cancelled => ScopeLoadError::Cancelled,
        error => ScopeLoadError::Failed(error.to_string()),
    }
}

impl ReviewBackend {
    /// Captures the workspace's default page. A live turn can change the workspace between the
    /// patch read and its validation; that is tolerated while any session runs, because the
    /// workspace watcher reports later changes.
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
                    ScopeLoadError::Failed(error) => ReviewError::WorkspaceValidation(error),
                })?
                != version
                && !self.hub.any_busy()
            {
                continue;
            }
            let bootstrap = ReviewBootstrap {
                protocol_version: PROTOCOL_VERSION,
                generation: 0,
                title: format!("Review {}", context.repository()),
                repository: context.repository().to_owned(),
                checkout: self.identity.clone(),
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
        #[cfg(test)]
        if let Some(error) = &self.current_version_error {
            return Err(ScopeLoadError::Failed(error.clone()));
        }

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
            full_context: true,
            diff,
        })
    }

    async fn overview(
        &self,
        session: &str,
        label: &str,
        context: &diff::OverviewContext,
        instructions: Option<&str>,
        shutdown: CancellationToken,
    ) -> Result<String, ScopeLoadError> {
        let prompt = overview_prompt(label, context, instructions);
        let result = self.run_agent(session, prompt, shutdown).await?;
        let overview = strip_mdx_fence(result.trim());
        if overview.is_empty() {
            return Err(ScopeLoadError::Failed(
                "the agent returned an empty review overview".to_owned(),
            ));
        }
        if overview.len() > MAX_OVERVIEW_BYTES {
            return Err(ScopeLoadError::Failed(
                "the agent returned a review overview larger than 1 MiB".to_owned(),
            ));
        }
        Ok(overview.to_owned())
    }

    /// Runs the prompt on `session`'s worker, abandoning it when the session closes.
    async fn run_agent(
        &self,
        session: &str,
        prompt: String,
        shutdown: CancellationToken,
    ) -> Result<String, ScopeLoadError> {
        let Some(closed) = self.hub.closed_token(session) else {
            return Err(ScopeLoadError::Failed(
                "the session is no longer live".to_owned(),
            ));
        };
        let run = shutdown.child_token();
        let result = tokio::select! {
            result = (self.review_agent)(session.to_owned(), prompt, run.clone()) => result,
            () = closed.cancelled() => {
                run.cancel();
                return Err(ScopeLoadError::Cancelled);
            }
        };
        result.map_err(|error| match error {
            AuxiliaryError::Cancelled => ScopeLoadError::Cancelled,
            AuxiliaryError::Failed(error) => ScopeLoadError::Failed(error),
        })
    }

    async fn ai_review(
        &self,
        session: &str,
        label: &str,
        context: &diff::OverviewContext,
        shutdown: CancellationToken,
    ) -> Result<Vec<AiReviewComment>, ScopeLoadError> {
        let repository = repository_scope(context);
        let prompt = format!(
            "Delegate a comprehensive code review of `{label}` to a sub-agent and wait for its result. {repository} Inspect the actual diff, source, surrounding callers, tests, and relevant history without modifying the workspace. Find actionable bugs, regressions, and security or correctness failures introduced by this change. Verify each finding against the code and cite only lines present in the selected diff. Return only a JSON object with a `comments` array. Each comment must have `path` (repository-relative path), `side` (`additions` or `deletions`), `start_line` and `end_line` (positive line numbers on that side of the diff), `body` (beginning with a severity label `[P0]` for critical, `[P1]` for high, `[P2]` for medium, or `[P3]` for low, then a concise explanation of the failure, triggering condition, and consequence). Prefer the smallest relevant changed line range. If no actionable defects are found, return {{\"comments\":[]}}. Do not include speculative suggestions, a summary, Markdown fences, or prose outside the JSON object."
        );
        let result = self.run_agent(session, prompt, shutdown).await?;
        if result.len() > MAX_AI_REVIEW_BYTES {
            return Err(ScopeLoadError::Failed(
                "the agent returned a review larger than 1 MiB".into(),
            ));
        }
        let result = result.trim();
        let result = result
            .strip_prefix("```json")
            .and_then(|value| value.strip_suffix("```"))
            .map(str::trim)
            .unwrap_or(result);
        let review: AiReviewResult = serde_json::from_str(result).map_err(|error| {
            ScopeLoadError::Failed(format!("the agent returned invalid review JSON: {error}"))
        })?;
        Ok(review.comments)
    }

    async fn answer_question(
        &self,
        session: &str,
        label: &str,
        context: &diff::OverviewContext,
        question: &QuestionRequest,
        shutdown: CancellationToken,
    ) -> Result<String, ScopeLoadError> {
        let repository = repository_scope(context);
        let side = match question.side {
            CommentSide::Additions => "new",
            CommentSide::Deletions => "old",
        };
        let lines = if question.start_line == question.end_line {
            question.start_line.to_string()
        } else {
            format!("{}-{}", question.start_line, question.end_line)
        };
        let messages = serde_json::to_string(&question.messages)
            .map_err(|error| ScopeLoadError::Failed(error.to_string()))?;
        let prompt = format!(
            "Delegate this task to a sub-agent so the host agent does not absorb the investigation context. Ask the sub-agent to answer the reviewer's latest question about `{path}:{lines}` on the {side} side of `{label}`. {repository} It must inspect the repository, diff, history, selected lines, and surrounding code needed for an accurate answer without modifying the workspace. The complete conversation is JSON: {messages}. Return the sub-agent's answer as concise Markdown with direct `path:line` citations where useful. Return only the answer to the reviewer, with no preamble about delegation.",
            path = question.path,
        );
        let answer = self.run_agent(session, prompt, shutdown).await?;
        let answer = answer.trim();
        if answer.is_empty() {
            return Err(ScopeLoadError::Failed(
                "the agent returned an empty answer".to_owned(),
            ));
        }
        if answer.len() > MAX_QUESTION_ANSWER_BYTES {
            return Err(ScopeLoadError::Failed(
                "the agent returned an answer larger than 256 KiB".to_owned(),
            ));
        }
        Ok(answer.to_owned())
    }
}

fn overview_prompt(
    label: &str,
    context: &diff::OverviewContext,
    instructions: Option<&str>,
) -> String {
    let repository = repository_scope(context);
    let mut prompt = format!(
        r#"Delegate this task to a sub-agent so the host agent does not absorb the investigation context. Ask the sub-agent to quickly write a concise MDX explainer of `{label}` for a human reviewer. {repository} Inspect the diff, actual source files, relevant history, and surrounding code needed to understand the change without modifying the workspace. Explain what changed, why it matters, how the pieces fit together, and where a human reviewer should direct attention. Keep it brief and proportionate to the change. This is guidance for a human reviewer; do not perform a comprehensive defect audit or generate inline review findings. Whenever directing the reviewer's attention to code, cite direct `path:line` or `path:start-end` locations. Return only MDX source with Markdown headings, paragraphs, lists, tables, and fenced code where useful. Prefer the concise built-in components `<Callout title="..." tone="...">`, `<CardGrid>` with `<Card title="..." label="...">`, `<MetricGrid>` with `<Metric value="..." label="..." detail="..." />`, `<Process>` with `<ProcessStep title="...">`, and `<Figure caption="...">` for familiar layouts. You may define your own MDX components with `export function Name() {{ return <svg viewBox="0 0 400 120">...</svg> }}` and use `<Name />` for custom charts, diagrams, SVGs, and visual explanations when they clarify a change. Write self-contained JSX and calculations; do not import packages, fetch external resources, or use Markdown fences around the document. Charts must reflect actual repository facts, not invented measurements. The overview runs in an isolated frame with no network access. Keep all visualizations accessible and responsive."#,
    );
    prompt.push_str(" Use CSS theme variables such as `var(--ink)`, `var(--muted)`, `var(--paper-deep)`, `var(--rule)`, and `var(--accent)` in custom components and SVGs. Text and surfaces must remain legible in light and dark mode; avoid hard-coded black or white.");
    if let Some(instructions) = instructions {
        prompt.push_str("\n\nApply the reviewer's additional instructions to the emphasis and presentation of this overview. Keep it a concise explanation for a human reviewer, without turning it into a comprehensive defect audit:\n");
        prompt.push_str(instructions);
    }
    prompt
}

fn repository_scope(context: &diff::OverviewContext) -> String {
    let repository = context.repository.to_string_lossy();
    match &context.range {
        diff::OverviewRange::Commits { base, head } => format!(
            "Inspect the Git commit range `{base}..{head}` in the repository at `{repository}`."
        ),
        diff::OverviewRange::WorkingTree { base } => format!(
            "Inspect the changes from Git commit `{base}` through the working tree, including untracked files, in the repository at `{repository}`."
        ),
    }
}

fn strip_mdx_fence(value: &str) -> &str {
    value
        .strip_prefix("```mdx")
        .or_else(|| value.strip_prefix("```markdown"))
        .and_then(|value| value.strip_suffix("```"))
        .map(str::trim)
        .unwrap_or(value)
}

impl ReviewDecision {
    fn to_markdown(&self) -> String {
        let heading = match self.decision {
            Decision::Approve => "Approved",
            Decision::RequestChanges => "Changes requested",
        };
        let mut markdown = format!("## Review: {heading}\n\n**Scope:** {}\n", self.scope);
        if let Some(checkout) = &self.reviewed_in {
            markdown.push_str(&format!("**Checkout:** `{}`\n", checkout.display()));
        }
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
                CommentSide::Additions => "new",
                CommentSide::Deletions => "old",
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
enum ReviewError {
    #[error(transparent)]
    Diff(#[from] diff::DiffError),
    #[error("failed to validate the review workspace: {0}")]
    WorkspaceValidation(String),
    #[error("review preparation was cancelled")]
    Cancelled,
    #[error("the workspace kept changing while the review was being prepared; try again")]
    WorkspaceChanged,
}

#[cfg(test)]
mod tests {
    use crate::web::{
        bridge::{Busy, Publication},
        review::ReviewAgent,
        testing::{Harness, idle_agent, repository},
        wire::PROTOCOL_VERSION,
    };
    use axum::http::{Method, StatusCode};
    use serde_json::{Value, json};
    use std::{
        fs,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };
    use tokio::sync::Notify;

    fn full_range() -> Value {
        json!({"from": 0, "to": 2})
    }

    fn overview_request(session: &str, instructions: Option<&str>) -> Value {
        json!({
            "session": session,
            "generation": 0,
            "range": full_range(),
            "instructions": instructions,
        })
    }

    fn question_request(session: &str) -> Value {
        json!({
            "session": session,
            "thread_id": "thread-1",
            "operation_id": "question-1",
            "generation": 0,
            "range": full_range(),
            "path": "tracked.txt",
            "side": "additions",
            "start_line": 1,
            "end_line": 1,
            "messages": [{ "role": "reviewer", "body": "Why was this changed?" }]
        })
    }

    fn counting_agent(answer: &'static str) -> (ReviewAgent, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let agent: ReviewAgent = Arc::new({
            let calls = Arc::clone(&calls);
            move |_, _, _| {
                calls.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move { Ok(answer.to_owned()) })
            }
        });
        (agent, calls)
    }

    #[tokio::test]
    async fn the_review_is_prepared_lazily_and_matches_the_browser_protocol() {
        let harness = Harness::new();

        let (status, review) = harness.call(Method::GET, "/api/review", None).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(review["protocol_version"], PROTOCOL_VERSION);
        assert_eq!(review["generation"], 0);
        assert_eq!(review["turn_running"], false);
        assert!(
            review["page"]["patch"]
                .as_str()
                .unwrap()
                .contains("working.txt")
        );
        assert!(review["range_targets"].as_array().unwrap().len() >= 2);
        assert_eq!(review["overview"], Value::Null);
        assert_eq!(review["questions"], json!([]));
    }

    #[tokio::test]
    async fn other_review_routes_refuse_until_the_review_is_loaded() {
        let harness = Harness::new();

        let (status, body) = harness
            .call(
                Method::POST,
                "/api/range",
                Some(json!({"generation": 0, "range": full_range()})),
            )
            .await;

        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "stale_snapshot");
    }

    #[tokio::test]
    async fn a_directory_that_is_not_a_repository_is_reported_plainly() {
        let harness = Harness::with(tempfile::tempdir().unwrap(), idle_agent());

        let (status, body) = harness.call(Method::GET, "/api/review", None).await;

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["error"], "The folder must be a git repository.");
    }

    #[tokio::test]
    async fn refresh_replaces_the_snapshot_and_range_loads_use_the_new_generation() {
        let harness = Harness::new();
        harness.call(Method::GET, "/api/review", None).await;
        fs::write(
            harness.workspace.path().join("working.txt"),
            "changed again\n",
        )
        .unwrap();

        let (status, refreshed) = harness
            .call(Method::POST, "/api/refresh", Some(json!({"generation": 0})))
            .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(refreshed["generation"], 1);
        assert!(
            refreshed["page"]["patch"]
                .as_str()
                .unwrap()
                .contains("changed again")
        );
        let (status, stale) = harness
            .call(
                Method::POST,
                "/api/range",
                Some(json!({"generation": 0, "range": full_range()})),
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(stale["code"], "stale_snapshot");
    }

    #[tokio::test]
    async fn overviews_are_generated_on_demand_cached_and_scoped_to_their_session() {
        let (agent, calls) = counting_agent("<p>Overview</p>");
        let harness = Harness::with(repository(), agent);
        harness.open_session("s1").await;
        harness.open_session("s2").await;
        harness.call(Method::GET, "/api/review", None).await;
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        for _ in 0..2 {
            let (status, body) = harness
                .call(
                    Method::POST,
                    "/api/overview",
                    Some(overview_request("s1", None)),
                )
                .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body["overview_mdx"], "<p>Overview</p>");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let (_, own) = harness
            .call(Method::GET, "/api/review?session=s1", None)
            .await;
        assert_eq!(own["overview"]["status"], "ready");
        let (_, other) = harness
            .call(Method::GET, "/api/review?session=s2", None)
            .await;
        assert_eq!(other["overview"], Value::Null);
    }

    #[tokio::test]
    async fn overview_prompts_name_the_repository_range_and_instructions() {
        let prompts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let agent: ReviewAgent = Arc::new({
            let prompts = Arc::clone(&prompts);
            move |session, prompt, _| {
                prompts.lock().unwrap().push((session, prompt));
                Box::pin(async { Ok("```mdx\n## Overview\n```".to_owned()) })
            }
        });
        let harness = Harness::with(repository(), agent);
        harness.open_session("s1").await;
        harness.call(Method::GET, "/api/review", None).await;

        let (_, body) = harness
            .call(
                Method::POST,
                "/api/overview",
                Some(overview_request("s1", Some("Focus on the migration."))),
            )
            .await;

        assert_eq!(body["overview_mdx"], "## Overview");
        let prompts = prompts.lock().unwrap();
        let (session, prompt) = &prompts[0];
        assert_eq!(session, "s1");
        assert!(prompt.contains("concise MDX explainer"));
        assert!(prompt.contains(&harness.workspace.path().to_string_lossy().into_owned()));
        assert!(prompt.contains("Focus on the migration."));
    }

    #[tokio::test]
    async fn a_running_session_blocks_review_actions() {
        let harness = Harness::new();
        harness.open_session("s1").await;
        harness.call(Method::GET, "/api/review", None).await;
        harness.terminal.publisher.publish(Publication::Busy {
            session: "s1".into(),
            busy: Busy {
                turns: 1,
                shells: 0,
            },
        });
        while !harness.hub.any_busy() {
            tokio::task::yield_now().await;
        }

        for (route, request) in [
            ("/api/overview", overview_request("s1", None)),
            (
                "/api/ai-review",
                json!({"session": "s1", "generation": 0, "range": full_range()}),
            ),
            ("/api/question", question_request("s1")),
        ] {
            let (status, body) = harness.call(Method::POST, route, Some(request)).await;
            assert_eq!(status, StatusCode::CONFLICT, "{route}");
            assert_eq!(body["code"], "turn_running", "{route}");
        }
        let (_, review) = harness.call(Method::GET, "/api/review", None).await;
        assert_eq!(review["turn_running"], true);
    }

    #[tokio::test]
    async fn closing_a_session_cancels_its_running_overview() {
        let started = Arc::new(Notify::new());
        let agent: ReviewAgent = Arc::new({
            let started = Arc::clone(&started);
            move |_, _, shutdown| {
                let started = Arc::clone(&started);
                Box::pin(async move {
                    started.notify_one();
                    shutdown.cancelled().await;
                    Err(crate::web::bridge::AuxiliaryError::Cancelled)
                })
            }
        });
        let harness = Arc::new(Harness::with(repository(), agent));
        harness.open_session("s1").await;
        harness.call(Method::GET, "/api/review", None).await;
        let request = tokio::spawn({
            let harness = Arc::clone(&harness);
            async move {
                harness
                    .call(
                        Method::POST,
                        "/api/overview",
                        Some(overview_request("s1", None)),
                    )
                    .await
            }
        });
        started.notified().await;

        harness.terminal.publisher.publish(Publication::Closed {
            session: "s1".into(),
        });

        let (status, body) = tokio::time::timeout(Duration::from_secs(10), request)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "session_cancelled");
    }

    #[tokio::test]
    async fn question_threads_belong_to_their_session() {
        let (agent, _) = counting_agent("It supports the feature.");
        let harness = Harness::with(repository(), agent);
        harness.open_session("s1").await;
        harness.call(Method::GET, "/api/review", None).await;

        let (status, answer) = harness
            .call(Method::POST, "/api/question", Some(question_request("s1")))
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(answer["answer"], "It supports the feature.");

        let list = |session: &str| {
            harness.call(
                Method::POST,
                "/api/questions",
                Some(json!({"generation": 0, "session": session})),
            )
        };
        let (_, own) = list("s1").await;
        assert_eq!(own["questions"][0]["thread_id"], "thread-1");
        assert_eq!(own["questions"][0]["status"], "idle");
        let (_, other) = list("s2").await;
        assert_eq!(other["questions"], json!([]));
    }

    #[tokio::test]
    async fn questions_must_anchor_to_the_reviewed_patch() {
        let harness = Harness::new();
        harness.open_session("s1").await;
        harness.call(Method::GET, "/api/review", None).await;
        let mut request = question_request("s1");
        request["path"] = json!("missing.txt");

        let (status, body) = harness
            .call(Method::POST, "/api/question", Some(request))
            .await;

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["code"], "invalid_thread");
    }

    #[tokio::test]
    async fn compose_renders_the_canonical_markdown_for_anchored_comments() {
        let harness = Harness::new();
        harness.call(Method::GET, "/api/review", None).await;
        let decision = |comments: Value, generation: u64| {
            json!({
                "generation": generation,
                "range": full_range(),
                "decision": "request_changes",
                "summary": "Please address this.",
                "comments": comments,
            })
        };
        let comment = json!({
            "path": "tracked.txt", "side": "additions", "start_line": 1, "end_line": 1,
            "body": "Handle the error.\nThis can fail."
        });

        let (status, body) = harness
            .call(
                Method::POST,
                "/api/review/compose",
                Some(decision(json!([comment]), 0)),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        let markdown = body["markdown"].as_str().unwrap();
        assert!(markdown.starts_with("## Review: Changes requested\n\n**Scope:** "));
        assert!(
            markdown.contains("\n- `tracked.txt:1` (new)\n  Handle the error.\n  This can fail.\n")
        );

        let unanchored = json!({
            "path": "tracked.txt", "side": "additions", "start_line": 99, "end_line": 99, "body": "x"
        });
        let (status, body) = harness
            .call(
                Method::POST,
                "/api/review/compose",
                Some(decision(json!([unanchored]), 0)),
            )
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["code"], "invalid_comment_anchor");

        let (status, body) = harness
            .call(
                Method::POST,
                "/api/review/compose",
                Some(decision(json!([]), 7)),
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "stale_snapshot");
    }

    #[tokio::test]
    async fn the_workspace_watcher_reports_changes_only_to_connected_streams() {
        let harness = Harness::new();
        let mut stream = harness.hub.subscribe().unwrap();
        // Opening the review starts watching its checkout.
        harness.call(Method::GET, "/api/review", None).await;

        let mut seen = false;
        'attempts: for attempt in 0..20 {
            fs::write(
                harness.workspace.path().join("working.txt"),
                format!("edit {attempt}\n"),
            )
            .unwrap();
            let deadline = tokio::time::Instant::now() + Duration::from_millis(1200);
            while let Ok(Some(frame)) =
                tokio::time::timeout_at(deadline, stream.frames.recv()).await
            {
                if frame.starts_with("event: workspace\n") {
                    seen = true;
                    break 'attempts;
                }
            }
        }
        harness.shutdown.cancel();
        assert!(seen, "an edit should produce a workspace event");
    }

    #[tokio::test]
    async fn review_actions_for_a_session_that_is_not_live_are_unknown_session() {
        let harness = Harness::new();
        harness.call(Method::GET, "/api/review", None).await;

        for (route, request) in [
            ("/api/overview", overview_request("ghost", None)),
            ("/api/question", question_request("ghost")),
        ] {
            let (status, body) = harness.call(Method::POST, route, Some(request)).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{route}");
            assert_eq!(body["code"], "unknown_session", "{route}");
        }
        let (status, _) = harness
            .call(
                Method::POST,
                "/api/question/cancel",
                Some(json!({
                    "session": "ghost", "operation_id": "q", "generation": 0, "range": full_range()
                })),
            )
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn a_checkout_of_the_repository_is_reviewed_on_its_own() {
        let harness = Harness::new();
        harness.open_session("s1").await;
        let (_directory, worktree) = crate::web::testing::worktree(harness.workspace.path());
        std::fs::write(worktree.join("only-here.txt"), "elsewhere\n").unwrap();
        let uri = format!("/api/review?session=s1&checkout={}", worktree.display());

        let (status, other) = harness.call(Method::GET, &uri, None).await;
        let (_, own) = harness
            .call(Method::GET, "/api/review?session=s1", None)
            .await;

        assert_eq!(status, StatusCode::OK, "{other}");
        assert_eq!(other["checkout"]["path"], worktree.to_str().unwrap());
        assert_eq!(other["checkout"]["label"], "elsewhere");
        assert!(
            other["page"]["patch"]
                .as_str()
                .unwrap()
                .contains("only-here.txt")
        );
        assert!(
            !own["page"]["patch"]
                .as_str()
                .unwrap()
                .contains("only-here.txt")
        );
        assert!(
            own["page"]["patch"]
                .as_str()
                .unwrap()
                .contains("working.txt")
        );
        assert_ne!(own["checkout"]["path"], other["checkout"]["path"]);
    }

    #[tokio::test]
    async fn a_directory_outside_the_repository_cannot_be_reviewed() {
        let harness = Harness::new();
        harness.open_session("s1").await;
        let stranger = tempfile::tempdir().unwrap();
        let uri = format!(
            "/api/review?session=s1&checkout={}",
            stranger.path().display()
        );

        let (status, body) = harness.call(Method::GET, &uri, None).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "invalid_checkout");
    }

    #[tokio::test]
    async fn a_review_of_another_checkout_names_it_when_composed() {
        let harness = Harness::new();
        harness.open_session("s1").await;
        let (_directory, worktree) = crate::web::testing::worktree(harness.workspace.path());
        let uri = format!("/api/review?session=s1&checkout={}", worktree.display());
        harness.call(Method::GET, &uri, None).await;
        let request = |checkout: Option<&std::path::Path>| {
            let mut request = json!({
                "session": "s1",
                "generation": 0,
                "range": full_range(),
                "decision": "approve",
            });
            if let Some(checkout) = checkout {
                request["checkout"] = json!(checkout);
            }
            request
        };

        let (status, other) = harness
            .call(
                Method::POST,
                "/api/review/compose",
                Some(request(Some(&worktree))),
            )
            .await;
        harness
            .call(Method::GET, "/api/review?session=s1", None)
            .await;
        let (_, own) = harness
            .call(Method::POST, "/api/review/compose", Some(request(None)))
            .await;

        assert_eq!(status, StatusCode::OK, "{other}");
        assert!(
            other["markdown"]
                .as_str()
                .unwrap()
                .contains(&format!("**Checkout:** `{}`", worktree.display())),
            "{other}"
        );
        assert!(
            !own["markdown"].as_str().unwrap().contains("Checkout:"),
            "{own}"
        );
    }
}
