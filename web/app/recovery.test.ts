import { expect, test } from "bun:test";

const panel = () => Bun.file(new URL("review-panel.ts", import.meta.url)).text();

test("a stale generation re-bootstraps instead of refreshing the stale generation", async () => {
  const app = await panel();
  const refresh = app.slice(app.indexOf("private async refreshReview"), app.indexOf("private async installSnapshot"));

  expect(refresh).toContain('errorCode(error) !== "stale_snapshot"');
  expect(refresh).toContain("this.api.review()");
});

test("range retry preserves the confirmed feedback discard", async () => {
  const app = await panel();
  const error = app.slice(app.indexOf("private showRangeError"), app.indexOf("private syncSelectedRange"));

  expect(error).toContain("selectRange(range, discardCurrentFeedback)");
});

test("responses for a previous session are ignored", async () => {
  const app = await panel();
  const change = app.slice(app.indexOf("private async sessionChanged"), app.indexOf("private async refreshReview"));

  for (const invalidated of ["overviewRequest++", "aiReviewRequest++", "questionRequest++", "questionOperations.clear()"]) {
    expect(change).toContain(invalidated);
  }
  expect(app).toContain("if (epoch !== this.sessionEpoch) return;");
});
