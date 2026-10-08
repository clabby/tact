//! The Markdown a review decision becomes in a session's draft.
//!
//! The format is part of the contract with the agent that reads the draft: a heading with the
//! verdict, the reviewed scope, the reviewed checkout when it is not the session's own, the
//! summary, and one list item per comment with its `path:lines` anchor and diff side.

use super::model::{Decision, ReviewDecision, line_label};
use std::fmt::Write as _;

impl ReviewDecision {
    pub(super) fn to_markdown(&self) -> String {
        let heading = match self.decision {
            Decision::Approve => "Approved",
            Decision::RequestChanges => "Changes requested",
        };
        let mut markdown = format!("## Review: {heading}\n\n**Scope:** {}\n", self.scope);
        if let Some(checkout) = &self.reviewed_in {
            let _ = writeln!(markdown, "**Checkout:** `{}`", checkout.display());
        }
        let summary = self.summary.trim();
        if !summary.is_empty() {
            let _ = write!(markdown, "\n{summary}\n");
        }
        if self.comments.is_empty() {
            return markdown;
        }

        markdown.push_str("\n### Comments\n");
        for comment in &self.comments {
            let _ = write!(
                markdown,
                "\n- `{path}:{lines}` ({side})\n  {body}\n",
                path = comment.path,
                lines = line_label(comment.start_line, comment.end_line),
                side = comment.side.version_name(),
                body = comment.body.trim().replace('\n', "\n  "),
            );
        }
        markdown
    }
}

#[cfg(test)]
mod tests {
    use super::super::model::{CommentSide, Decision, ReviewComment, ReviewDecision};
    use tact_vcs::ReviewRange;

    fn decision(decision: Decision, summary: &str, comments: Vec<ReviewComment>) -> ReviewDecision {
        ReviewDecision {
            session: None,
            checkout: None,
            reviewed_in: None,
            generation: 0,
            range: ReviewRange { from: 0, to: 2 },
            scope: "Full branch".to_owned(),
            decision,
            summary: summary.to_owned(),
            comments,
        }
    }

    #[test]
    fn a_decision_with_comments_renders_the_full_document() {
        let comments = vec![
            ReviewComment {
                path: "src/lib.rs".to_owned(),
                side: CommentSide::Additions,
                start_line: 4,
                end_line: 4,
                body: "Handle the error.\nThis can fail.".to_owned(),
            },
            ReviewComment {
                path: "old.rs".to_owned(),
                side: CommentSide::Deletions,
                start_line: 1,
                end_line: 3,
                body: "  Why remove this?  ".to_owned(),
            },
        ];
        let mut review = decision(
            Decision::RequestChanges,
            "  Please address this.\n",
            comments,
        );
        review.reviewed_in = Some("/work/feature".into());

        assert_eq!(
            review.to_markdown(),
            "## Review: Changes requested\n\n\
             **Scope:** Full branch\n\
             **Checkout:** `/work/feature`\n\
             \n\
             Please address this.\n\
             \n\
             ### Comments\n\
             \n\
             - `src/lib.rs:4` (new)\n  Handle the error.\n  This can fail.\n\
             \n\
             - `old.rs:1-3` (old)\n  Why remove this?\n"
        );
    }

    #[test]
    fn an_approval_without_a_summary_is_only_the_heading_and_scope() {
        assert_eq!(
            decision(Decision::Approve, " ", Vec::new()).to_markdown(),
            "## Review: Approved\n\n**Scope:** Full branch\n"
        );
    }
}
