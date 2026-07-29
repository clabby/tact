import { expect, test } from "bun:test";
import { parsePatchFiles } from "@pierre/diffs";
import { reviewBootstrap, reviewFixtures } from "./dev-fixture";
import { rangeKey } from "./range-selection";

test("standalone review fixtures contain valid patches", () => {
  for (const fixture of Object.values(reviewFixtures)) {
    expect(() => parsePatchFiles(fixture.patch, "dev-fixture", true)).not.toThrow();
  }
  expect(reviewFixtures[rangeKey(reviewBootstrap.default_range) as keyof typeof reviewFixtures].selected_range)
    .toEqual({ from: 0, to: 3 });
  expect(reviewFixtures["2:3"]).not.toHaveProperty("overview_html");
});

test("review bootstrap exposes every selectable commit-range endpoint", () => {
  expect(reviewBootstrap).toHaveProperty("range_targets");
  expect(reviewBootstrap.range_targets.map((target) => target.kind)).toEqual([
    "trunk",
    "commit",
    "commit",
    "working_tree",
  ]);
});
