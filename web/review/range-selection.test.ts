import { describe, expect, test } from "bun:test";
import {
  canSelectTarget,
  rangeKey,
  rangeLabel,
  rangesEqual,
  selectTarget,
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

  test("each endpoint can move without crossing the other", () => {
    const range = { from: 1, to: 3 };
    expect(canSelectTarget(range, "from", 2)).toBe(true);
    expect(canSelectTarget(range, "from", 3)).toBe(false);
    expect(canSelectTarget(range, "to", 2)).toBe(true);
    expect(canSelectTarget(range, "to", 1)).toBe(false);
    expect(selectTarget(range, "from", 2)).toEqual({ from: 2, to: 3 });
    expect(selectTarget(range, "to", 1)).toBe(range);
  });

  test("range identity is stable across decoded objects", () => {
    expect(rangeKey({ from: 1, to: 3 })).toBe("1:3");
    expect(rangesEqual({ from: 1, to: 3 }, { from: 1, to: 3 })).toBe(true);
  });
});
