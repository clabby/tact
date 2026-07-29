import { describe, expect, test } from "bun:test";
import {
  beginFollowUp,
  beginStopping,
  cancelQuestion,
  createQuestionThread,
  failQuestion,
  finishQuestion,
  questionValidationError,
  retryQuestion,
  stopFailed,
} from "./question-state";

const anchor = {
  itemId: "item",
  range: { from: 0, to: 1 },
  path: "src/main.rs",
  side: "additions" as const,
  startLine: 4,
  endLine: 8,
};

describe("inline agent question threads", () => {
  test("preserves an alternating conversation across follow-ups", () => {
    const thread = createQuestionThread("thread-1", anchor, "Why?", 10, "operation-10");
    expect(thread.messages).toEqual([{ role: "reviewer", body: "Why?" }]);
    expect(finishQuestion(thread, 10, "Because.")).toBe(true);

    thread.draft = "Where is it used?";
    expect(beginFollowUp(thread, 11, "operation-11")).toBe(true);
    expect(finishQuestion(thread, 11, "In `src/lib.rs:12`.")).toBe(true);
    expect(thread.messages.map(({ role }) => role)).toEqual([
      "reviewer", "agent", "reviewer", "agent",
    ]);
  });

  test("keeps a failed question for retry and ignores stale completions", () => {
    const thread = createQuestionThread("thread-1", anchor, "Why?", 10, "operation-10");
    expect(failQuestion(thread, 10, "network failed")).toBe(true);
    expect(thread.messages).toHaveLength(1);
    expect(retryQuestion(thread, 11, "operation-11")).toBe(true);
    expect(finishQuestion(thread, 10, "stale")).toBe(false);
    expect(finishQuestion(thread, 11, "Current answer")).toBe(true);
    expect(thread.messages.at(-1)).toEqual({ role: "agent", body: "Current answer" });
  });

  test("stopping is tied to the active request", () => {
    const thread = createQuestionThread("thread-1", anchor, "Why?", 10, "operation-10");

    expect(beginStopping(thread, 9)).toBe(false);
    expect(beginStopping(thread, 10)).toBe(true);
    expect(beginStopping(thread, 10)).toBe(false);
    expect(stopFailed(thread, 10)).toBe(true);
    expect(beginStopping(thread, 10)).toBe(true);
    expect(cancelQuestion(thread, 10)).toBe(true);
    expect(retryQuestion(thread, 11, "operation-11")).toBe(true);
  });

  test("rejects questions that would exceed the server thread limits", () => {
    const full = Array.from({ length: 63 }, (_, index) => ({
      role: index % 2 === 0 ? "reviewer" as const : "agent" as const,
      body: "message",
    }));

    expect(questionValidationError(full, "Another question")).toContain("full");
    expect(questionValidationError([], "🙂".repeat(70_000))).toContain("too large");
    expect(questionValidationError([], "Small question")).toBeUndefined();
  });
});
