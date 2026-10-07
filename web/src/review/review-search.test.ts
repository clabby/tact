import { describe, expect, test } from "bun:test";
import { parseReviewPatch } from "./review-diff";
import { moveSearchTarget, searchReview } from "./review-search";

describe("review search", () => {
  test("handles empty queries and displayed rename paths", () => {
    const patch = [
      "diff --git a/src/NameName.ts b/src/new-name.ts\n",
      "similarity index 100%\n",
      "rename from src/NameName.ts\n",
      "rename to src/new-name.ts\n",
    ].join("");
    const files = parseReviewPatch(patch, "paths", false);

    expect(searchReview(files, " ")).toEqual({ count: 0 });
    expect(matches(files, "nAmE")).toEqual([
      { kind: "path", itemId: "src/new-name.ts" },
    ]);
  });

  test("returns one navigable match per row with actual line numbers", () => {
    const patch = [
      "diff --git a/example.txt b/example.txt\n",
      "index 1111111..2222222 100644\n",
      "--- a/example.txt\n",
      "+++ b/example.txt\n",
      "@@ -40,2 +70,3 @@\n",
      "-needle NEEDLE needle\n",
      "+İ Needle then nEeDlE İ\n",
      "+İ value\n",
      " context NEEDLE twice needle\n",
    ].join("");
    const files = parseReviewPatch(patch, "rows", false);

    expect(matches(files, "NeEdLe").map(summary)).toEqual([
      ["deletions", 40],
      ["additions", 70],
      ["additions", 72],
    ]);
    expect(searchReview(files, "NeEdLe").match).toMatchObject({ start: 0 });
    expect(searchReview(files, "NeEdLe", 1).match).toMatchObject({ start: 2, length: 6 });
    expect(searchReview(files, "NeEdLe", 2).match).toMatchObject({ start: 8 });
    expect(searchReview(files, "i\u0307").match).toMatchObject({ start: 0, length: 1 });
    expect(searchReview(files, "i\u0307", 0, 1).match).toMatchObject({
      start: 21,
      length: 1,
      occurrenceIndex: 1,
      occurrenceCount: 2,
    });
  });

  test("navigates every occurrence on the active row without indexing them globally", () => {
    const patch = [
      "diff --git a/example.txt b/example.txt\n",
      "index 1111111..2222222 100644\n",
      "--- a/example.txt\n",
      "+++ b/example.txt\n",
      "@@ -1,2 +1,2 @@\n",
      "-needle NEEDLE needle\n",
      "+needle elsewhere\n",
      " context needle\n",
    ].join("");
    const files = parseReviewPatch(patch, "occurrences", false);
    const first = searchReview(files, "needle");

    expect(first.count).toBe(3);
    expect(first.match).toMatchObject({ start: 0, occurrenceIndex: 0, occurrenceCount: 3 });
    const second = searchReview(files, "needle", 0, 1).match;
    expect(second).toMatchObject({
      start: 7,
      occurrenceIndex: 1,
      occurrenceCount: 3,
    });
    const last = searchReview(files, "needle", 0, Number.MAX_SAFE_INTEGER).match;
    expect(last).toMatchObject({ start: 14, occurrenceIndex: 2, occurrenceCount: 3 });
    expect(moveSearchTarget(first.match, 0, first.count, 1)).toEqual([0, 1]);
    expect(moveSearchTarget(second, 0, first.count, -1)).toEqual([0, 0]);
    expect(moveSearchTarget(last, 0, first.count, 1)).toEqual([1, 0]);
    expect(moveSearchTarget(searchReview(files, "needle", 1).match, 1, first.count, -1)).toEqual([
      0,
      Number.MAX_SAFE_INTEGER,
    ]);
    expect(moveSearchTarget(first.match, 0, first.count, -1)).toEqual([
      2,
      Number.MAX_SAFE_INTEGER,
    ]);
  });

  test("walks partial hunks independently of packed line indexes", () => {
    const patch = [
      "diff --git a/example.txt b/example.txt\n",
      "index 1111111..2222222 100644\n",
      "--- a/example.txt\n",
      "+++ b/example.txt\n",
      "@@ -10,2 +20,2 @@\n",
      "-first needle in old file\n",
      "+first needle in new file\n",
      " shared first context\n",
      "@@ -100,3 +200,4 @@\n",
      " shared second context\n",
      "-second needle in old file\n",
      "+second needle in new file\n",
      "+extra needle in new file\n",
      " shared final context\n",
    ].join("");
    const [file] = parseReviewPatch(patch, "partial", false);

    expect(file.isPartial).toBeTrue();
    expect(matches([file], "needle").map(summary)).toEqual([
      ["deletions", 10],
      ["additions", 20],
      ["deletions", 101],
      ["additions", 201],
      ["additions", 202],
    ]);
  });

  test("excludes collapsed unchanged context from full files", () => {
    const original = Array.from({ length: 20 }, (_, index) => `line ${index + 1}\n`);
    original[0] = "collapsed target context\n";
    original[6] = "displayed target context\n";
    original[9] = "old target value\n";
    const patch = [
      "diff --git a/example.txt b/example.txt\n",
      "index 1111111..2222222 100644\n",
      "--- a/example.txt\n",
      "+++ b/example.txt\n",
      "@@ -1,20 +1,20 @@\n",
      ...original.map((line, index) => index === 9 ? `-${line}+new target value\n` : ` ${line}`),
    ].join("");
    const [file] = parseReviewPatch(patch, "full", true);

    expect(file.hunks[0].collapsedBefore).toBe(6);
    expect(matches([file], "target").map(summary)).toEqual([
      ["additions", 7],
      ["deletions", 10],
      ["additions", 10],
    ]);
  });
});

function matches(files: ReturnType<typeof parseReviewPatch>, query: string) {
  const { count } = searchReview(files, query);
  return Array.from({ length: count }, (_, index) => searchReview(files, query, index).match!);
}

function summary(match: ReturnType<typeof matches>[number]) {
  if (match.kind === "path") return ["path"];
  return [match.side, match.lineNumber];
}
