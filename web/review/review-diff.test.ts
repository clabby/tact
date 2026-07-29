import { expect, test } from "bun:test";
import { parseReviewPatch } from "./review-diff";

test("changed files start collapsed to their diff hunks", () => {
  const original = Array.from({ length: 40 }, (_, index) => `line ${index + 1}\n`);
  const changed = [...original];
  changed[19] = "changed line 20\n";
  const patch = [
    "diff --git a/example.txt b/example.txt\n",
    "index 1111111..2222222 100644\n",
    "--- a/example.txt\n",
    "+++ b/example.txt\n",
    "@@ -1,40 +1,40 @@\n",
    ...original.slice(0, 19).map((line) => ` ${line}`),
    `-${original[19]}`,
    `+${changed[19]}`,
    ...original.slice(20).map((line) => ` ${line}`),
  ].join("");

  const [file] = parseReviewPatch(patch, "test", true);
  const [hunk] = file.hunks;

  expect(file.isPartial).toBeFalse();
  expect(file.deletionLines).toEqual(original);
  expect(file.additionLines).toEqual(changed);
  expect(hunk.deletionStart).toBe(17);
  expect(hunk.additionStart).toBe(17);
  expect(hunk.deletionCount).toBe(7);
  expect(hunk.additionCount).toBe(7);
  expect(hunk.collapsedBefore).toBe(16);
});

test("each file in a multi-file patch is trimmed independently", () => {
  const firstPatch = changedFilePatch("first.txt", 20);
  const secondPatch = changedFilePatch("second.txt", 30);

  expect(() => parseReviewPatch(firstPatch + secondPatch, "test", true)).not.toThrow();
});

test("a partial patch remains partial", () => {
  const patch = [
    "diff --git a/example.txt b/example.txt\n",
    "index 1111111..2222222 100644\n",
    "--- a/example.txt\n",
    "+++ b/example.txt\n",
    "@@ -18,3 +18,3 @@\n",
    " line 18\n",
    "-line 19\n",
    "+changed line 19\n",
    " line 20\n",
  ].join("");

  const [file] = parseReviewPatch(patch, "test", false);

  expect(file.isPartial).toBeTrue();
});

test("a partial patch beginning at the first line remains partial", () => {
  const patch = [
    "diff --git a/example.txt b/example.txt\n",
    "index 1111111..2222222 100644\n",
    "--- a/example.txt\n",
    "+++ b/example.txt\n",
    "@@ -1,3 +1,3 @@\n",
    " line 1\n",
    "-line 2\n",
    "+changed line 2\n",
    " line 3\n",
  ].join("");

  const [file] = parseReviewPatch(patch, "test", false);

  expect(file.isPartial).toBeTrue();
});

function changedFilePatch(name: string, changedLine: number) {
  const lines = Array.from({ length: 40 }, (_, index) => `line ${index + 1}\n`);
  return [
    `diff --git a/${name} b/${name}\n`,
    "index 1111111..2222222 100644\n",
    `--- a/${name}\n`,
    `+++ b/${name}\n`,
    "@@ -1,40 +1,40 @@\n",
    ...lines.map((line, index) => {
      if (index !== changedLine - 1) return ` ${line}`;
      return `-${line}+changed ${line}`;
    }),
  ].join("");
}
