//! The review state of each checkout and the operations that read and change it.
//!
//! One review exists per checkout. It is prepared lazily by the first request and replaced when a
//! browser refreshes it; the replacement bumps a generation that stales every request that still
//! refers to the old snapshot and cancels the work started for it. Overviews, AI reviews, and
//! question threads belong to a live session and run on that session's worker as clean-context
//! prompts, so they stop when the session closes.
//!
//! Locks are taken in this order: `ReviewState::preparing`, then `ReviewState::session`, then
//! `ReviewState::overview_operations`. `ReviewState::active_questions` is a synchronous lock that
//! is only held for map updates and never across an `await`.

use super::{
    agent::ReviewAgent,
    backend::{OperationError, PreparedReview, ReviewBackend, ReviewError},
    error::ReviewApiError,
    model::{
        AiReviewComment, OverviewStatus, QuestionRequest, QuestionStatus, ReviewBootstrap,
        ReviewPage, StoredOverview, StoredQuestion, ThreadMessage, ThreadRole,
    },
    prompts::OverviewPrompt,
    validation::{MAX_COMMENTS, is_anchored},
};
use crate::web::{
    hub::Hub,
    workspaces::{Target, Workspaces},
};
use serde::Serialize;
use std::{
    collections::{HashMap, VecDeque, hash_map::Entry},
    hash::Hash,
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};
use tact_vcs::{ReviewContext, ReviewRange, WorkspaceVersion};
use tokio::sync::{MappedMutexGuard, Mutex, MutexGuard, oneshot};
use tokio_util::sync::CancellationToken;

const MAX_QUESTION_THREADS: usize = 256;
const MAX_CACHED_PAGES: usize = 8;
const MAX_CACHED_PAGE_BYTES: usize = 64 * 1024 * 1024;
const MAX_CACHED_OVERVIEWS: usize = 8;
const MAX_CACHED_OVERVIEW_BYTES: usize = 8 * 1024 * 1024;
/// The most checkouts whose review is kept at once. A checkout reviewed longer ago is dropped, with
/// its cached pages and watcher, and prepared again when it is next opened.
const MAX_REVIEWED_CHECKOUTS: usize = 4;
const WORKSPACE_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// One session's overview of one range, shaped by optional instructions.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct OverviewKey {
    pub(super) range: ReviewRange,
    pub(super) instructions: Option<String>,
    pub(super) session: String,
}

/// One generation of an overview. Requests for the same key share a single agent run.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct OverviewOperationKey {
    pub(super) generation: u64,
    pub(super) overview: OverviewKey,
}

/// Why an agent operation produced no result. Shared by every request waiting on the operation.
#[derive(Clone)]
pub(super) enum OperationFailure {
    Cancelled,
    /// The review or the checkout changed underneath the operation.
    Stale(&'static str),
    /// The checkout's version could not be read.
    Workspace(String),
    /// The agent failed or replied with something unusable.
    Agent(String),
}

type OperationResult = Result<String, OperationFailure>;

struct OverviewOperation {
    /// Held by the request that runs the operation; joined requests wait on it for the result.
    gate: Arc<Mutex<()>>,
    result: Mutex<Option<OperationResult>>,
}

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
pub(in crate::web) struct ReviewRegistry {
    workspaces: Arc<Workspaces>,
    pub(super) hub: Hub,
    agent: Arc<dyn ReviewAgent>,
    shutdown: CancellationToken,
    reviewed: Mutex<Vec<ReviewedCheckout>>,
}

impl ReviewRegistry {
    pub(in crate::web) fn new(
        workspaces: Arc<Workspaces>,
        hub: Hub,
        agent: Arc<dyn ReviewAgent>,
        shutdown: CancellationToken,
    ) -> Arc<Self> {
        Arc::new(Self {
            workspaces,
            hub,
            agent,
            shutdown,
            reviewed: Mutex::new(Vec::new()),
        })
    }

    /// The review for the checkout a request names, with the checkout it resolved to.
    pub(super) async fn resolve(
        &self,
        session: Option<&str>,
        checkout: Option<&str>,
    ) -> Result<(Arc<ReviewState>, Target), ReviewApiError> {
        let target = self.workspaces.resolve(session, checkout).await?;
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
            Arc::clone(&self.agent),
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
    pub(super) async fn all(&self) -> Vec<Arc<ReviewState>> {
        self.reviewed
            .lock()
            .await
            .iter()
            .map(|kept| Arc::clone(&kept.state))
            .collect()
    }
}

/// Tells connected browsers when the working tree changes so they can mark their diff stale.
///
/// The workspace is only inspected while a stream is connected. Failed inspections are skipped:
/// the next poll retries, and a transient git error must not look like a change.
async fn watch_workspace(state: Arc<ReviewState>, shutdown: CancellationToken) {
    #[derive(Serialize)]
    struct WorkspaceEvent<'a> {
        version: String,
        /// The checkout whose files changed, so a client reviewing another one can ignore it.
        checkout: &'a Path,
    }

    let mut seen: Option<WorkspaceVersion> = None;
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

/// The review of one checkout.
pub(super) struct ReviewState {
    /// `None` until the first request prepares the checkout's review.
    session: Mutex<Option<ReviewSession>>,
    backend: ReviewBackend,
    pub(super) hub: Hub,
    overview_operations: Mutex<HashMap<OverviewOperationKey, Arc<OverviewOperation>>>,
    active_questions: StdMutex<HashMap<String, ActiveQuestion>>,
    /// Serializes preparation and refresh so concurrent requests capture the checkout once.
    preparing: Mutex<()>,
    shutdown: CancellationToken,
}

struct ActiveQuestion {
    generation: u64,
    range: ReviewRange,
    cancellation: CancellationToken,
}

/// Removes a question from the active set when its task ends, however it ends.
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

/// A prepared review: the snapshot of one generation and everything derived from it.
pub(super) struct ReviewSession {
    pub(super) generation: u64,
    pub(super) bootstrap: ReviewBootstrap,
    default_page: ReviewPage,
    pub(super) selected_page: ReviewPage,
    context: ReviewContext,
    range_pages: BoundedCache<ReviewRange, ReviewPage>,
    overviews: BoundedCache<OverviewKey, String>,
    /// Per session: the overview currently being generated and the one the browser has selected.
    active_overview: HashMap<String, OverviewOperationKey>,
    selected_overview_key: HashMap<String, OverviewKey>,
    questions: Vec<StoredQuestion>,
    version: WorkspaceVersion,
    /// Cancelled when this generation is replaced.
    generation_shutdown: CancellationToken,
    session_shutdown: CancellationToken,
}

/// A least-recently-used cache bounded by entry count and total size.
struct BoundedCache<K, V> {
    values: HashMap<K, (V, usize)>,
    order: VecDeque<K>,
    bytes: usize,
    max_entries: usize,
    max_bytes: usize,
}

impl<K, V> BoundedCache<K, V>
where
    K: Clone + Eq + Hash,
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

    /// Inserts a value, evicting the least recently used ones to stay within bounds. A value
    /// larger than the whole cache is not kept.
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

    /// Replaces the snapshot with a new generation, cancelling the work of the old one.
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

    pub(super) fn page(&self, range: &ReviewRange) -> Option<&ReviewPage> {
        if range == &self.bootstrap.default_range {
            return Some(&self.default_page);
        }
        self.range_pages.get(range)
    }

    /// The page for `range` if `generation` is current.
    pub(super) fn matching_page(
        &self,
        generation: u64,
        range: &ReviewRange,
    ) -> Option<&ReviewPage> {
        if generation != self.generation {
            return None;
        }
        self.page(range)
    }

    fn insert_page(&mut self, page: ReviewPage) {
        let bytes = page.diff.patch.len();
        self.range_pages.insert(page.selected_range, page, bytes);
    }

    pub(super) fn questions_for(&self, session: &str) -> Vec<StoredQuestion> {
        self.questions
            .iter()
            .filter(|question| question.session == session)
            .cloned()
            .collect()
    }

    fn finish_overview(&mut self, key: &OverviewOperationKey) {
        let session = &key.overview.session;
        if self.active_overview.get(session) == Some(key) {
            self.active_overview.remove(session);
        }
    }

    fn select_overview(&mut self, key: &OverviewKey) {
        self.selected_overview_key
            .insert(key.session.clone(), key.clone());
    }

    fn start_overview(&mut self, key: &OverviewOperationKey) {
        self.active_overview
            .insert(key.overview.session.clone(), key.clone());
        self.select_overview(&key.overview);
    }

    /// The overview `session` sees for the selected range: the one it selected, if it is being
    /// generated or is cached, otherwise its most recent cached overview of the range.
    pub(super) fn selected_overview(&self, session: &str) -> Option<StoredOverview> {
        let range = self.selected_page.selected_range;
        let selected = self.selected_overview_key.get(session);
        if let Some(active) = self.active_overview.get(session)
            && active.overview.range == range
            && selected.is_some_and(|key| {
                key.range == range && key.instructions == active.overview.instructions
            })
        {
            return Some(StoredOverview {
                selected_range: range,
                status: OverviewStatus::Generating,
                overview_mdx: None,
                instructions: active.overview.instructions.clone(),
            });
        }
        let key = selected
            .filter(|key| key.range == range && self.overviews.get(key).is_some())
            .or_else(|| {
                self.overviews
                    .order
                    .iter()
                    .rev()
                    .find(|key| key.range == range && key.session == session)
            })?;
        self.overviews.get(key).map(|overview_mdx| StoredOverview {
            selected_range: range,
            status: OverviewStatus::Ready,
            overview_mdx: Some(overview_mdx.clone()),
            instructions: key.instructions.clone(),
        })
    }

    /// Records that a question is being asked. Fails unless the request starts a new thread or
    /// continues its stored thread: the same anchor, and either a retry of a failed question or
    /// exactly one new reviewer message after an answer.
    fn begin_question(&mut self, request: &QuestionRequest) -> bool {
        if self
            .matching_page(request.generation, &request.range)
            .is_none()
        {
            return false;
        }
        if let Some(thread) = self.questions.iter_mut().find(|thread| {
            thread.thread_id == request.thread_id && thread.session == request.session
        }) {
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
                && request.messages.last().map(|message| message.role)
                    == Some(ThreadRole::Reviewer);
            if !retries_last_question && !adds_follow_up {
                return false;
            }
            thread.operation_id.clone_from(&request.operation_id);
            thread.messages.clone_from(&request.messages);
            thread.status = QuestionStatus::Asking;
            thread.error = None;
            return true;
        }
        if self.questions.len() >= MAX_QUESTION_THREADS {
            return false;
        }
        self.questions.push(StoredQuestion {
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

    /// Records how a question ended on its stored thread.
    fn finish_question(&mut self, request: &QuestionRequest, result: &OperationResult) {
        let Some(thread) = self.questions.iter_mut().find(|thread| {
            thread.thread_id == request.thread_id
                && thread.operation_id == request.operation_id
                && thread.generation == request.generation
                && thread.range == request.range
        }) else {
            return;
        };
        let (status, error) = match result {
            Ok(answer) => {
                thread.messages.push(ThreadMessage {
                    role: ThreadRole::Agent,
                    body: answer.clone(),
                });
                (QuestionStatus::Idle, None)
            }
            Err(OperationFailure::Cancelled) => (QuestionStatus::Cancelled, None),
            Err(OperationFailure::Stale(message)) => {
                (QuestionStatus::Error, Some((*message).to_owned()))
            }
            Err(OperationFailure::Workspace(error) | OperationFailure::Agent(error)) => {
                (QuestionStatus::Error, Some(error.clone()))
            }
        };
        thread.status = status;
        thread.error = error;
    }
}

impl ReviewState {
    fn new(
        target: Target,
        hub: Hub,
        agent: Arc<dyn ReviewAgent>,
        shutdown: CancellationToken,
    ) -> Arc<Self> {
        Arc::new(Self {
            session: Mutex::new(None),
            backend: ReviewBackend::new(target, hub.clone(), agent),
            hub,
            overview_operations: Mutex::new(HashMap::new()),
            active_questions: StdMutex::new(HashMap::new()),
            preparing: Mutex::new(()),
            shutdown,
        })
    }

    pub(super) fn turn_running(&self) -> bool {
        self.hub.any_busy()
    }

    /// The prepared review. Fails with a stale snapshot if none has been loaded yet.
    pub(super) async fn session(
        &self,
    ) -> Result<MappedMutexGuard<'_, ReviewSession>, ReviewApiError> {
        MutexGuard::try_map(self.session.lock().await, Option::as_mut)
            .map_err(|_| ReviewApiError::StaleSnapshot("the review has not been loaded"))
    }

    /// Prepares the checkout's review on first use.
    pub(super) async fn ensure_loaded(&self) -> Result<(), ReviewApiError> {
        let _preparing = self.preparing.lock().await;
        if self.session.lock().await.is_some() {
            return Ok(());
        }
        let review = self.backend.prepare(self.shutdown.child_token()).await?;
        *self.session.lock().await = Some(ReviewSession::new(review, self.shutdown.clone()));
        Ok(())
    }

    /// Captures the checkout again as the next generation, if `generation` is still current.
    pub(super) async fn refresh(
        &self,
        generation: u64,
    ) -> Result<MappedMutexGuard<'_, ReviewSession>, ReviewApiError> {
        let _preparing = self.preparing.lock().await;
        let shutdown = {
            let session = self.session().await?;
            if generation != session.generation {
                return Err(ReviewApiError::StaleSnapshot(
                    "the review generation is stale",
                ));
            }
            session.generation_shutdown.clone()
        };
        let review = self.backend.prepare(shutdown).await?;
        let mut session = self.session().await?;
        if generation != session.generation {
            return Err(ReviewApiError::StaleSnapshot(
                "the review changed while refresh was loading",
            ));
        }
        session.replace(review);
        self.overview_operations.lock().await.clear();
        Ok(session)
    }

    /// The page for `range`, captured on first request and validated against the checkout's
    /// version before and after the capture.
    pub(super) async fn load_page(
        &self,
        generation: u64,
        range: ReviewRange,
    ) -> Result<ReviewPage, ReviewApiError> {
        let (context, version, shutdown) = {
            let mut session = self.session().await?;
            if generation != session.generation {
                return Err(ReviewApiError::StaleSnapshot(
                    "the review generation is stale",
                ));
            }
            if let Some(page) = session.page(&range).cloned() {
                session.selected_page = page.clone();
                return Ok(page);
            }
            (
                session.context.clone(),
                session.version.clone(),
                session.generation_shutdown.clone(),
            )
        };

        match self.backend.current_version(shutdown.clone()).await {
            Ok(current) if current == version => {}
            Ok(_) => {
                return Err(ReviewApiError::StaleSnapshot(
                    "the workspace changed before this range was loaded",
                ));
            }
            Err(OperationError::Cancelled) => {
                return Err(ReviewApiError::OperationCancelled(
                    "range validation was cancelled",
                ));
            }
            Err(OperationError::Failed(error)) => {
                return Err(ReviewApiError::WorkspaceUnreadable(error));
            }
        }
        let page = self
            .backend
            .prepare_page(context, range, shutdown.clone())
            .await
            .map_err(|error| match error {
                ReviewError::Cancelled => {
                    ReviewApiError::OperationCancelled("range loading was cancelled")
                }
                error => ReviewApiError::InvalidRange(error.to_string()),
            })?;
        let changed = "the workspace changed while this range was loading";
        match self.backend.current_version(shutdown).await {
            Ok(current) if current == version => {}
            Ok(_) => return Err(ReviewApiError::StaleSnapshot(changed)),
            Err(OperationError::Cancelled) => {
                return Err(ReviewApiError::StaleSnapshot(
                    "the review changed while this range was loading",
                ));
            }
            Err(OperationError::Failed(error)) => {
                return Err(ReviewApiError::WorkspaceUnreadable(error));
            }
        }
        let mut session = self.session().await?;
        if session.generation != generation {
            return Err(ReviewApiError::StaleSnapshot(
                "the review changed while this range was loading",
            ));
        }
        let page = ReviewPage { generation, ..page };
        session.insert_page(page.clone());
        session.selected_page = page.clone();
        Ok(page)
    }

    /// The error for an operation that was cancelled: distinct when its session closed.
    fn cancellation(&self, session: &str, message: &'static str) -> ReviewApiError {
        if self.hub.is_live(session) {
            ReviewApiError::OperationCancelled(message)
        } else {
            ReviewApiError::SessionCancelled(message)
        }
    }

    fn operation_error(
        &self,
        failure: OperationFailure,
        session: &str,
        cancelled: &'static str,
        agent_failed: fn(String) -> ReviewApiError,
    ) -> ReviewApiError {
        match failure {
            OperationFailure::Cancelled => self.cancellation(session, cancelled),
            OperationFailure::Stale(message) => ReviewApiError::StaleSnapshot(message),
            OperationFailure::Workspace(error) => ReviewApiError::WorkspaceUnreadable(error),
            OperationFailure::Agent(error) => agent_failed(error),
        }
    }

    /// Generates the overview `key` names, or joins the request already generating it.
    ///
    /// The generation runs in its own task, so a browser that disconnects does not cancel it; a
    /// reloaded browser that asks again joins the same operation and receives its result.
    pub(super) async fn overview(
        self: &Arc<Self>,
        key: OverviewOperationKey,
    ) -> Result<String, ReviewApiError> {
        let (operation, joined) = {
            let mut operations = self.overview_operations.lock().await;
            match operations.entry(key.clone()) {
                Entry::Occupied(entry) => (Arc::clone(entry.get()), true),
                Entry::Vacant(entry) => (
                    Arc::clone(entry.insert(Arc::new(OverviewOperation {
                        gate: Arc::new(Mutex::new(())),
                        result: Mutex::new(None),
                    }))),
                    false,
                ),
            }
        };
        if joined {
            let mut session = self.session().await?;
            if self.turn_running() {
                return Err(ReviewApiError::TurnRunning);
            }
            if session
                .matching_page(key.generation, &key.overview.range)
                .is_none()
            {
                return Err(ReviewApiError::StaleSnapshot(
                    "the requested review snapshot is stale",
                ));
            }
            session.start_overview(&key);
        }
        let operation_gate = Arc::clone(&operation.gate).lock_owned().await;
        if let Some(result) = operation.result.lock().await.clone() {
            self.session().await?.finish_overview(&key);
            self.overview_operations.lock().await.remove(&key);
            return result.map_err(|failure| self.overview_error(failure, &key));
        }
        let prepared = {
            let mut session = self.session().await?;
            if self.turn_running() {
                Err(ReviewApiError::TurnRunning)
            } else if let Some(page) = session
                .matching_page(key.generation, &key.overview.range)
                .cloned()
            {
                if let Some(overview_mdx) = session.overviews.touch(&key.overview).cloned() {
                    session.select_overview(&key.overview);
                    Ok(Err(overview_mdx))
                } else {
                    Ok(Ok((
                        page,
                        session.version.clone(),
                        session.generation_shutdown.clone(),
                    )))
                }
            } else {
                Err(ReviewApiError::StaleSnapshot(
                    "the requested review snapshot is stale",
                ))
            }
        };
        let (page, version, shutdown) = match prepared {
            Ok(Ok(prepared)) => prepared,
            Ok(Err(cached)) => {
                self.discard_idle_overview_operation(&key, &operation).await;
                return Ok(cached);
            }
            Err(error) => {
                self.discard_idle_overview_operation(&key, &operation).await;
                return Err(error);
            }
        };
        {
            let mut session = self.session().await?;
            if session
                .matching_page(key.generation, &key.overview.range)
                .is_none()
            {
                drop(session);
                self.discard_idle_overview_operation(&key, &operation).await;
                return Err(ReviewApiError::StaleSnapshot(
                    "the requested review snapshot is stale",
                ));
            }
            session.start_overview(&key);
        }

        let (completion, response) = oneshot::channel();
        let state = Arc::clone(self);
        tokio::spawn(async move {
            let _operation_gate = operation_gate;
            let result = state.run_overview(&key, &page, version, shutdown).await;
            let result = state.store_overview_result(&key, result).await;
            *operation.result.lock().await = Some(result.clone());
            let initiating_browser_is_connected = completion
                .send(result.map_err(|failure| state.overview_error(failure, &key)))
                .is_ok();
            let reloaded_browser_is_waiting = Arc::strong_count(&operation) > 2;
            if initiating_browser_is_connected || !reloaded_browser_is_waiting {
                state.overview_operations.lock().await.remove(&key);
            }
        });

        response.await.unwrap_or(Err(ReviewApiError::Internal(
            "the overview operation stopped unexpectedly",
        )))
    }

    fn overview_error(
        &self,
        failure: OperationFailure,
        key: &OverviewOperationKey,
    ) -> ReviewApiError {
        self.operation_error(
            failure,
            &key.overview.session,
            "overview generation was cancelled",
            ReviewApiError::OverviewFailed,
        )
    }

    /// Forgets an operation that nobody else joined and that will not run.
    async fn discard_idle_overview_operation(
        &self,
        key: &OverviewOperationKey,
        operation: &Arc<OverviewOperation>,
    ) {
        let mut operations = self.overview_operations.lock().await;
        if Arc::strong_count(operation) == 2
            && operations
                .get(key)
                .is_some_and(|current| Arc::ptr_eq(current, operation))
        {
            operations.remove(key);
        }
    }

    async fn run_overview(
        &self,
        key: &OverviewOperationKey,
        page: &ReviewPage,
        version: WorkspaceVersion,
        shutdown: CancellationToken,
    ) -> OperationResult {
        self.check_version(
            &version,
            shutdown.clone(),
            OperationFailure::Stale("the review changed before its overview was generated"),
            "the workspace changed before its overview was generated",
        )
        .await?;
        let prompt = OverviewPrompt {
            label: &page.diff.scope,
            context: &page.diff.overview,
            instructions: key.overview.instructions.as_deref(),
        };
        let overview_mdx = self
            .backend
            .overview(&key.overview.session, prompt, shutdown.clone())
            .await
            .map_err(|error| match error {
                OperationError::Cancelled => OperationFailure::Cancelled,
                OperationError::Failed(error) => OperationFailure::Agent(error),
            })?;
        self.check_version(
            &version,
            shutdown,
            OperationFailure::Stale("the workspace changed while its overview was generated"),
            "the workspace changed while its overview was generated",
        )
        .await?;
        Ok(overview_mdx)
    }

    /// Fails unless the checkout still has `version`: with `changed` when it differs, and with
    /// `cancelled` when the check is cancelled.
    async fn check_version(
        &self,
        version: &WorkspaceVersion,
        shutdown: CancellationToken,
        cancelled: OperationFailure,
        changed: &'static str,
    ) -> Result<(), OperationFailure> {
        match self.backend.current_version(shutdown).await {
            Ok(current) if current == *version => Ok(()),
            Ok(_) => Err(OperationFailure::Stale(changed)),
            Err(OperationError::Cancelled) => Err(cancelled),
            Err(OperationError::Failed(error)) => Err(OperationFailure::Workspace(error)),
        }
    }

    async fn store_overview_result(
        &self,
        key: &OverviewOperationKey,
        result: OperationResult,
    ) -> OperationResult {
        let Ok(mut session) = self.session().await else {
            return Err(OperationFailure::Stale("the review is no longer loaded"));
        };
        session.finish_overview(key);
        if self.turn_running() {
            return Err(OperationFailure::Stale(
                "the agent started another turn during overview generation",
            ));
        }
        if session
            .matching_page(key.generation, &key.overview.range)
            .is_none()
        {
            return Err(OperationFailure::Stale(
                "the review changed while its overview was loading",
            ));
        }
        if let Ok(overview_mdx) = &result {
            session.overviews.insert(
                key.overview.clone(),
                overview_mdx.clone(),
                overview_mdx.len(),
            );
        }
        result
    }

    /// Runs an AI review of a page and returns its anchored comments.
    pub(super) async fn ai_review(
        &self,
        session_id: &str,
        generation: u64,
        range: ReviewRange,
    ) -> Result<Vec<AiReviewComment>, ReviewApiError> {
        let (page, version, shutdown) = {
            let session = self.session().await?;
            let Some(page) = session.matching_page(generation, &range) else {
                return Err(ReviewApiError::StaleSnapshot(
                    "the requested review snapshot is stale",
                ));
            };
            (
                page.clone(),
                session.version.clone(),
                session.generation_shutdown.clone(),
            )
        };
        let as_error = |failure| {
            self.operation_error(
                failure,
                session_id,
                "AI review was cancelled",
                ReviewApiError::AiReviewFailed,
            )
        };
        self.check_version(
            &version,
            shutdown.clone(),
            OperationFailure::Stale("the review changed before AI review started"),
            "the workspace changed before AI review started",
        )
        .await
        .map_err(as_error)?;
        let comments = self
            .backend
            .ai_review(
                session_id,
                &page.diff.scope,
                &page.diff.overview,
                shutdown.clone(),
            )
            .await
            .map_err(|error| match error {
                OperationError::Cancelled => as_error(OperationFailure::Cancelled),
                OperationError::Failed(error) => ReviewApiError::AiReviewFailed(error),
            })?;
        self.check_version(
            &version,
            shutdown,
            OperationFailure::Stale("the review changed during AI review"),
            "the workspace changed during AI review",
        )
        .await
        .map_err(as_error)?;
        let session = self.session().await?;
        if self.turn_running() {
            return Err(ReviewApiError::TurnRunning);
        }
        if session.matching_page(generation, &range).is_none() {
            return Err(ReviewApiError::StaleSnapshot(
                "the review changed during AI review",
            ));
        }
        if comments.len() > MAX_COMMENTS
            || comments
                .iter()
                .any(|comment| !comment.is_well_formed() || !comment.is_anchored_in(&page.diff))
        {
            return Err(ReviewApiError::AiReviewFailed(
                "the agent returned invalid or unanchored review comments".to_owned(),
            ));
        }
        Ok(comments)
    }

    /// Answers a question thread's latest message and records the answer on the thread.
    pub(super) async fn answer(
        self: &Arc<Self>,
        request: QuestionRequest,
    ) -> Result<String, ReviewApiError> {
        let (page, version, shutdown) = {
            let session = self.session().await?;
            let Some(page) = session.matching_page(request.generation, &request.range) else {
                return Err(ReviewApiError::StaleSnapshot(
                    "the question's review snapshot is stale",
                ));
            };
            if !is_anchored(
                &page.diff,
                &request.path,
                request.side,
                request.start_line,
                request.end_line,
            ) {
                return Err(ReviewApiError::InvalidThread(
                    "the question is not anchored to the reviewed patch",
                ));
            }
            (
                page.clone(),
                session.version.clone(),
                session.generation_shutdown.clone(),
            )
        };
        let operation_shutdown = {
            let mut session = self.session().await?;
            if self.turn_running() {
                return Err(ReviewApiError::TurnRunning);
            }
            let Ok(mut active) = self.active_questions.lock() else {
                return Err(ReviewApiError::Internal(
                    "the active question state is unavailable",
                ));
            };
            if active.contains_key(&request.operation_id) {
                return Err(ReviewApiError::InvalidThread(
                    "the question operation identifier is already in use",
                ));
            }
            if !session.begin_question(&request) {
                return Err(ReviewApiError::InvalidThread(
                    "the question does not continue its stored thread",
                ));
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
        let state = Arc::clone(self);
        tokio::spawn(async move {
            let _registration = ActiveQuestionRegistration {
                state: Arc::clone(&state),
                operation_id: request.operation_id.clone(),
                cancellation: operation_shutdown.clone(),
            };
            let mut result = state
                .run_question(&request, &page, version, operation_shutdown.clone())
                .await;
            if operation_shutdown.is_cancelled() || state.turn_running() {
                result = Err(OperationFailure::Cancelled);
            }
            if let Ok(mut session) = state.session().await {
                if state.turn_running() {
                    result = Err(OperationFailure::Cancelled);
                }
                session.finish_question(&request, &result);
            }
            let _ = completion.send(result.map_err(|failure| {
                state.operation_error(
                    failure,
                    &request.session,
                    "question answering was cancelled",
                    ReviewApiError::QuestionFailed,
                )
            }));
        });

        response.await.unwrap_or(Err(ReviewApiError::Internal(
            "the question operation stopped unexpectedly",
        )))
    }

    async fn run_question(
        &self,
        request: &QuestionRequest,
        page: &ReviewPage,
        version: WorkspaceVersion,
        shutdown: CancellationToken,
    ) -> OperationResult {
        self.check_version(
            &version,
            shutdown.clone(),
            OperationFailure::Cancelled,
            "the workspace changed before the question was answered",
        )
        .await?;
        let answer = self
            .backend
            .answer_question(
                &page.diff.scope,
                &page.diff.overview,
                request,
                shutdown.clone(),
            )
            .await
            .map_err(|error| match error {
                OperationError::Cancelled => OperationFailure::Cancelled,
                OperationError::Failed(error) => OperationFailure::Agent(error),
            })?;
        self.check_version(
            &version,
            shutdown,
            OperationFailure::Cancelled,
            "the workspace changed while the question was answered",
        )
        .await?;
        let Ok(session) = self.session().await else {
            return Err(OperationFailure::Stale("the review is no longer loaded"));
        };
        if session
            .matching_page(request.generation, &request.range)
            .is_none()
        {
            return Err(OperationFailure::Stale(
                "the review changed while the question was answered",
            ));
        }
        Ok(answer)
    }

    /// Cancels the question `operation_id` names if it belongs to the given snapshot.
    pub(super) fn cancel_question(
        &self,
        operation_id: &str,
        generation: u64,
        range: ReviewRange,
    ) -> Result<(), ReviewApiError> {
        let Ok(active) = self.active_questions.lock() else {
            return Err(ReviewApiError::Internal(
                "the active question state is unavailable",
            ));
        };
        if let Some(question) = active.get(operation_id)
            && question.generation == generation
            && question.range == range
        {
            question.cancellation.cancel();
        }
        Ok(())
    }
}
