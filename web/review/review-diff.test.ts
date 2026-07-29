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

  const [file] = parseReviewPatch(patch, "test");
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
