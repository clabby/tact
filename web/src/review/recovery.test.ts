import { expect, test } from "bun:test";

const read = (name: string) => Bun.file(new URL(name, import.meta.url)).text();
const panel = () => read("review-panel.ts");

test("a stale generation re-bootstraps instead of refreshing the stale generation", async () => {
  const app = await panel();
  const refresh = app.slice(app.indexOf("private async refreshReview"), app.indexOf("private async installSnapshot"));

  expect(refresh).toContain('errorCode(error) !== "stale_snapshot"');
  expect(refresh).toContain("this.api.review(this.session)");
});

test("range retry preserves the confirmed feedback discard", async () => {
  const app = await panel();
  const error = app.slice(app.indexOf("private showRangeError"), app.indexOf("private syncSelectedRange"));

  expect(error).toContain("selectRange(range, discardCurrentFeedback)");
});

test("responses for a previous session are ignored", async () => {
  const app = await panel();
  const change = app.slice(app.indexOf("private async sessionChanged"), app.indexOf("private async refreshReview"));

  for (const owner of ["overview", "aiReview", "questionThreads"]) {
    expect(change).toContain("this." + owner + ".reset()");
  }
  const resets = await Promise.all(["overview-panel.ts", "ai-review.ts", "question-threads.ts"].map(async (name) => {
    const source = await read(name);
    return source.slice(source.indexOf("\n  reset()"), source.indexOf("\n  }", source.indexOf("\n  reset()")));
  }));
  for (const [reset, invalidated] of [
    [resets[0], "overviewRequest++"],
    [resets[1], "aiReviewRequest++"],
    [resets[2], "questionRequest++"],
    [resets[2], "questionOperations.clear()"],
  ]) {
    expect(reset).toContain(invalidated);
  }
  expect(await read("question-threads.ts")).toContain("if (epoch !== this.deps.sessionEpoch()) return;");
});
