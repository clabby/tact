//! The review's data: snapshots, decisions, comments, and question threads, in their wire shapes.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tact_vcs::{CheckoutKind, DiffSnapshot, PatchSide, ReviewRange, ReviewTarget};

/// One captured range of the review.
#[derive(Clone, Serialize)]
pub(super) struct ReviewPage {
    pub(super) generation: u64,
    pub(super) selected_range: ReviewRange,
    pub(super) full_context: bool,
    #[serde(flatten)]
    pub(super) diff: DiffSnapshot,
}

/// What a browser needs to lay out a review before it loads any range.
#[derive(Clone, Serialize)]
pub(super) struct ReviewBootstrap {
    pub(super) protocol_version: u32,
    pub(super) generation: u64,
    pub(super) title: String,
    pub(super) repository: String,
    /// The checkout this review reads.
    pub(super) checkout: CheckoutIdentity,
    pub(super) trunk: String,
    pub(super) range_targets: Vec<ReviewTarget>,
    pub(super) default_range: ReviewRange,
}

#[derive(Clone, Serialize)]
pub(super) struct CheckoutIdentity {
    pub(super) path: PathBuf,
    pub(super) name: String,
    pub(super) label: String,
    pub(super) kind: CheckoutKind,
}

/// The agent's answer to an AI review prompt.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AiReviewResult {
    pub(super) comments: Vec<AiReviewComment>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AiReviewComment {
    pub(super) path: String,
    pub(super) side: CommentSide,
    pub(super) start_line: u32,
    pub(super) end_line: u32,
    /// Begins with a severity label such as `[P1] `.
    pub(super) body: String,
}

/// The overview a session has selected for the current range.
#[derive(Clone, Serialize)]
pub(super) struct StoredOverview {
    pub(super) selected_range: ReviewRange,
    pub(super) status: OverviewStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) overview_mdx: Option<String>,
    pub(super) instructions: Option<String>,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum OverviewStatus {
    Generating,
    Ready,
}

/// A reviewer's question about lines of the patch, with the thread so far.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct QuestionRequest {
    pub(super) session: String,
    #[serde(default)]
    pub(super) checkout: Option<String>,
    pub(super) thread_id: String,
    pub(super) operation_id: String,
    pub(super) generation: u64,
    pub(super) range: ReviewRange,
    pub(super) path: String,
    pub(super) side: CommentSide,
    pub(super) start_line: u32,
    pub(super) end_line: u32,
    /// Alternates reviewer and agent, starting and ending with the reviewer.
    pub(super) messages: Vec<ThreadMessage>,
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

/// A question thread as the server keeps it for one session.
#[derive(Clone, Serialize)]
pub(super) struct StoredQuestion {
    #[serde(skip)]
    pub(super) session: String,
    pub(super) thread_id: String,
    pub(super) operation_id: String,
    pub(super) generation: u64,
    pub(super) range: ReviewRange,
    pub(super) path: String,
    pub(super) side: CommentSide,
    pub(super) start_line: u32,
    pub(super) end_line: u32,
    pub(super) messages: Vec<ThreadMessage>,
    pub(super) status: QuestionStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) error: Option<String>,
}

#[derive(Clone, Copy, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum QuestionStatus {
    Asking,
    Idle,
    Error,
    Cancelled,
}

/// A reviewer's verdict on a range, which the server renders as Markdown for the session's draft.
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
    pub(super) range: ReviewRange,
    /// The reviewed range's label, filled in from the snapshot.
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

/// Which side of the patch a comment's line numbers count.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum CommentSide {
    Additions,
    Deletions,
}

impl CommentSide {
    pub(super) fn patch_side(self) -> PatchSide {
        match self {
            Self::Additions => PatchSide::Additions,
            Self::Deletions => PatchSide::Deletions,
        }
    }

    /// How the side is named in prose: the new or the old version of the file.
    pub(super) fn version_name(self) -> &'static str {
        match self {
            Self::Additions => "new",
            Self::Deletions => "old",
        }
    }
}

/// `start`, or `start-end` for a multi-line range.
pub(super) fn line_label(start_line: u32, end_line: u32) -> String {
    if start_line == end_line {
        start_line.to_string()
    } else {
        format!("{start_line}-{end_line}")
    }
}
