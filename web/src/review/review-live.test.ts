import { describe, expect, test } from "bun:test";
import type { FileDiffMetadata } from "@pierre/diffs";
import type { QuestionThread } from "./question-state";
import { carryFeedback, carryQuestions, changedFileNames } from "./review-live";
import { parseReviewPatch } from "./review-diff";
import type { CommentMetadata, FeedbackState } from "./review-state";

type Edit = { at: number; remove?: number; add?: string[] };

const lines = (count: number) => Array.from({ length: count }, (_, index) => "line " + (index + 1));

/** A whole-file patch for \`name\`: \`edits\` replace \`remove\` lines at the 1-based line \`at\`. */
function filePatch(name: string, before: string[], edits: Edit[]) {
  const body: string[] = [];
  let oldCount = 0;
  let newCount = 0;
  let index = 0;
  for (const edit of [...edits].sort((left, right) => left.at - right.at)) {
    while (index + 1 < edit.at) {
      body.push(" " + before[index++]);
      oldCount++;
      newCount++;
    }
    for (let removed = 0; removed < (edit.remove ?? 0); removed++) {
      body.push("-" + before[index++]);
      oldCount++;
    }
    for (const added of edit.add ?? []) {
      body.push("+" + added);
      newCount++;
    }
  }
  for (; index < before.length; index++) {
    body.push(" " + before[index]);
    oldCount++;
    newCount++;
  }
  return [
    "diff --git a/" + name + " b/" + name + "\n",
    "index 1111111..2222222 100644\n",
    "--- a/" + name + "\n",
    "+++ b/" + name + "\n",
    "@@ -1," + oldCount + " +1," + newCount + " @@\n",
    ...body.map((line) => line + "\n"),
  ].join("");
}

const parse = (patch: string): FileDiffMetadata[] => parseReviewPatch(patch, "test", true);
const first = [{ at: 30, remove: 1, add: ["changed line 30"] }];
const comment = (overrides: Partial<CommentMetadata>): CommentMetadata => ({
  id: 1, itemId: "a.txt", path: "a.txt", side: "additions", start_line: 31, end_line: 31, body: "note", ...overrides,
});
const feedback = (overrides: Partial<FeedbackState>): FeedbackState => ({
  summary: "", comments: [], seenPaths: new Set(), ...overrides,
});
const carry = (before: FileDiffMetadata[], after: FileDiffMetadata[], state: FeedbackState) =>
  carryFeedback(before, after, state, () => 99);

describe("carrying feedback across diff snapshots", () => {
  const before = parse(filePatch("a.txt", lines(60), first));

  test("a comment follows its line when edits elsewhere shift it", () => {
    const after = parse(filePatch("a.txt", lines(60), [{ at: 10, add: ["new 1", "new 2"] }, ...first]));
    const carried = carry(before, after, feedback({ comments: [comment({})] }));
    expect(carried.comments).toEqual([comment({ start_line: 33, end_line: 33 })]);
  });

  test("a comment on edited lines becomes an outdated comment that keeps its text", () => {
    const after = parse(filePatch("a.txt", lines(60), [{ at: 30, remove: 1, add: ["changed again"] }]));
    const carried = carry(before, after, feedback({
      comments: [comment({ start_line: 30, end_line: 30, body: "why?" })],
    }));
    expect(carried.comments).toEqual([
      comment({ itemId: "", outdated: true, start_line: 30, end_line: 30, body: "why?" }),
    ]);
  });

  test("a comment on a file that left the diff is outdated", () => {
    const carried = carry(before, [], feedback({ comments: [comment({})] }));
    expect(carried.comments[0].outdated).toBe(true);
  });

  test("seen marks survive only on files whose diff is unchanged", () => {
    const both = (second: Edit[]) => parse(
      filePatch("a.txt", lines(60), first) + filePatch("b.txt", lines(60), second),
    );
    const previous = both([{ at: 5, remove: 1, add: ["b edit"] }]);
    const next = both([{ at: 5, remove: 1, add: ["b edit, revised"] }]);
    const carried = carry(previous, next, feedback({ seenPaths: new Set(["a.txt", "b.txt", "gone.txt"]) }));
    expect([...carried.seenPaths]).toEqual(["a.txt"]);
    expect(changedFileNames(previous, next)).toEqual(new Set(["b.txt"]));
  });

  test("an open draft follows its line or is preserved as an outdated comment", () => {
    const draft = {
      itemId: "a.txt", path: "a.txt", side: "additions" as const, startLine: 31, endLine: 31,
      body: "half written", tab: "comment" as const,
    };
    const shifted = parse(filePatch("a.txt", lines(60), [{ at: 10, add: ["new"] }, ...first]));
    expect(carry(before, shifted, feedback({ draft })).draft).toMatchObject({ startLine: 32, body: "half written" });

    const edited = parse(filePatch("a.txt", lines(60), [{ at: 30, remove: 2, add: ["rewritten"] }]));
    const carried = carry(before, edited, feedback({ draft }));
    expect(carried.draft).toBeUndefined();
    expect(carried.comments).toEqual([
      comment({ id: 99, itemId: "", outdated: true, body: "half written" }),
    ]);
  });

  test("editing an outdated comment folds the draft back into that comment", () => {
    const edited = parse(filePatch("a.txt", lines(60), [{ at: 30, remove: 2, add: ["rewritten"] }]));
    const draft = {
      itemId: "a.txt", path: "a.txt", side: "additions" as const, startLine: 31, endLine: 31,
      body: "revised", tab: "comment" as const, editingId: 1,
    };
    const carried = carry(before, edited, feedback({ comments: [comment({})], draft }));
    expect(carried.draft).toBeUndefined();
    expect(carried.comments).toEqual([comment({ itemId: "", outdated: true, body: "revised" })]);
  });

  test("question threads follow their code and are dropped with it", () => {
    const thread = (id: string, line: number): QuestionThread => ({
      id, itemId: "a.txt", range: { from: 0, to: 1 }, path: "a.txt", side: "additions",
      startLine: line, endLine: line, messages: [], draft: "", turn: { kind: "idle" },
    });
    const after = parse(filePatch("a.txt", lines(60), [
      { at: 10, add: ["new"] },
      { at: 30, remove: 1, add: ["changed again"] },
    ]));
    const carried = carryQuestions(before, after, [thread("kept", 31), thread("edited", 30)]);
    expect(carried.map(({ id, startLine }) => [id, startLine])).toEqual([["kept", 32]]);
  });
});
