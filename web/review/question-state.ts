import type { ReviewComment, ThreadMessage } from "./protocol";

export type QuestionThread = {
  id: number;
  itemId: string;
  path: string;
  side: ReviewComment["side"];
  startLine: number;
  endLine: number;
  messages: ThreadMessage[];
  draft: string;
  validationError?: string;
  turn: QuestionTurn;
};

export type QuestionTurn =
  | { kind: "idle" }
  | { kind: "asking"; request: number; stopping: boolean }
  | { kind: "cancelled"; request: number }
  | { kind: "error"; request: number; message: string };

export type QuestionAnchor = Pick<
  QuestionThread,
  "itemId" | "path" | "side" | "startLine" | "endLine"
>;

export const MAX_THREAD_MESSAGES = 64;
export const MAX_THREAD_BYTES = 256 * 1024;

export function createQuestionThread(
  id: number,
  anchor: QuestionAnchor,
  question: string,
  request: number,
): QuestionThread {
  return {
    id,
    ...anchor,
    messages: [{ role: "reviewer", body: question.trim() }],
    draft: "",
    turn: { kind: "asking", request, stopping: false },
  };
}

export function beginFollowUp(thread: QuestionThread, request: number) {
  const question = thread.draft.trim();
  if (!question || thread.turn.kind !== "idle") return false;
  thread.messages.push({ role: "reviewer", body: question });
  thread.draft = "";
  thread.validationError = undefined;
  thread.turn = { kind: "asking", request, stopping: false };
  return true;
}

export function retryQuestion(thread: QuestionThread, request: number) {
  if (thread.turn.kind !== "error" && thread.turn.kind !== "cancelled") return false;
  thread.turn = { kind: "asking", request, stopping: false };
  return true;
}

export function cancelQuestion(thread: QuestionThread, request: number) {
  if (thread.turn.kind !== "asking" || thread.turn.request !== request) return false;
  thread.turn = { kind: "cancelled", request };
  return true;
}

export function beginStopping(thread: QuestionThread, request: number) {
  if (thread.turn.kind !== "asking"
    || thread.turn.request !== request
    || thread.turn.stopping) return false;
  thread.turn.stopping = true;
  return true;
}

export function stopFailed(thread: QuestionThread, request: number) {
  if (thread.turn.kind !== "asking" || thread.turn.request !== request) return false;
  thread.turn.stopping = false;
  return true;
}

export function questionValidationError(
  messages: readonly ThreadMessage[],
  question: string,
) {
  if (messages.length >= MAX_THREAD_MESSAGES - 1) {
    return "This thread is full. Start a new question on the code to continue.";
  }
  const bytes = messages.reduce(
    (total, message) => total + utf8Bytes(message.body),
    utf8Bytes(question.trim()),
  );
  if (bytes > MAX_THREAD_BYTES) {
    return "This thread is too large. Start a new question on the code to continue.";
  }
  return undefined;
}

export function finishQuestion(thread: QuestionThread, request: number, answer: string) {
  if (thread.turn.kind !== "asking" || thread.turn.request !== request) return false;
  thread.messages.push({ role: "agent", body: answer.trim() });
  thread.turn = { kind: "idle" };
  return true;
}

export function failQuestion(thread: QuestionThread, request: number, message: string) {
  if (thread.turn.kind !== "asking" || thread.turn.request !== request) return false;
  thread.turn = { kind: "error", request, message };
  return true;
}

function utf8Bytes(value: string) {
  return new TextEncoder().encode(value).byteLength;
}
