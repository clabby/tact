import { expect, test } from "bun:test";
import { parseApplyPatch, patchFileDiff, patchStats, toUnifiedDiff } from "./patch";

const envelope = [
  "*** Begin Patch",
  "*** Update File: src/main.rs",
  "@@ fn main()",
  " fn main() {",
  '-    println!("hello");',
  '+    println!("hello, tact");',
  " }",
  "*** Add File: notes.md",
  "+one",
  "+two",
  "*** Delete File: old.txt",
  "*** End Patch",
].join("\n");

test("parses each file operation with its hunks", () => {
  const files = parseApplyPatch(envelope);
  expect(files.map((file) => [file.kind, file.path, file.hunks.length])).toEqual([
    ["update", "src/main.rs", 1],
    ["add", "notes.md", 1],
    ["delete", "old.txt", 0],
  ]);
  expect(patchStats(files)).toEqual({ additions: 3, deletions: 1 });
});

test("renders a unified diff that Pierre accepts", () => {
  const [update, add] = parseApplyPatch(envelope);
  expect(toUnifiedDiff(update!)).toContain("@@ -1,3 +1,3 @@ fn main()");
  expect(toUnifiedDiff(add!)).toContain("@@ -0,0 +1,2 @@");
  expect(patchFileDiff(update!, "t")?.name).toBe("src/main.rs");
  expect(patchFileDiff(add!, "t")?.type).toBe("new");
});

test("deletions and malformed envelopes have no diff", () => {
  const [, , removal] = parseApplyPatch(envelope);
  expect(patchFileDiff(removal!, "t")).toBeNull();
  expect(parseApplyPatch("not a patch")).toEqual([]);
});
