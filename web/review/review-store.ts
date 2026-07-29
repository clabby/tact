import type { ReviewComment, ReviewPage, ReviewSession } from "./protocol";
import { rangeKey, type ReviewRange } from "./range-selection";

export type CommentDraft = {
  itemId: string;
  path: string;
  side: ReviewComment["side"];
  startLine: number;
  endLine: number;
  body: string;
  editingId?: number;
  tab: "comment" | "preview";
};

export type CommentMetadata = ReviewComment & { itemId: string };

export type FeedbackState = {
  summary: string;
  comments: CommentMetadata[];
  draft?: CommentDraft;
  seenPaths: Set<string>;
};

export type TerminalState =
  | { kind: "idle" }
  | { kind: "busy"; action: "submit" | "cancel" }
  | { kind: "error"; action: "submit" | "cancel"; message: string; code: string }
  | { kind: "finished"; action: "submit" | "cancel" };

export type ReviewState = {
  session: ReviewSession;
  page: ReviewPage;
  feedbackByOwner: Map<string, FeedbackState>;
  terminal: TerminalState;
};

export function feedbackOwner(generation: number, range: ReviewRange) {
  return `${generation}:${rangeKey(range)}`;
}

export function createReviewState(session: ReviewSession): ReviewState {
  return activatePage({
    session,
    page: session.page,
    feedbackByOwner: new Map(),
    terminal: { kind: "idle" },
  }, session.page);
}

export function activatePage(state: ReviewState, page: ReviewPage): ReviewState {
  const key = feedbackOwner(page.generation, page.selected_range);
  if (state.feedbackByOwner.has(key)) return { ...state, page };
  const feedbackByOwner = new Map(state.feedbackByOwner);
  feedbackByOwner.set(key, emptyFeedback());
  return { ...state, page, feedbackByOwner };
}

export function installSession(_state: ReviewState, session: ReviewSession): ReviewState {
  return createReviewState(session);
}

export function currentFeedback(state: ReviewState): FeedbackState {
  const key = feedbackOwner(state.page.generation, state.page.selected_range);
  const feedback = state.feedbackByOwner.get(key);
  if (!feedback) throw new Error(`missing feedback owner ${key}`);
  return feedback;
}

export function feedbackDescription(feedback: FeedbackState) {
  const parts = [
    feedback.summary.trim() ? "the overall comment" : "",
    feedback.comments.length > 0
      ? `${feedback.comments.length} pending ${feedback.comments.length === 1 ? "comment" : "comments"}`
      : "",
    feedback.draft ? "the open comment draft" : "",
  ].filter(Boolean);
  return parts.join(parts.length > 2 ? ", " : " and ");
}

export function hasVisibleDraft(state: ReviewState) {
  return currentFeedback(state).draft !== undefined;
}

export function discardCurrentFeedback(state: ReviewState): ReviewState {
  const key = feedbackOwner(state.page.generation, state.page.selected_range);
  const feedbackByOwner = new Map(state.feedbackByOwner);
  feedbackByOwner.set(key, emptyFeedback());
  return { ...state, feedbackByOwner };
}

export function beginTerminal(
  state: ReviewState,
  action: "submit" | "cancel",
): ReviewState {
  if (state.terminal.kind === "busy" || state.terminal.kind === "finished") return state;
  return { ...state, terminal: { kind: "busy", action } };
}

export function failTerminal(
  state: ReviewState,
  action: "submit" | "cancel",
  code: string,
  message: string,
): ReviewState {
  return { ...state, terminal: { kind: "error", action, code, message } };
}

export function finishTerminal(
  state: ReviewState,
  action: "submit" | "cancel",
): ReviewState {
  return { ...state, terminal: { kind: "finished", action } };
}

function emptyFeedback(): FeedbackState {
  return { summary: "", comments: [], seenPaths: new Set() };
}

export class ReviewStore {
  state: ReviewState;

  constructor(session: ReviewSession) {
    this.state = createReviewState(session);
  }

  feedback() {
    return currentFeedback(this.state);
  }

  activate(page: ReviewPage) {
    this.state = activatePage(this.state, page);
  }

  replaceSession(session: ReviewSession) {
    this.state = installSession(this.state, session);
  }

  discardFeedback() {
    this.state = discardCurrentFeedback(this.state);
  }
}
