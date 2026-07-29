import type {
  ReviewComment,
  ReviewPage,
  ReviewSession,
  StoredQuestionThread,
} from "./protocol";
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
  const state = activatePage({
    session,
    page: session.page,
    feedbackByOwner: new Map(),
    questionsByOwner: new Map(),
    terminal: { kind: "idle" },
  }, session.page);
  return synchronizeQuestions(state, session.questions);
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

export function allQuestions(state: ReviewState): QuestionThread[] {
  return [...state.questionsByOwner.values()].flat();
}

export function synchronizeQuestions(
  state: ReviewState,
  stored: readonly StoredQuestionThread[],
) {
  const existing = new Map(
    allQuestions(state).map((thread) => [thread.id, thread]),
  );
  const questionsByOwner = new Map(state.questionsByOwner);
  for (const key of questionsByOwner.keys()) {
    if (key.startsWith(`${state.session.generation}:`)) questionsByOwner.set(key, []);
  }
  for (const question of stored) {
    if (question.generation !== state.session.generation) continue;
    const key = feedbackOwner(question.generation, question.range);
    const threads = questionsByOwner.get(key) ?? [];
    const previous = existing.get(question.thread_id);
    threads.push(storedQuestion(question, previous));
    questionsByOwner.set(key, threads);
  }
  return { ...state, questionsByOwner };
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

function storedQuestion(
  question: StoredQuestionThread,
  previous?: QuestionThread,
): QuestionThread {
  const turn = question.status === "asking"
    ? { kind: "asking" as const, request: 0, operationId: question.operation_id, stopping: false }
    : question.status === "error"
      ? { kind: "error" as const, request: 0, message: question.error ?? "The question failed." }
      : question.status === "cancelled"
        ? { kind: "cancelled" as const, request: 0 }
        : { kind: "idle" as const };
  return {
    id: question.thread_id,
    itemId: previous?.itemId ?? "",
    range: question.range,
    path: question.path,
    side: question.side,
    startLine: question.start_line,
    endLine: question.end_line,
    messages: question.messages.map((message) => ({ ...message })),
    draft: previous?.draft ?? "",
    validationError: previous?.validationError,
    turn,
  };
}
