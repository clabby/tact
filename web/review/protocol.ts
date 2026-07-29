import type { ReviewRange, ReviewTarget } from "./range-selection";

export const REVIEW_PROTOCOL_VERSION = 2;

export type ReviewComment = {
  id: number;
  path: string;
  side: "additions" | "deletions";
  start_line: number;
  end_line: number;
  body: string;
};

export type ReviewPage = {
  generation: number;
  selected_range: ReviewRange;
  patch: string;
  repository: string;
  scope: string;
  base: string;
};

export type ReviewSession = {
  protocol_version: number;
  generation: number;
  title: string;
  repository: string;
  trunk: string;
  range_targets: ReviewTarget[];
  default_range: ReviewRange;
  page: ReviewPage;
};

export type ReviewStatus = {
  generation: number;
  changed: boolean;
};

export type OverviewResponse = {
  generation: number;
  selected_range: ReviewRange;
  overview_html: string;
};

export type ThreadMessage = {
  role: "reviewer" | "agent";
  body: string;
};

export type QuestionRequest = {
  generation: number;
  range: ReviewRange;
  path: string;
  side: ReviewComment["side"];
  start_line: number;
  end_line: number;
  messages: ThreadMessage[];
};

export type QuestionResponse = {
  generation: number;
  selected_range: ReviewRange;
  answer: string;
};

export type ReviewDecision = {
  generation: number;
  range: ReviewRange;
  decision: "approve" | "request_changes";
  summary: string;
  comments: Omit<ReviewComment, "id">[];
};

export type ReviewErrorCode =
  | "stale_snapshot"
  | "invalid_range"
  | "workspace_changed"
  | "overview_failed"
  | "question_failed"
  | "invalid_thread"
  | "agent_busy"
  | "operation_cancelled"
  | "session_cancelled"
  | "invalid_comment_anchor"
  | "network_error"
  | "invalid_response"
  | "unknown";
