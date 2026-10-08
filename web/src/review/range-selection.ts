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

export type RangePreset = {
  id: "branch" | "latest" | "uncommitted";
  label: string;
  range: ReviewRange;
};

/**
 * The one-click ranges: everything on the branch, the newest commit alone, and the uncommitted
 * work. Targets run from the trunk through the commits to the working tree, and a range reviews
 * the changes after `from` up to and including `to`. A preset that would repeat an earlier one
 * (a branch with no commits) is dropped.
 */
export function rangePresets(targets: ReviewTarget[]): RangePreset[] {
  const last = targets.length - 1;
  const candidates: (RangePreset | null)[] = [
    last >= 1 ? { id: "branch", label: "Full branch", range: { from: 0, to: last } } : null,
    last >= 2 ? { id: "latest", label: "Latest commit", range: { from: last - 2, to: last - 1 } } : null,
    last >= 1 ? { id: "uncommitted", label: "Uncommitted", range: { from: last - 1, to: last } } : null,
  ];
  const presets: RangePreset[] = [];
  for (const candidate of candidates) {
    if (candidate && !presets.some((preset) => rangesEqual(preset.range, candidate.range))) presets.push(candidate);
  }
  return presets;
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
