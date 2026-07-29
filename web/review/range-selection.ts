export type ReviewRange = { from: number; to: number };
export type RangeBoundary = "from" | "to";

export type ReviewTarget = {
  index: number;
  kind: "trunk" | "commit" | "working_tree";
  short_id: string;
  title: string;
};

export function rangeKey(range: ReviewRange) {
  return `${range.from}:${range.to}`;
}

export function rangesEqual(left: ReviewRange | undefined, right: ReviewRange | undefined) {
  return left?.from === right?.from && left?.to === right?.to;
}

export function rangeLabel(targets: ReviewTarget[], range: ReviewRange) {
  if (range.from === targets.length - 2 && range.to === targets.length - 1) {
    return "Uncommitted changes";
  }
  if (range.from === 0 && range.to === targets.length - 1) {
    return "Full branch";
  }
  return `${targetLabel(targets[range.from])} → ${targetLabel(targets[range.to])}`;
}

export function targetLabel(target: ReviewTarget) {
  return target.kind === "working_tree" ? "Working tree" : target.short_id;
}

export function expandRange(range: ReviewRange, targetIndex: number): ReviewRange {
  if (targetIndex < range.from) return { from: targetIndex, to: range.to };
  if (targetIndex > range.to) return { from: range.from, to: targetIndex };
  return range;
}

export function moveRangeBoundary(
  range: ReviewRange,
  boundary: RangeBoundary,
  targetIndex: number,
): ReviewRange {
  if (boundary === "from" && targetIndex < range.to) {
    return targetIndex === range.from ? range : { from: targetIndex, to: range.to };
  }
  if (boundary === "to" && targetIndex > range.from) {
    return targetIndex === range.to ? range : { from: range.from, to: targetIndex };
  }
  return range;
}
