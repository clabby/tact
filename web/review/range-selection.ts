export type ReviewRange = { from: number; to: number };
export type RangeEndpoint = "from" | "to";

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

export function canSelectTarget(
  range: ReviewRange,
  endpoint: RangeEndpoint,
  targetIndex: number,
) {
  return endpoint === "from" ? targetIndex < range.to : targetIndex > range.from;
}

export function selectTarget(
  range: ReviewRange,
  endpoint: RangeEndpoint,
  targetIndex: number,
): ReviewRange {
  if (!canSelectTarget(range, endpoint, targetIndex)) return range;
  return endpoint === "from"
    ? { from: targetIndex, to: range.to }
    : { from: range.from, to: targetIndex };
}
