import type { ReviewComment, ReviewPage, ReviewSession } from "./protocol";
import type { QuestionThread } from "./question-state";
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
  questionsByOwner: Map<string, QuestionThread[]>;
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
    questionsByOwner: new Map(),
    terminal: { kind: "idle" },
  }, session.page);
}

export function activatePage(state: ReviewState, page: ReviewPage): ReviewState {
  const key = feedbackOwner(page.generation, page.selected_range);
  const hasFeedback = state.feedbackByOwner.has(key);
  const hasQuestions = state.questionsByOwner.has(key);
  if (hasFeedback && hasQuestions) return { ...state, page };

  const feedbackByOwner = new Map(state.feedbackByOwner);
  const questionsByOwner = new Map(state.questionsByOwner);
  if (!hasFeedback) feedbackByOwner.set(key, emptyFeedback());
  if (!hasQuestions) questionsByOwner.set(key, []);
  return { ...state, page, feedbackByOwner, questionsByOwner };
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

export function currentQuestions(state: ReviewState): QuestionThread[] {
  const key = feedbackOwner(state.page.generation, state.page.selected_range);
  const questions = state.questionsByOwner.get(key);
  if (!questions) throw new Error(`missing question owner ${key}`);
  return questions;
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
