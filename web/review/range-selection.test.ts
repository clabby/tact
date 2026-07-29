import { describe, expect, test } from "bun:test";
import {
  rangeKey,
  rangeLabel,
  resizeRange,
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

  test("clicking outside the range widens its nearest boundary", () => {
    const range = { from: 2, to: 5 };

    expect(resizeRange(range, 0)).toEqual({ from: 0, to: 5 });
    expect(resizeRange(range, 7)).toEqual({ from: 2, to: 7 });
  });

  test("clicking inside the range shrinks its nearest boundary", () => {
    const range = { from: 2, to: 6 };

    expect(resizeRange(range, 3)).toEqual({ from: 3, to: 6 });
    expect(resizeRange(range, 5)).toEqual({ from: 2, to: 5 });
    expect(resizeRange(range, 4)).toEqual({ from: 4, to: 6 });
  });

  test("clicking a boundary never crosses or collapses the range", () => {
    const range = { from: 2, to: 3 };

    expect(resizeRange(range, 2)).toBe(range);
    expect(resizeRange(range, 3)).toBe(range);
  });

  test("range identity is stable across decoded objects", () => {
    expect(rangeKey({ from: 1, to: 3 })).toBe("1:3");
    expect(rangesEqual({ from: 1, to: 3 }, { from: 1, to: 3 })).toBe(true);
  });
});
