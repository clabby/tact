import type { FileContext } from "./diff-context";
import type { ReviewRange, ReviewTarget } from "./range-selection";

export const REVIEW_PROTOCOL_VERSION = 1;

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
  snapshot_id: string;
  patch_id: string;
  title: string;
  patch: string;
  file_contexts?: FileContext[];
  repository: string;
  scope: string;
  base: string;
};

export type ReviewSession = {
  protocol_version: number;
  generation: number;
  workspace_version: string;
  snapshot_id: string;
  title: string;
  repository: string;
  trunk: string;
  range_targets: ReviewTarget[];
  default_range: ReviewRange;
  page: ReviewPage;
};

export type ReviewStatus = {
  generation: number;
  workspace_version: string;
  changed: boolean;
};

export type OverviewResponse = {
  generation: number;
  selected_range: ReviewRange;
  snapshot_id: string;
  patch_id: string;
  overview_html: string;
};

export type ReviewDecision = {
  generation: number;
  snapshot_id: string;
  patch_id: string;
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
  | "operation_cancelled"
  | "session_cancelled"
  | "invalid_comment_anchor"
  | "network_error"
  | "invalid_response"
  | "unknown";
