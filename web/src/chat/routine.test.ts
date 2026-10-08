import { expect, test } from "bun:test";
import { isRoutine, runLabel, runOpen, type ToolEntry } from "./routine";
import { transcript } from "./test-entries";

test("only routine calls with rows of their own can fold", () => {
  const t = transcript();
  t.tool("exec_command", "cargo test");
  t.tool("apply_patch", "a.rs", { significance: "landmark" });
  t.tool("exec_command", "ls", { state: "failed" });
  t.tool("view_image", "a.png", { parent: 1 });
  t.other({ kind: "tool", name: "view_image", summary: "b.png", state: "succeeded" });
  expect(t.entries.map(isRoutine)).toEqual([true, false, false, false, false]);
});

test("a run that ran commands or code cells says so and counts each file once", () => {
  const t = transcript();
  t.tool("exec_command", "cargo check");
  t.tool("exec_command", "jj st");
  t.tool("exec", "2 tools");
  t.tool("write_stdin", "poll output");
  for (const path of ["a.png", "a.png"]) t.tool("view_image", path);
  t.tool("mystery", "2 arguments");
  expect(runLabel(t.entries as ToolEntry[])).toEqual({
    verb: "Ran",
    summary: "2 commands, 1 code cell, 1 shell input, 1 file, 1 tool call",
  });
});

test("a code cell alone is enough to have run something", () => {
  const t = transcript();
  t.tool("exec", "1 emitted item");
  t.tool("web__run", "rust select");
  expect(runLabel(t.entries as ToolEntry[]).verb).toBe("Ran");
});

test("a run that only looked around explored", () => {
  const t = transcript();
  t.tool("web__run", "rust select");
  t.tool("read_session", "abc");
  t.tool("find_sessions", "draft");
  t.tool("write_stdin", "poll output");
  expect(runLabel(t.entries as ToolEntry[])).toEqual({ verb: "Explored", summary: "2 session lookups, 1 web lookup, 1 shell input" });
});

test("while a call runs, the run's header names it", () => {
  const t = transcript();
  t.tool("view_image", "a.png");
  t.tool("exec_command", "cargo test", { state: "running" });
  expect(runLabel(t.entries as ToolEntry[])).toEqual({ verb: "Running", summary: "cargo test" });
  const u = transcript();
  u.tool("exec_command", "ls");
  u.tool("web__run", "rust select", { state: "running" });
  expect(runLabel(u.entries as ToolEntry[])).toEqual({ verb: "Exploring", summary: "rust select" });
});

test("a run with a running call is open unless the reader closed it", () => {
  const t = transcript();
  t.tool("exec_command", "ls");
  const running = t.tool("exec_command", "cargo test", { state: "running" });
  const calls = t.entries as ToolEntry[];
  expect(runOpen(calls, false)).toBe(true);
  expect(runOpen(calls, true)).toBe(false);
  Object.assign(calls[running - 1]!, { state: "succeeded" });
  expect(runOpen(calls, false)).toBe(false);
  expect(runOpen(calls, true)).toBe(true);
});
