import { expect, test } from "bun:test";
import { classifyFailures } from "./recovery";
import { transcript } from "./test-entries";
import type { ToolEntry } from "./exploration";

test("a failure is recovered only by a later success of the same tool and summary", () => {
  const t = transcript();
  t.tool("exec_command", "cargo test", { state: "failed" });
  t.tool("exec_command", "cargo test -p other");
  t.tool("write_stdin", "cargo test");
  t.tool("exec_command", "cargo clippy", { state: "failed" });
  t.tool("exec_command", "cargo test");
  t.tool("exec_command", "cargo build");
  t.tool("exec_command", "cargo build", { state: "failed" });
  t.tool("exec_command", "cargo doc", { state: "running" });
  expect(classifyFailures(t.entries as ToolEntry[])).toEqual({ recovered: new Set([1]), unrecovered: [4, 7] });
});
