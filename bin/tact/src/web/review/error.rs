//! Failures of the review routes and the JSON body that reports them.
//!
//! Every failure carries a stable [`ErrorCode`] that clients act on, an HTTP status, and a
//! [`Recovery`] that tells the client whether repeating the request can help and whether its
//! current snapshot is still usable. The display text is the human-readable message.

use super::backend::ReviewError;
use crate::{
    vcs::VcsError,
    web::{api::secure_json, workspaces::WorkspaceError},
};
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;

/// A review request that could not be served.
#[derive(Debug, thiserror::Error)]
pub(super) enum ReviewApiError {
    /// The request names a review generation, range, or snapshot that no longer exists.
    #[error("{0}")]
    StaleSnapshot(&'static str),
    #[error("{0}")]
    InvalidRange(String),
    #[error("overview instructions exceed 8 KiB")]
    OverviewInstructionsTooLong,
    #[error("The folder must be a git repository.")]
    NotRepository,
    /// The checkout could not be captured; it may be in the middle of a change.
    #[error(transparent)]
    Preparation(ReviewError),
    /// The checkout's current version could not be read to validate a snapshot.
    #[error("{0}")]
    WorkspaceUnreadable(String),
    #[error("{0}")]
    OverviewFailed(String),
    #[error("{0}")]
    AiReviewFailed(String),
    #[error("{0}")]
    QuestionFailed(String),
    #[error("{0}")]
    InvalidThread(&'static str),
    #[error("The agent turn is still running. Review actions are available when it finishes.")]
    TurnRunning,
    /// The operation was cancelled by the client or superseded; it can be started again.
    #[error("{0}")]
    OperationCancelled(&'static str),
    /// The operation stopped because its session closed.
    #[error("{0}")]
    SessionCancelled(&'static str),
    #[error("{0}")]
    InvalidCommentAnchor(&'static str),
    #[error("the session is not live")]
    UnknownSession,
    #[error("{0}")]
    InvalidCheckout(String),
    /// The server failed in a way that does not depend on the request.
    #[error("{0}")]
    Internal(&'static str),
}

/// The stable identifier of a failure, as sent in the `code` field.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ErrorCode {
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
    Failed,
}

/// What a client can do about a failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Recovery {
    /// Repeat the request; the snapshot it named is still valid.
    Retry,
    /// Reload the review first; the snapshot the request named is gone.
    Reload,
    /// Change the request; repeating it unchanged fails again.
    Correct,
    /// The checkout cannot be reviewed at all.
    Unreviewable,
}

impl ReviewApiError {
    pub(super) fn code(&self) -> ErrorCode {
        match self {
            Self::StaleSnapshot(_) => ErrorCode::StaleSnapshot,
            Self::InvalidRange(_) => ErrorCode::InvalidRange,
            Self::OverviewInstructionsTooLong => ErrorCode::InvalidOverviewInstructions,
            Self::NotRepository | Self::Preparation(_) | Self::WorkspaceUnreadable(_) => {
                ErrorCode::WorkspaceChanged
            }
            Self::OverviewFailed(_) => ErrorCode::OverviewFailed,
            Self::AiReviewFailed(_) => ErrorCode::AiReviewFailed,
            Self::QuestionFailed(_) => ErrorCode::QuestionFailed,
            Self::InvalidThread(_) => ErrorCode::InvalidThread,
            Self::TurnRunning => ErrorCode::TurnRunning,
            Self::OperationCancelled(_) => ErrorCode::OperationCancelled,
            Self::SessionCancelled(_) => ErrorCode::SessionCancelled,
            Self::InvalidCommentAnchor(_) => ErrorCode::InvalidCommentAnchor,
            Self::UnknownSession => ErrorCode::UnknownSession,
            Self::InvalidCheckout(_) => ErrorCode::InvalidCheckout,
            Self::Internal(_) => ErrorCode::Failed,
        }
    }

    fn status(&self) -> StatusCode {
        match self {
            Self::StaleSnapshot(_)
            | Self::TurnRunning
            | Self::OperationCancelled(_)
            | Self::SessionCancelled(_) => StatusCode::CONFLICT,
            Self::OverviewInstructionsTooLong | Self::InvalidCheckout(_) => StatusCode::BAD_REQUEST,
            Self::UnknownSession => StatusCode::NOT_FOUND,
            Self::WorkspaceUnreadable(_) => StatusCode::SERVICE_UNAVAILABLE,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::InvalidRange(_)
            | Self::NotRepository
            | Self::Preparation(_)
            | Self::OverviewFailed(_)
            | Self::AiReviewFailed(_)
            | Self::QuestionFailed(_)
            | Self::InvalidThread(_)
            | Self::InvalidCommentAnchor(_) => StatusCode::UNPROCESSABLE_ENTITY,
        }
    }

    pub(super) fn recovery(&self) -> Recovery {
        match self {
            Self::StaleSnapshot(_) | Self::Preparation(_) | Self::WorkspaceUnreadable(_) => {
                Recovery::Reload
            }
            Self::NotRepository => Recovery::Unreviewable,
            Self::OverviewInstructionsTooLong
            | Self::OverviewFailed(_)
            | Self::AiReviewFailed(_)
            | Self::QuestionFailed(_)
            | Self::TurnRunning
            | Self::OperationCancelled(_)
            | Self::Internal(_) => Recovery::Retry,
            Self::InvalidRange(_)
            | Self::InvalidThread(_)
            | Self::SessionCancelled(_)
            | Self::InvalidCommentAnchor(_)
            | Self::UnknownSession
            | Self::InvalidCheckout(_) => Recovery::Correct,
        }
    }
}

impl From<ReviewError> for ReviewApiError {
    fn from(error: ReviewError) -> Self {
        match error {
            ReviewError::Cancelled => Self::OperationCancelled("review preparation was cancelled"),
            ReviewError::Diff(VcsError::NotRepository(_)) => Self::NotRepository,
            error => Self::Preparation(error),
        }
    }
}

impl From<WorkspaceError> for ReviewApiError {
    fn from(error: WorkspaceError) -> Self {
        match error {
            WorkspaceError::UnknownSession => Self::UnknownSession,
            WorkspaceError::NotACheckout(_) => Self::InvalidCheckout(error.to_string()),
        }
    }
}

impl IntoResponse for ReviewApiError {
    fn into_response(self) -> Response {
        #[derive(Serialize)]
        struct Body {
            code: ErrorCode,
            error: String,
            retryable: bool,
            snapshot_valid: bool,
        }
        let recovery = self.recovery();
        secure_json(
            self.status(),
            Body {
                code: self.code(),
                error: self.to_string(),
                retryable: matches!(recovery, Recovery::Retry | Recovery::Reload),
                snapshot_valid: matches!(recovery, Recovery::Retry | Recovery::Correct),
            },
        )
    }
}
