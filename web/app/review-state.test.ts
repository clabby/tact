import { describe, expect, test } from "bun:test";
import type { ReviewPage, ReviewSession } from "./protocol";
import { createQuestionThread } from "./question-state";
import {
  activatePage,
  createReviewState,
  currentFeedback,
  currentQuestions,
  discardCurrentFeedback,
  feedbackOwner,
  rebaseState,
  replaceQuestions,
} from "./review-state";

const firstPage = page(3, { from: 0, to: 2 });
const session: ReviewSession = {
  protocol_version: 9,
  turn_running: false,
  generation: 3,
  title: "Review",
  repository: "tact",
  trunk: "main",
  range_targets: [],
  default_range: firstPage.selected_range,
  page: firstPage,
  overview: null,
  questions: [],
};

describe("review state transitions", () => {
  test("feedback and questions are isolated by generation and range", () => {
    let state = createReviewState(session);
    currentFeedback(state).summary = "first summary";
    currentFeedback(state).seenPaths.add("src/main.rs");
    currentQuestions(state).push(createQuestionThread("thread-1", {
      itemId: "item",
      range: { from: 0, to: 2 },
      path: "src/main.rs",
      side: "additions",
      startLine: 1,
      endLine: 1,
    }, "Why?", 1, "operation-1"));

    state = activatePage(state, page(3, { from: 1, to: 2 }));
    expect(currentFeedback(state).summary).toBe("");
    expect(currentFeedback(state).seenPaths.size).toBe(0);
    expect(currentQuestions(state)).toEqual([]);

    state = activatePage(state, firstPage);
    expect(currentFeedback(state).summary).toBe("first summary");
    expect(currentFeedback(state).seenPaths).toContain("src/main.rs");
    expect(currentQuestions(state)).toHaveLength(1);
    expect(state.feedbackByOwner.has(feedbackOwner(3, { from: 0, to: 2 }))).toBe(true);
  });

  test("discard clears feedback without removing question threads", () => {
    let state = createReviewState(session);
    const feedback = currentFeedback(state);
    feedback.summary = "summary";
    feedback.seenPaths.add("README.md");
    feedback.draft = {
      itemId: "item", path: "README.md", side: "additions", startLine: 1,
      endLine: 1, body: "draft", tab: "comment",
    };
    currentQuestions(state).push(createQuestionThread("thread-1", {
      itemId: "item", range: { from: 0, to: 2 }, path: "README.md",
      side: "additions", startLine: 1, endLine: 1,
    }, "Why?", 1, "operation-1"));

    state = discardCurrentFeedback(state);
    expect(currentFeedback(state)).toEqual({ summary: "", comments: [], seenPaths: new Set() });
    expect(currentQuestions(state)).toHaveLength(1);
  });

  test("a newer snapshot keeps the reviewer's feedback and replaces the session's questions", () => {
    const stale = createReviewState(session);
    currentFeedback(stale).summary = "kept";
    const carried = createQuestionThread("carried", {
      itemId: "item", range: { from: 0, to: 2 }, path: "a.rs", side: "additions", startLine: 1, endLine: 1,
    }, "Why?", 1, "op");
    const next = page(4, { from: 0, to: 2 });
    const state = rebaseState({ ...session, generation: 4, page: next }, next, currentFeedback(stale), [carried]);
    expect(currentFeedback(state).summary).toBe("kept");
    expect(currentQuestions(state).map((thread) => thread.id)).toEqual(["carried"]);
    expect(state.feedbackByOwner.has(feedbackOwner(3, { from: 0, to: 2 }))).toBe(false);
  });

  test("switching session clears question threads of every range but keeps feedback", () => {
    let state = createReviewState(session);
    currentFeedback(state).summary = "kept";
    currentQuestions(state).push(createQuestionThread("old", {
      itemId: "item", range: { from: 0, to: 2 }, path: "a.rs", side: "additions", startLine: 1, endLine: 1,
    }, "Why?", 1, "op"));
    state = replaceQuestions(state, {
      ...session,
      questions: [{
        thread_id: "theirs", operation_id: "op2", generation: 3, range: { from: 0, to: 2 }, path: "b.rs",
        side: "additions", start_line: 2, end_line: 2, messages: [{ role: "reviewer", body: "Hi" }], status: "idle",
      }],
    });
    expect(currentFeedback(state).summary).toBe("kept");
    expect(currentQuestions(state).map((thread) => thread.id)).toEqual(["theirs"]);
  });


  test("restores an in-progress question from the review session", () => {
    const state = createReviewState({
      ...session,
      questions: [{
        thread_id: "thread-1",
        operation_id: "operation-1",
        generation: 3,
        range: { from: 0, to: 2 },
        path: "src/main.rs",
        side: "additions",
        start_line: 4,
        end_line: 8,
        messages: [{ role: "reviewer", body: "Why?" }],
        status: "asking",
      }],
    });

    expect(currentQuestions(state)).toHaveLength(1);
    expect(currentQuestions(state)[0].turn).toEqual({
      kind: "asking",
      request: 0,
      operationId: "operation-1",
      stopping: false,
    });
  });
});

function page(generation: number, selected_range: { from: number; to: number }): ReviewPage {
  return {
    generation,
    selected_range,
    patch: "",
    repository: "tact",
    scope: "Full branch",
    base: "main",
  };
}
