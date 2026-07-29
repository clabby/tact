import { expect, test } from "bun:test";
import { parsePatchFiles } from "@pierre/diffs";
import { addExpandableContext } from "./diff-context";

test("full file contents never replace Git's authoritative hunks", () => {
  const oldLines = Array.from({ length: 40 }, (_, index) => `line ${index + 1}`);
  const newLines = [...oldLines];
  newLines[19] = "changed line";
  const patch = `diff --git a/example.txt b/example.txt
index 1111111..2222222 100644
--- a/example.txt
+++ b/example.txt
@@ -17,7 +17,7 @@
 line 17
 line 18
 line 19
-line 20
+changed line
 line 21
 line 22
 line 23
`;
  const partial = parsePatchFiles(patch, "test", true)[0]!.files[0]!;

  const [expandable] = addExpandableContext(
    [partial],
    [{
      old_path: "example.txt",
      new_path: "example.txt",
      old_contents: `${oldLines.join("\n")}\n`,
      new_contents: `${newLines.join("\n")}\n`,
    }],
  );

  expect(partial.isPartial).toBe(true);
  expect(expandable).toBe(partial);
  expect(expandable!.isPartial).toBe(true);
  expect(expandable!.hunks).toEqual(partial.hunks);
});

test("files without context retain their patch representation", () => {
  const file = parsePatchFiles(`diff --git a/example.txt b/example.txt
index 1111111..2222222 100644
--- a/example.txt
+++ b/example.txt
@@ -1 +1 @@
-old
+new
`, "test", true)[0]!.files[0]!;

  expect(addExpandableContext([file], [])[0]).toBe(file);
});
