import { expect, test } from "bun:test";
import { explorationKind, explorationLabel, type ToolEntry } from "./exploration";
import { transcript } from "./test-entries";

test("only successful read-only calls explore", () => {
  const t = transcript();
  const kinds = [
    t.tool("read", "a.rs"),
    t.tool("web__run", "query"),
    t.tool("list_agents", "all agents"),
    t.tool("memory", "read · local · 12@v2 · 1 memory"),
    t.tool("memory", "delete · 9@v1"),
    t.tool("exec_command", "rg foo"),
    t.tool("apply_patch", "a.rs"),
    t.tool("read", "b.rs", { state: "failed" }),
    t.tool("read", "c.rs", { parent: 1 }),
  ].map((id) => explorationKind(t.entries[id - 1]!));
  expect(kinds).toEqual(["file", "web", "agents", "memory", null, null, null, null, null]);
});

test("the label counts each file once and lists the largest kinds first", () => {
  const t = transcript();
  for (const path of ["a.rs:1-40", "a.rs:40-80", "b.rs", "c.rs"]) t.tool("read", path);
  t.tool("grep", "fn publish");
  t.tool("read_session", "abc");
  expect(explorationLabel(t.entries as ToolEntry[])).toBe("3 files, 1 search, 1 session lookup");
});
