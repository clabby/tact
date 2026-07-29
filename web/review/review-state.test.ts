import { describe, expect, test } from "bun:test";
import type { ReviewPage, ReviewSession } from "./protocol";
import {
  activatePage,
  beginTerminal,
  createReviewState,
  currentFeedback,
  discardCurrentFeedback,
  feedbackOwner,
  finishTerminal,
} from "./review-state";

const firstPage = page(3, { from: 0, to: 2 });
const session: ReviewSession = {
  protocol_version: 1,
  generation: 3,
  title: "Review",
  repository: "tact",
  trunk: "main",
  range_targets: [],
  default_range: firstPage.selected_range,
  page: firstPage,
};

describe("review state transitions", () => {
  test("feedback is isolated by generation and range", () => {
    let state = createReviewState(session);
    currentFeedback(state).summary = "first summary";
    currentFeedback(state).seenPaths.add("src/main.rs");

    state = activatePage(state, page(3, { from: 1, to: 2 }));
    expect(currentFeedback(state).summary).toBe("");
    expect(currentFeedback(state).seenPaths.size).toBe(0);

    state = activatePage(state, firstPage);
    expect(currentFeedback(state).summary).toBe("first summary");
    expect(currentFeedback(state).seenPaths).toContain("src/main.rs");
    expect(state.feedbackByOwner.has(feedbackOwner(3, { from: 0, to: 2 }))).toBe(true);
  });

  test("discard clears summary, comments, draft, and seen state together", () => {
    let state = createReviewState(session);
    const feedback = currentFeedback(state);
    feedback.summary = "summary";
    feedback.seenPaths.add("README.md");
    feedback.draft = {
      itemId: "item", path: "README.md", side: "additions", startLine: 1,
      endLine: 1, body: "draft", tab: "comment",
    };

    state = discardCurrentFeedback(state);
    expect(currentFeedback(state)).toEqual({ summary: "", comments: [], seenPaths: new Set() });
  });

  test("terminal actions are idempotent while busy and after completion", () => {
    const idle = createReviewState(session);
    const busy = beginTerminal(idle, "submit");
    expect(beginTerminal(busy, "cancel")).toBe(busy);
    const finished = finishTerminal(busy, "submit");
    expect(beginTerminal(finished, "submit")).toBe(finished);
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
