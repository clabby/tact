import { expect, test } from "bun:test";
import { resultText, summaryFailed, turnMarkdown, turnOutcome } from "./outcome";
import { planTurn, segmentTurns } from "./turns";
import { transcript } from "./test-entries";
import type { ToolEntry } from "./exploration";

const failedTests = { exit_code: 101, tail: ["FAIL a::b", "Summary 18 tests run: 17 passed, 1 failed"], summary: "17 passed, 1 failed" };

test("a turn's outcome sums its patches by distinct file and keeps the last test summary", () => {
  const t = transcript();
  t.user("fix it", 1_000);
  t.tool("apply_patch", "a.rs, b.rs", { stats: { files: 2, additions: 10, deletions: 2 } });
  t.tool("exec_command", "cargo test", { state: "failed", outcome: failedTests });
  t.tool("apply_patch", "a.rs", { stats: { files: 1, additions: 3, deletions: 1 } });
  t.tool("apply_patch", "a.rs", { stats: { files: 1, additions: 1, deletions: 0 } });
  t.tool("apply_patch", "c.rs", { state: "failed" });
  t.tool("exec_command", "cargo test", { outcome: { exit_code: 0, tail: ["ok"], summary: "18 passed" } });
  t.tool("exec_command", "cargo clippy", { state: "failed", outcome: { exit_code: 1, tail: ["error: unused"], summary: null } });
  t.say("Fixed.");
  t.done(402_000_000_000, 9_000);
  const data = t.data();
  const turn = segmentTurns(data)[0]!;
  expect(turnOutcome(turn, planTurn(turn, data.entries), data.entries)).toEqual({
    durationNs: 402_000_000_000,
    toolCalls: 7,
    changes: [
      { paths: ["a.rs", "b.rs"], additions: 10, deletions: 2 },
      { paths: ["a.rs"], additions: 4, deletions: 1 },
    ],
    files: 2,
    additions: 14,
    deletions: 3,
    tests: { summary: "18 passed", failed: false },
    failures: [6, 8],
    endedAt: 9_000,
  });
});

test("test summaries that report failures are recognised", () => {
  expect(summaryFailed("17 passed, 1 failed")).toBe(true);
  expect(summaryFailed("3 passed; 0 failed")).toBe(false);
  expect(summaryFailed("2 errors")).toBe(true);
  expect(summaryFailed("ok. 12 passed")).toBe(false);
});

test("a command's row shows its exit code and summary, or else its last line", () => {
  const t = transcript();
  t.tool("exec_command", "cargo test", { state: "failed", outcome: failedTests });
  t.tool("exec_command", "ls", { outcome: { exit_code: 0, tail: ["a", "b"], summary: null } });
  t.tool("exec_command", "true", { outcome: { exit_code: 0, tail: [], summary: null } });
  t.tool("read", "a.rs");
  expect((t.entries as ToolEntry[]).map(resultText)).toEqual(["exit 101 · 17 passed, 1 failed", "b", null, null]);
});

test("a turn exports as Markdown without its narration", () => {
  const markdown = turnMarkdown("  Fix the bridge  ", "It is fixed.", {
    durationNs: 1, toolCalls: 3, files: 2, additions: 14, deletions: 3, failures: [], endedAt: null,
    changes: [{ paths: ["a.rs", "b.rs"], additions: 10, deletions: 2 }, { paths: ["a.rs"], additions: 4, deletions: 1 }],
    tests: { summary: "18 passed", failed: false },
  });
  expect(markdown).toBe([
    "## Prompt", "", "Fix the bridge", "",
    "## Answer", "", "It is fixed.", "",
    "## Changes", "", "- `a.rs`, `b.rs` (+10 −2)", "- `a.rs` (+4 −1)", "", "2 files changed, +14 −3", "",
    "## Tests", "", "18 passed", "",
  ].join("\n"));
  expect(turnMarkdown("Hi", null, { durationNs: null, toolCalls: 0, changes: [], files: 0, additions: 0, deletions: 0, tests: null, failures: [], endedAt: null }))
    .toBe("## Prompt\n\nHi\n");
});
