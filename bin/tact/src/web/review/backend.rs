//! The review's access to the checkout and to the session's agent.

use super::{
    agent::{AgentPrompt, ReviewAgent},
    model::{AiReviewComment, CheckoutIdentity, QuestionRequest, ReviewBootstrap, ReviewPage},
    prompts::{AiReviewPrompt, OverviewPrompt, QuestionPrompt},
};
use crate::{
    core::protocol::AuxiliaryError,
    vcs::{OverviewContext, ReviewContext, ReviewRange, VcsError, WorkspaceVersion},
    web::{hub::Hub, wire::PROTOCOL_VERSION, workspaces::Target},
};
use std::{path::PathBuf, sync::Arc};
use tokio_util::sync::CancellationToken;

/// How often preparation re-reads a checkout that changed while it was being captured.
const MAX_PREPARATION_ATTEMPTS: usize = 3;

/// A freshly captured review: its bootstrap, its default page, and the version it was read at.
#[derive(Clone)]
pub(super) struct PreparedReview {
    pub(super) bootstrap: ReviewBootstrap,
    pub(super) initial_page: ReviewPage,
    pub(super) context: ReviewContext,
    pub(super) version: WorkspaceVersion,
}

/// A failure to capture a checkout for review.
#[derive(Debug, thiserror::Error)]
pub(super) enum ReviewError {
    #[error(transparent)]
    Diff(#[from] VcsError),
    #[error("failed to validate the review workspace: {0}")]
    WorkspaceValidation(String),
    #[error("review preparation was cancelled")]
    Cancelled,
    #[error("the workspace kept changing while the review was being prepared; try again")]
    WorkspaceChanged,
}

/// A failure of a step that can be cancelled: reading the checkout's version or asking the agent.
pub(super) enum OperationError {
    Cancelled,
    Failed(String),
}

/// Reads one checkout and runs prompts about it on a session's agent.
pub(super) struct ReviewBackend {
    pub(super) workspace: PathBuf,
    identity: CheckoutIdentity,
    hub: Hub,
    agent: Arc<dyn ReviewAgent>,
}

impl ReviewBackend {
    pub(super) fn new(target: Target, hub: Hub, agent: Arc<dyn ReviewAgent>) -> Self {
        Self {
            workspace: target.path.clone(),
            identity: CheckoutIdentity {
                path: target.path,
                name: target.name,
                label: target.label,
                kind: target.kind,
            },
            hub,
            agent,
        }
    }

    /// Captures the workspace's default page. A live turn can change the workspace between the
    /// patch read and its validation; that is tolerated while any session runs, because the
    /// workspace watcher reports later changes.
    pub(super) async fn prepare(
        &self,
        shutdown: CancellationToken,
    ) -> Result<PreparedReview, ReviewError> {
        for _ in 0..MAX_PREPARATION_ATTEMPTS {
            let context = tokio::select! {
                result = ReviewContext::load(&self.workspace) => result?,
                () = shutdown.cancelled() => return Err(ReviewError::Cancelled),
            };
            let default_range = context.default_range();
            let initial_page = self
                .prepare_page(context.clone(), default_range, shutdown.clone())
                .await?;
            let version = context.version();
            let current =
                self.current_version(shutdown.clone())
                    .await
                    .map_err(|error| match error {
                        OperationError::Cancelled => ReviewError::Cancelled,
                        OperationError::Failed(error) => ReviewError::WorkspaceValidation(error),
                    })?;
            if current != version && !self.hub.any_busy() {
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

    pub(super) async fn current_version(
        &self,
        shutdown: CancellationToken,
    ) -> Result<WorkspaceVersion, OperationError> {
        tokio::select! {
            result = WorkspaceVersion::current(&self.workspace) => {
                result.map_err(|error| OperationError::Failed(error.to_string()))
            }
            () = shutdown.cancelled() => Err(OperationError::Cancelled),
        }
    }

    pub(super) async fn prepare_page(
        &self,
        context: ReviewContext,
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

    pub(super) async fn overview(
        &self,
        session: &str,
        prompt: OverviewPrompt<'_>,
        shutdown: CancellationToken,
    ) -> Result<String, OperationError> {
        let reply = self.run_agent(session, prompt.render(), shutdown).await?;
        OverviewPrompt::parse(&reply).map_err(|error| OperationError::Failed(error.to_string()))
    }

    pub(super) async fn ai_review(
        &self,
        session: &str,
        label: &str,
        context: &OverviewContext,
        shutdown: CancellationToken,
    ) -> Result<Vec<AiReviewComment>, OperationError> {
        let prompt = AiReviewPrompt { label, context }.render();
        let reply = self.run_agent(session, prompt, shutdown).await?;
        AiReviewPrompt::parse(&reply).map_err(|error| OperationError::Failed(error.to_string()))
    }

    pub(super) async fn answer_question(
        &self,
        label: &str,
        context: &OverviewContext,
        question: &QuestionRequest,
        shutdown: CancellationToken,
    ) -> Result<String, OperationError> {
        let prompt = QuestionPrompt {
            label,
            context,
            question,
        }
        .render();
        let reply = self.run_agent(&question.session, prompt, shutdown).await?;
        QuestionPrompt::parse(&reply).map_err(|error| OperationError::Failed(error.to_string()))
    }

    /// Runs the prompt on `session`'s worker, abandoning it when the session closes.
    async fn run_agent(
        &self,
        session: &str,
        prompt: String,
        shutdown: CancellationToken,
    ) -> Result<String, OperationError> {
        let Some(closed) = self.hub.closed_token(session) else {
            return Err(OperationError::Failed(
                "the session is no longer live".to_owned(),
            ));
        };
        let run = shutdown.child_token();
        let prompt = AgentPrompt {
            session: session.to_owned(),
            prompt,
            shutdown: run.clone(),
        };
        let result = tokio::select! {
            result = self.agent.run(prompt) => result,
            () = closed.cancelled() => {
                run.cancel();
                return Err(OperationError::Cancelled);
            }
        };
        result.map_err(|error| match error {
            AuxiliaryError::Cancelled => OperationError::Cancelled,
            AuxiliaryError::Failed(error) => OperationError::Failed(error),
        })
    }
}
