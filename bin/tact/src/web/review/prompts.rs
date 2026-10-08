//! The prompts the review sends to a session's agent, and the checks on what comes back.
//!
//! Every prompt names the repository and the exact revision range, so the agent can inspect the
//! source and history around the change instead of only the patch text.

use super::model::{AiReviewComment, AiReviewResult, QuestionRequest, line_label};
use crate::vcs::{OverviewContext, OverviewRange};

const MAX_OVERVIEW_BYTES: usize = 1024 * 1024;
const MAX_QUESTION_ANSWER_BYTES: usize = 256 * 1024;
const MAX_AI_REVIEW_BYTES: usize = 1024 * 1024;

/// An MDX explainer of a range for a human reviewer.
pub(super) struct OverviewPrompt<'a> {
    /// The range's label, such as "Full branch".
    pub(super) label: &'a str,
    pub(super) context: &'a OverviewContext,
    /// The reviewer's emphasis, if any.
    pub(super) instructions: Option<&'a str>,
}

/// A defect-finding review of a range that returns anchored JSON comments.
pub(super) struct AiReviewPrompt<'a> {
    pub(super) label: &'a str,
    pub(super) context: &'a OverviewContext,
}

/// The reviewer's latest question in a thread anchored to lines of the patch.
pub(super) struct QuestionPrompt<'a> {
    pub(super) label: &'a str,
    pub(super) context: &'a OverviewContext,
    pub(super) question: &'a QuestionRequest,
}

/// The agent's reply did not have the required shape.
#[derive(Debug, thiserror::Error)]
pub(super) enum AgentOutputError {
    #[error("the agent returned an empty review overview")]
    EmptyOverview,
    #[error("the agent returned a review overview larger than 1 MiB")]
    OverviewTooLarge,
    #[error("the agent returned a review larger than 1 MiB")]
    ReviewTooLarge,
    #[error("the agent returned invalid review JSON: {0}")]
    InvalidReviewJson(#[source] serde_json::Error),
    #[error("the agent returned an empty answer")]
    EmptyAnswer,
    #[error("the agent returned an answer larger than 256 KiB")]
    AnswerTooLarge,
}

impl OverviewPrompt<'_> {
    pub(super) fn render(&self) -> String {
        let label = self.label;
        let repository = repository_scope(self.context);
        let mut prompt = format!(
            r#"Delegate this task to a sub-agent so the host agent does not absorb the investigation context. Ask the sub-agent to quickly write a concise MDX explainer of `{label}` for a human reviewer. {repository} Inspect the diff, actual source files, relevant history, and surrounding code needed to understand the change without modifying the workspace. Explain what changed, why it matters, how the pieces fit together, and where a human reviewer should direct attention. Keep it brief and proportionate to the change. This is guidance for a human reviewer; do not perform a comprehensive defect audit or generate inline review findings. Whenever directing the reviewer's attention to code, cite direct `path:line` or `path:start-end` locations. Return only MDX source with Markdown headings, paragraphs, lists, tables, and fenced code where useful. Prefer the concise built-in components `<Callout title="..." tone="...">`, `<CardGrid>` with `<Card title="..." label="...">`, `<MetricGrid>` with `<Metric value="..." label="..." detail="..." />`, `<Process>` with `<ProcessStep title="...">`, and `<Figure caption="...">` for familiar layouts. You may define your own MDX components with `export function Name() {{ return <svg viewBox="0 0 400 120">...</svg> }}` and use `<Name />` for custom charts, diagrams, SVGs, and visual explanations when they clarify a change. Write self-contained JSX and calculations; do not import packages, fetch external resources, or use Markdown fences around the document. Charts must reflect actual repository facts, not invented measurements. The overview runs in an isolated frame with no network access. Keep all visualizations accessible and responsive."#,
        );
        prompt.push_str(" Use CSS theme variables such as `var(--ink)`, `var(--muted)`, `var(--paper-deep)`, `var(--rule)`, and `var(--accent)` in custom components and SVGs. Text and surfaces must remain legible in light and dark mode; avoid hard-coded black or white.");
        if let Some(instructions) = self.instructions {
            prompt.push_str("\n\nApply the reviewer's additional instructions to the emphasis and presentation of this overview. Keep it a concise explanation for a human reviewer, without turning it into a comprehensive defect audit:\n");
            prompt.push_str(instructions);
        }
        prompt
    }

    /// The overview in the agent's reply, without a surrounding code fence.
    pub(super) fn parse(reply: &str) -> Result<String, AgentOutputError> {
        let reply = reply.trim();
        let overview = reply
            .strip_prefix("```mdx")
            .or_else(|| reply.strip_prefix("```markdown"))
            .and_then(|value| value.strip_suffix("```"))
            .map_or(reply, str::trim);
        if overview.is_empty() {
            return Err(AgentOutputError::EmptyOverview);
        }
        if overview.len() > MAX_OVERVIEW_BYTES {
            return Err(AgentOutputError::OverviewTooLarge);
        }
        Ok(overview.to_owned())
    }
}

impl AiReviewPrompt<'_> {
    pub(super) fn render(&self) -> String {
        let label = self.label;
        let repository = repository_scope(self.context);
        format!(
            "Delegate a comprehensive code review of `{label}` to a sub-agent and wait for its result. {repository} Inspect the actual diff, source, surrounding callers, tests, and relevant history without modifying the workspace. Find actionable bugs, regressions, and security or correctness failures introduced by this change. Verify each finding against the code and cite only lines present in the selected diff. Return only a JSON object with a `comments` array. Each comment must have `path` (repository-relative path), `side` (`additions` or `deletions`), `start_line` and `end_line` (positive line numbers on that side of the diff), `body` (beginning with a severity label `[P0]` for critical, `[P1]` for high, `[P2]` for medium, or `[P3]` for low, then a concise explanation of the failure, triggering condition, and consequence). Prefer the smallest relevant changed line range. If no actionable defects are found, return {{\"comments\":[]}}. Do not include speculative suggestions, a summary, Markdown fences, or prose outside the JSON object."
        )
    }

    /// The comments in the agent's reply, which may be wrapped in a JSON code fence.
    pub(super) fn parse(reply: &str) -> Result<Vec<AiReviewComment>, AgentOutputError> {
        if reply.len() > MAX_AI_REVIEW_BYTES {
            return Err(AgentOutputError::ReviewTooLarge);
        }
        let reply = reply.trim();
        let reply = reply
            .strip_prefix("```json")
            .and_then(|value| value.strip_suffix("```"))
            .map_or(reply, str::trim);
        let review: AiReviewResult =
            serde_json::from_str(reply).map_err(AgentOutputError::InvalidReviewJson)?;
        Ok(review.comments)
    }
}

impl QuestionPrompt<'_> {
    pub(super) fn render(&self) -> String {
        let question = self.question;
        let label = self.label;
        let repository = repository_scope(self.context);
        let side = question.side.version_name();
        let lines = line_label(question.start_line, question.end_line);
        let messages =
            serde_json::to_string(&question.messages).expect("thread messages serialize to JSON");
        format!(
            "Delegate this task to a sub-agent so the host agent does not absorb the investigation context. Ask the sub-agent to answer the reviewer's latest question about `{path}:{lines}` on the {side} side of `{label}`. {repository} It must inspect the repository, diff, history, selected lines, and surrounding code needed for an accurate answer without modifying the workspace. The complete conversation is JSON: {messages}. Return the sub-agent's answer as concise Markdown with direct `path:line` citations where useful. Return only the answer to the reviewer, with no preamble about delegation.",
            path = question.path,
        )
    }

    pub(super) fn parse(reply: &str) -> Result<String, AgentOutputError> {
        let answer = reply.trim();
        if answer.is_empty() {
            return Err(AgentOutputError::EmptyAnswer);
        }
        if answer.len() > MAX_QUESTION_ANSWER_BYTES {
            return Err(AgentOutputError::AnswerTooLarge);
        }
        Ok(answer.to_owned())
    }
}

/// Tells the agent where the change lives: a commit range, or a base commit through the working
/// tree.
fn repository_scope(context: &OverviewContext) -> String {
    let repository = context.repository.to_string_lossy();
    match &context.range {
        OverviewRange::Commits { base, head } => format!(
            "Inspect the Git commit range `{base}..{head}` in the repository at `{repository}`."
        ),
        OverviewRange::WorkingTree { base } => format!(
            "Inspect the changes from Git commit `{base}` through the working tree, including untracked files, in the repository at `{repository}`."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{AgentOutputError, AiReviewPrompt, OverviewPrompt};

    #[test]
    fn overview_replies_lose_their_fence_and_must_not_be_empty() {
        assert_eq!(
            OverviewPrompt::parse("```mdx\n## Overview\n```\n").unwrap(),
            "## Overview"
        );
        assert_eq!(
            OverviewPrompt::parse("<p>Plain</p>").unwrap(),
            "<p>Plain</p>"
        );
        assert!(matches!(
            OverviewPrompt::parse("```markdown\n```"),
            Err(AgentOutputError::EmptyOverview)
        ));
    }

    #[test]
    fn ai_review_replies_are_json_with_an_optional_fence() {
        let comments = AiReviewPrompt::parse(
            "```json\n{\"comments\":[{\"path\":\"a.rs\",\"side\":\"additions\",\"start_line\":1,\"end_line\":2,\"body\":\"[P1] x\"}]}\n```",
        )
        .unwrap();

        assert_eq!(comments.len(), 1);
        assert_eq!(
            (
                comments[0].path.as_str(),
                comments[0].start_line,
                comments[0].end_line
            ),
            ("a.rs", 1, 2)
        );
        assert!(matches!(
            AiReviewPrompt::parse("Looks good to me."),
            Err(AgentOutputError::InvalidReviewJson(_))
        ));
    }
}
