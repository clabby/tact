import { expect, test } from "bun:test";
import { rangeOptions } from "./range-selection";

test("range choices distinguish the working tree from the full branch", () => {
  expect(rangeOptions("main")).toEqual([
    {
      scope: "uncommitted",
      label: "Uncommitted changes",
      description: "Review only the edits currently in your working tree.",
      from: "HEAD",
      to: "Working tree",
      recommended: true,
    },
    {
      scope: "full_branch",
      label: "Full branch",
      description: "Review every committed and uncommitted change on this branch.",
      from: "main",
      to: "Working tree",
      recommended: false,
    },
  ]);
});
