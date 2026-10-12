import { describe, expect, test } from "bun:test";
import type { FileDiffMetadata } from "@pierre/diffs";
import { parseReviewPatch } from "./review-diff";
import { SeenFiles } from "./seen-files";

function memoryStorage() {
  const values = new Map<string, string>();
  return {
    getItem: (key: string) => values.get(key) ?? null,
    setItem: (key: string, value: string) => void values.set(key, value),
  };
}

/** A one-hunk patch replacing a line of the named file with `added`. */
function diff(name: string, added: string): FileDiffMetadata[] {
  return parseReviewPatch([
    "diff --git a/" + name + " b/" + name + "\n",
    "index 1111111..2222222 100644\n",
    "--- a/" + name + "\n",
    "+++ b/" + name + "\n",
    "@@ -1,3 +1,3 @@\n",
    " one\n",
    "-two\n",
    "+" + added + "\n",
    " three\n",
  ].join(""), "test", true);
}

describe("seen files", () => {
  test("a mark survives a reload and ends when the file's diff changes", () => {
    const storage = memoryStorage();
    const [file] = diff("a.txt", "2");
    new SeenFiles(storage).mark("/work", file!, true);

    const reloaded = new SeenFiles(storage);
    expect(reloaded.among("/work", diff("a.txt", "2"))).toEqual(new Set(["a.txt"]));
    expect(reloaded.among("/work", diff("a.txt", "two!"))).toEqual(new Set());
    // Returning to the marked content shows the file as seen again.
    expect(reloaded.among("/work", diff("a.txt", "2"))).toEqual(new Set(["a.txt"]));
  });

  test("marks are per checkout and per file, and can be cleared", () => {
    const storage = memoryStorage();
    const seen = new SeenFiles(storage);
    const files = [...diff("a.txt", "2"), ...diff("b.txt", "2")];
    seen.mark("/work", files[0]!, true);
    seen.mark("/work", files[1]!, true);
    expect(seen.among("/other", files)).toEqual(new Set());

    seen.mark("/work", files[0]!, false);
    expect(new SeenFiles(storage).among("/work", files)).toEqual(new Set(["b.txt"]));
  });

  test("an unreadable store is treated as empty", () => {
    const storage = memoryStorage();
    storage.setItem("tact.web.review-seen", "{not json");
    expect(new SeenFiles(storage).among("/work", diff("a.txt", "2"))).toEqual(new Set());
  });

  test("a mark made in another tab is picked up, and so is its removal", () => {
    const storage = memoryStorage();
    const files = diff("a.txt", "2");
    const here = new SeenFiles(storage);
    const there = new SeenFiles(storage);
    expect(here.among("/work", files)).toEqual(new Set());

    there.mark("/work", files[0]!, true);
    expect(here.among("/work", files)).toEqual(new Set(["a.txt"]));
    there.mark("/work", files[0]!, false);
    expect(here.among("/work", files)).toEqual(new Set());
  });

  test("marks that cannot be stored hold for the life of the page", () => {
    const storage = { getItem: () => null, setItem: () => { throw new Error("quota"); } };
    const files = diff("a.txt", "2");
    const seen = new SeenFiles(storage);
    seen.mark("/work", files[0]!, true);
    expect(seen.among("/work", files)).toEqual(new Set(["a.txt"]));
    seen.mark("/work", files[0]!, false);
    expect(seen.among("/work", files)).toEqual(new Set());
  });

  test("a binary file or a mode change is unseen again when only its identity changes", () => {
    const binary = (id: string) => parseReviewPatch([
      "diff --git a/img.png b/img.png\n",
      "index 1111111.." + id + " 100644\n",
      "Binary files a/img.png and b/img.png differ\n",
    ].join(""), "test", true);
    const mode = (from: string, to: string) => parseReviewPatch([
      "diff --git a/run.sh b/run.sh\n",
      "old mode " + from + "\n",
      "new mode " + to + "\n",
    ].join(""), "test", true);
    const seen = new SeenFiles(memoryStorage());
    for (const [marked, later] of [
      [binary("2222222"), binary("3333333")],
      [mode("100644", "100755"), mode("100755", "100644")],
    ]) {
      seen.mark("/work", marked[0]!, true);
      expect(seen.among("/work", marked)).toEqual(new Set([marked[0]!.name]));
      expect(seen.among("/work", later)).toEqual(new Set());
    }
  });
});
