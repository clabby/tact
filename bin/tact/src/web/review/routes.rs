//! The review's HTTP routes: request shapes, preconditions, and response bodies.
//!
//! Handlers check what a request may ask for (a live session, no running turn, well-formed input)
//! and leave snapshot, cache, and operation bookkeeping to the engine. Failures are
//! [`ReviewApiError`] values, which render the review's error body.

use super::{
    engine::{OverviewKey, OverviewOperationKey, ReviewRegistry, ReviewSession, ReviewState},
    error::ReviewApiError,
    model::{
        AiReviewComment, QuestionRequest, ReviewBootstrap, ReviewDecision, ReviewPage,
        StoredOverview, StoredQuestion,
    },
    validation::{MAX_OVERVIEW_INSTRUCTIONS_BYTES, is_valid_operation_id},
};
use crate::{vcs::ReviewRange, web::api::secure_json};
use axum::{
    Json, Router,
    body::Body,
    extract::{Query, State},
    http::{Response, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

type ReviewResult = Result<Response<Body>, ReviewApiError>;

/// Routes for the review engine. The caller supplies authentication and request limits.
pub(in crate::web) fn router<S>(registry: Arc<ReviewRegistry>) -> Router<S>
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

#[derive(Deserialize)]
struct SessionQuery {
    session: Option<String>,
    checkout: Option<String>,
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
#[serde(deny_unknown_fields)]
struct RangeRequest {
    generation: u64,
    range: ReviewRange,
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
    range: ReviewRange,
    instructions: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AiReviewRequest {
    session: String,
    #[serde(default)]
    checkout: Option<String>,
    generation: u64,
    range: ReviewRange,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QuestionCancelRequest {
    /// Accepted for symmetry with the other session-scoped requests; operations are looked up by id.
    #[serde(default, rename = "session")]
    _session: serde::de::IgnoredAny,
    operation_id: String,
    generation: u64,
    range: ReviewRange,
}

/// The whole review as one session sees it.
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
struct ComposeResponse {
    markdown: String,
}

#[derive(Serialize)]
struct OverviewResponse {
    generation: u64,
    selected_range: ReviewRange,
    overview_mdx: String,
    instructions: Option<String>,
}

#[derive(Serialize)]
struct AiReviewResponse {
    generation: u64,
    selected_range: ReviewRange,
    comments: Vec<AiReviewComment>,
}

#[derive(Serialize)]
struct QuestionResponse {
    generation: u64,
    selected_range: ReviewRange,
    answer: String,
}

#[derive(Serialize)]
struct QuestionListResponse {
    generation: u64,
    questions: Vec<StoredQuestion>,
}

impl RefreshResponse {
    fn new(
        state: &ReviewState,
        session: &ReviewSession,
        session_id: &str,
        page: ReviewPage,
    ) -> Self {
        Self {
            bootstrap: session.bootstrap.clone(),
            page,
            overview: session.selected_overview(session_id),
            questions: session.questions_for(session_id),
            turn_running: state.turn_running(),
        }
    }
}

/// Fails unless `session` may start an agent operation now.
fn ensure_operation_allowed(state: &ReviewState, session: &str) -> Result<(), ReviewApiError> {
    if state.turn_running() {
        return Err(ReviewApiError::TurnRunning);
    }
    if !state.hub.is_live(session) {
        return Err(ReviewApiError::UnknownSession);
    }
    Ok(())
}

async fn review(
    State(registry): State<Arc<ReviewRegistry>>,
    Query(query): Query<SessionQuery>,
) -> ReviewResult {
    let (state, _) = registry
        .resolve(query.session.as_deref(), query.checkout.as_deref())
        .await?;
    state.ensure_loaded().await?;
    let session_id = query.session.as_deref().unwrap_or_default();
    let session = state.session().await?;
    let page = session.selected_page.clone();
    Ok(secure_json(
        StatusCode::OK,
        RefreshResponse::new(&state, &session, session_id, page),
    ))
}

/// Renders a review decision as the Markdown a browser inserts into a session's draft.
async fn compose(
    State(registry): State<Arc<ReviewRegistry>>,
    Json(mut decision): Json<ReviewDecision>,
) -> ReviewResult {
    let (state, target) = registry
        .resolve(decision.session.as_deref(), decision.checkout.as_deref())
        .await?;
    decision.reviewed_in = (target.path != target.session_workspace).then(|| target.path.clone());
    if !decision.is_well_formed() {
        return Err(ReviewApiError::InvalidCommentAnchor(
            "invalid review decision",
        ));
    }
    let session = state.session().await?;
    let Some(page) = session.matching_page(decision.generation, &decision.range) else {
        return Err(ReviewApiError::StaleSnapshot(
            "the submitted review snapshot is stale",
        ));
    };
    if !decision
        .comments
        .iter()
        .all(|comment| comment.is_anchored_in(&page.diff))
    {
        return Err(ReviewApiError::InvalidCommentAnchor(
            "a review comment is not anchored to the reviewed patch",
        ));
    }
    decision.scope.clone_from(&page.diff.scope);
    Ok(secure_json(
        StatusCode::OK,
        ComposeResponse {
            markdown: decision.to_markdown(),
        },
    ))
}

async fn refresh_review(
    State(registry): State<Arc<ReviewRegistry>>,
    Json(request): Json<GenerationRequest>,
) -> ReviewResult {
    let (state, _) = registry
        .resolve(request.session.as_deref(), request.checkout.as_deref())
        .await?;
    let session_id = request.session.as_deref().unwrap_or_default();
    let session = state.refresh(request.generation).await?;
    let page = session
        .page(&session.bootstrap.default_range)
        .expect("the refreshed default page must remain cached")
        .clone();
    Ok(secure_json(
        StatusCode::OK,
        RefreshResponse::new(&state, &session, session_id, page),
    ))
}

async fn load_range(
    State(registry): State<Arc<ReviewRegistry>>,
    Json(request): Json<RangeRequest>,
) -> ReviewResult {
    let (state, _) = registry
        .resolve(request.session.as_deref(), request.checkout.as_deref())
        .await?;
    let page = state.load_page(request.generation, request.range).await?;
    Ok(secure_json(StatusCode::OK, page))
}

async fn load_overview(
    State(registry): State<Arc<ReviewRegistry>>,
    Json(request): Json<OverviewRequest>,
) -> ReviewResult {
    let (state, _) = registry
        .resolve(Some(&request.session), request.checkout.as_deref())
        .await?;
    ensure_operation_allowed(&state, &request.session)?;
    let instructions = request
        .instructions
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    if instructions
        .as_ref()
        .is_some_and(|value| value.len() > MAX_OVERVIEW_INSTRUCTIONS_BYTES)
    {
        return Err(ReviewApiError::OverviewInstructionsTooLong);
    }
    let key = OverviewOperationKey {
        generation: request.generation,
        overview: OverviewKey {
            range: request.range,
            instructions,
            session: request.session,
        },
    };
    let overview_mdx = state.overview(key.clone()).await?;
    Ok(secure_json(
        StatusCode::OK,
        OverviewResponse {
            generation: key.generation,
            selected_range: key.overview.range,
            overview_mdx,
            instructions: key.overview.instructions,
        },
    ))
}

async fn run_ai_review(
    State(registry): State<Arc<ReviewRegistry>>,
    Json(request): Json<AiReviewRequest>,
) -> ReviewResult {
    let (state, _) = registry
        .resolve(Some(&request.session), request.checkout.as_deref())
        .await?;
    ensure_operation_allowed(&state, &request.session)?;
    let comments = state
        .ai_review(&request.session, request.generation, request.range)
        .await?;
    Ok(secure_json(
        StatusCode::OK,
        AiReviewResponse {
            generation: request.generation,
            selected_range: request.range,
            comments,
        },
    ))
}

async fn ask_question(
    State(registry): State<Arc<ReviewRegistry>>,
    Json(request): Json<QuestionRequest>,
) -> ReviewResult {
    let (state, _) = registry
        .resolve(Some(&request.session), request.checkout.as_deref())
        .await?;
    ensure_operation_allowed(&state, &request.session)?;
    if !request.is_well_formed() {
        return Err(ReviewApiError::InvalidThread(
            "the question thread is invalid",
        ));
    }
    let (generation, selected_range) = (request.generation, request.range);
    let answer = state.answer(request).await?;
    Ok(secure_json(
        StatusCode::OK,
        QuestionResponse {
            generation,
            selected_range,
            answer,
        },
    ))
}

async fn list_questions(
    State(registry): State<Arc<ReviewRegistry>>,
    Json(request): Json<GenerationRequest>,
) -> ReviewResult {
    let Some(session_id) = request.session else {
        return Err(ReviewApiError::InvalidThread(
            "the question list requires a session",
        ));
    };
    // Listing is allowed for a session that has since closed: it finds nothing.
    let live = registry
        .hub
        .is_live(&session_id)
        .then_some(session_id.as_str());
    let (state, _) = registry.resolve(live, request.checkout.as_deref()).await?;
    let session = state.session().await?;
    if session.generation != request.generation {
        return Err(ReviewApiError::StaleSnapshot(
            "the question list belongs to an older review generation",
        ));
    }
    Ok(secure_json(
        StatusCode::OK,
        QuestionListResponse {
            generation: session.generation,
            questions: session.questions_for(&session_id),
        },
    ))
}

async fn cancel_question(
    State(registry): State<Arc<ReviewRegistry>>,
    Json(request): Json<QuestionCancelRequest>,
) -> ReviewResult {
    if !is_valid_operation_id(&request.operation_id) {
        return Err(ReviewApiError::InvalidThread(
            "the question operation identifier is invalid",
        ));
    }
    // The operation identifier names the question; which checkout it belongs to does not matter.
    for state in registry.all().await {
        state.cancel_question(&request.operation_id, request.generation, request.range)?;
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}
