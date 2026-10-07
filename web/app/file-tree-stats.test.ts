import { describe, expect, test } from "bun:test";
import { fileTreeChangeStats } from "./file-tree-stats";

describe("file tree change statistics", () => {
  test("counts each file", () => {
    const stats = fileTreeChangeStats([
      file("src/review/app.ts", [[4, 2], [3, 1]]),
    ]);

    expect(stats.get("src/review/app.ts")).toEqual({ additions: 7, deletions: 3 });
  });

  test("aggregates every descendant into its folders", () => {
    const stats = fileTreeChangeStats([
      file("src/review/app.ts", [[4, 2]]),
      file("src/review/styles.css", [[3, 1]]),
      file("tests/review.ts", [[2, 5]]),
    ]);

    expect(stats.get("src/review/")).toEqual({ additions: 7, deletions: 3 });
    expect(stats.get("src/")).toEqual({ additions: 7, deletions: 3 });
    expect(stats.get("tests/")).toEqual({ additions: 2, deletions: 5 });
  });
});

function file(name: string, changes: Array<[number, number]>) {
  return {
    name,
    hunks: changes.map(([additionLines, deletionLines]) => ({
      additionLines,
      deletionLines,
    })),
  };
}
