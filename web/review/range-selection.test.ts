import { describe, expect, test } from "bun:test";
import {
  expandRange,
  moveRangeBoundary,
  rangeKey,
  rangeLabel,
  rangesEqual,
  type ReviewTarget,
} from "./range-selection";

const targets: ReviewTarget[] = [
  { index: 0, kind: "trunk", short_id: "a000000", title: "main · Base" },
  { index: 1, kind: "commit", short_id: "b111111", title: "First change" },
  { index: 2, kind: "commit", short_id: "c222222", title: "Second change" },
  { index: 3, kind: "working_tree", short_id: "WT", title: "Uncommitted changes" },
];

describe("commit range selection", () => {
  test("labels presets and arbitrary commit intervals", () => {
    expect(rangeLabel(targets, { from: 2, to: 3 })).toBe("Uncommitted changes");
    expect(rangeLabel(targets, { from: 0, to: 3 })).toBe("Full branch");
    expect(rangeLabel(targets, { from: 1, to: 2 })).toBe("b111111 → c222222");
  });

  test("clicking outside the range widens the unambiguous boundary", () => {
    const range = { from: 2, to: 5 };

    expect(expandRange(range, 0)).toEqual({ from: 0, to: 5 });
    expect(expandRange(range, 7)).toEqual({ from: 2, to: 7 });
  });

  test("clicking inside the range does not guess which boundary to move", () => {
    const range = { from: 2, to: 6 };

    expect(expandRange(range, 3)).toBe(range);
    expect(expandRange(range, 5)).toBe(range);
  });

  test("explicit boundary moves can shrink without crossing", () => {
    const range = { from: 2, to: 6 };

    expect(moveRangeBoundary(range, "from", 4)).toEqual({ from: 4, to: 6 });
    expect(moveRangeBoundary(range, "to", 3)).toEqual({ from: 2, to: 3 });
    expect(moveRangeBoundary(range, "from", 6)).toBe(range);
    expect(moveRangeBoundary(range, "to", 2)).toBe(range);
  });

  test("range identity is stable across decoded objects", () => {
    expect(rangeKey({ from: 1, to: 3 })).toBe("1:3");
    expect(rangesEqual({ from: 1, to: 3 }, { from: 1, to: 3 })).toBe(true);
  });
});
