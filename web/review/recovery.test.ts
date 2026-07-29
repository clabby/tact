import { expect, test } from "bun:test";

test("a stale browser generation re-bootstraps instead of refreshing the stale generation", async () => {
  const app = await Bun.file(new URL("app.ts", import.meta.url)).text();
  const refresh = app.slice(
    app.indexOf("private async refreshReview"),
    app.indexOf("private bindSettings"),
  );

  expect(refresh).toContain("this.api.review()");
});

test("range retry preserves the confirmed feedback discard", async () => {
  const app = await Bun.file(new URL("app.ts", import.meta.url)).text();
  const error = app.slice(
    app.indexOf("private showRangeError"),
    app.indexOf("private syncSelectedRange"),
  );

  expect(error).toContain("selectRange(range, discardCurrentFeedback)");
});
