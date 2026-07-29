import { expect, test } from "bun:test";
import { parsePatchFiles } from "@pierre/diffs";
import { reviewBootstrap, reviewFixtures } from "./dev-fixture";

test("standalone review fixtures contain valid patches", () => {
  for (const fixture of Object.values(reviewFixtures)) {
    expect(() => parsePatchFiles(fixture.patch, "dev-fixture", true)).not.toThrow();
  }
  expect(reviewFixtures[reviewBootstrap.default_scope].selected_scope).toBe("uncommitted");
  expect(reviewFixtures.uncommitted).not.toHaveProperty("overview_html");
});
