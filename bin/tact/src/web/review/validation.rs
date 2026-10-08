//! Size and shape limits on what browsers and agents submit to the review.
//!
//! These checks bound memory and reject malformed input before it reaches the engine. Whether a
//! comment points at reviewed lines is checked separately, against the snapshot it names.

use super::model::{
    AiReviewComment, CommentSide, QuestionRequest, ReviewComment, ReviewDecision, ThreadRole,
};
use tact_vcs::DiffSnapshot;

pub(super) const MAX_COMMENTS: usize = 256;
const MAX_COMMENT_BYTES: usize = 64 * 1024;
const MAX_SUMMARY_BYTES: usize = 64 * 1024;
const MAX_PATH_BYTES: usize = 4096;
const MAX_OPERATION_ID_BYTES: usize = 128;
const MAX_THREAD_MESSAGES: usize = 64;
const MAX_THREAD_BYTES: usize = 256 * 1024;
pub(super) const MAX_OVERVIEW_INSTRUCTIONS_BYTES: usize = 8 * 1024;
/// The severity labels an AI review comment must begin with.
const SEVERITY_LABELS: [&str; 4] = ["[P0] ", "[P1] ", "[P2] ", "[P3] "];

/// Whether lines `start_line` through `end_line` of `path` lie inside one hunk of the snapshot.
pub(super) fn is_anchored(
    snapshot: &DiffSnapshot,
    path: &str,
    side: CommentSide,
    start_line: u32,
    end_line: u32,
) -> bool {
    snapshot.contains_anchor(path, side.patch_side(), start_line, end_line)
}

/// Whether an identifier chosen by the browser is non-blank, short, and ASCII.
pub(super) fn is_valid_operation_id(operation_id: &str) -> bool {
    !operation_id.trim().is_empty()
        && operation_id.len() <= MAX_OPERATION_ID_BYTES
        && operation_id.is_ascii()
}

fn is_valid_path(path: &str) -> bool {
    !path.trim().is_empty() && path.len() <= MAX_PATH_BYTES
}

fn is_valid_body(body: &str) -> bool {
    !body.trim().is_empty() && body.len() <= MAX_COMMENT_BYTES
}

fn is_valid_line_range(start_line: u32, end_line: u32) -> bool {
    start_line != 0 && end_line >= start_line
}

impl ReviewComment {
    pub(super) fn is_well_formed(&self) -> bool {
        is_valid_path(&self.path)
            && is_valid_body(&self.body)
            && is_valid_line_range(self.start_line, self.end_line)
    }

    pub(super) fn is_anchored_in(&self, snapshot: &DiffSnapshot) -> bool {
        is_anchored(
            snapshot,
            &self.path,
            self.side,
            self.start_line,
            self.end_line,
        )
    }
}

impl AiReviewComment {
    pub(super) fn is_well_formed(&self) -> bool {
        is_valid_path(&self.path)
            && is_valid_body(&self.body)
            && SEVERITY_LABELS
                .iter()
                .any(|label| self.body.starts_with(label) && self.body.len() > label.len())
            && is_valid_line_range(self.start_line, self.end_line)
    }

    pub(super) fn is_anchored_in(&self, snapshot: &DiffSnapshot) -> bool {
        is_anchored(
            snapshot,
            &self.path,
            self.side,
            self.start_line,
            self.end_line,
        )
    }
}

impl ReviewDecision {
    pub(super) fn is_well_formed(&self) -> bool {
        self.summary.len() <= MAX_SUMMARY_BYTES
            && self.comments.len() <= MAX_COMMENTS
            && self.comments.iter().all(ReviewComment::is_well_formed)
    }
}

impl QuestionRequest {
    /// Whether the identifiers, anchor, and thread are well formed: messages alternate between
    /// reviewer and agent, start and end with the reviewer, and stay within the size limits.
    pub(super) fn is_well_formed(&self) -> bool {
        if !is_valid_operation_id(&self.thread_id)
            || !is_valid_operation_id(&self.operation_id)
            || !is_valid_path(&self.path)
            || !is_valid_line_range(self.start_line, self.end_line)
            || self.messages.is_empty()
            || self.messages.len() > MAX_THREAD_MESSAGES
        {
            return false;
        }
        let mut bytes = 0_usize;
        for (index, message) in self.messages.iter().enumerate() {
            let expected = if index.is_multiple_of(2) {
                ThreadRole::Reviewer
            } else {
                ThreadRole::Agent
            };
            if message.role != expected || message.body.trim().is_empty() {
                return false;
            }
            bytes = bytes.saturating_add(message.body.len());
        }
        self.messages.last().map(|message| message.role) == Some(ThreadRole::Reviewer)
            && bytes <= MAX_THREAD_BYTES
    }
}
