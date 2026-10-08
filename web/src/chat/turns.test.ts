import { expect, test } from "bun:test";
import { planTurn, segmentTurns } from "./turns";
import { transcript } from "./test-entries";

test("a turn runs from a prompt to its end; later notices trail it and earlier ones form a preamble", () => {
  const t = transcript();
  t.other({ kind: "forked_from", session: "abc" });
  t.user("first");
  t.say("answer");
  t.done();
  t.other({ kind: "context_compacted", duration_ns: 1 });
  t.user("second");
  t.tool("read", "a.rs");
  expect(segmentTurns(t.data())).toEqual([
    { key: -1, user: null, body: [1], end: null, trailing: [] },
    { key: 2, user: 2, body: [3], end: 4, trailing: [5] },
    { key: 6, user: 6, body: [7], end: null, trailing: [] },
  ]);
});

test("the last assistant message is the answer and earlier ones narrate", () => {
  const t = transcript();
  t.user("go");
  t.say("Looking around first.");
  t.tool("read", "a.rs");
  t.say("Done: it works.");
  t.done();
  const data = t.data();
  const plan = planTurn(segmentTurns(data)[0]!, data.entries);
  expect(plan.answer).toBe(4);
  expect([...plan.narration]).toEqual([2]);
  expect(plan.log).toEqual([{ kind: "entry", id: 2 }, { kind: "entry", id: 3 }]);
});

test("while a turn runs, a message is the answer only until a tool call follows it", () => {
  const t = transcript();
  t.user("go");
  t.say("Let me check.");
  const data = t.data();
  expect(planTurn(segmentTurns(data)[0]!, data.entries).answer).toBe(2);
  t.tool("exec_command", "cargo test", { state: "running" });
  const later = t.data();
  const plan = planTurn(segmentTurns(later)[0]!, later.entries);
  expect(plan.answer).toBeNull();
  expect([...plan.narration]).toEqual([2]);
});

test("consecutive routine calls fold into one group with the thoughts between them", () => {
  const t = transcript();
  t.user("go");
  t.tool("read", "a.rs");
  t.think();
  t.tool("exec_command", "cargo test");
  t.tool("memory", "scan · local · ordering · 2 candidates");
  t.think("before the edit");
  t.tool("apply_patch", "a.rs", { significance: "landmark" });
  t.tool("read", "b.rs");
  t.say("done");
  t.done();
  const data = t.data();
  const plan = planTurn(segmentTurns(data)[0]!, data.entries);
  expect(plan.log).toEqual([
    { kind: "group", key: 2, members: [2, 3, 4, 5], tools: [2, 4, 5] },
    { kind: "entry", id: 6 },
    { kind: "entry", id: 7 },
    { kind: "entry", id: 8 },
  ]);
  expect(plan.steps).toBe(5);
});

test("landmarks are never swallowed by a group", () => {
  const t = transcript();
  t.user("go");
  t.tool("read", "a.rs");
  t.tool("exec_command", "cargo test", { state: "failed" });
  t.tool("exec_command", "ls");
  t.tool("exec", "2 tools", { significance: "landmark" });
  t.tool("read", "b.rs");
  t.tool("memory", "replace · local · 12@v3", { significance: "landmark" });
  t.tool("read", "c.rs");
  const data = t.data();
  const plan = planTurn(segmentTurns(data)[0]!, data.entries);
  expect(plan.log.every((item) => item.kind === "entry")).toBe(true);
  expect(plan.unrecovered).toEqual([3]);
});

test("a running routine call joins the run before it, under the run's key", () => {
  const t = transcript();
  t.user("go");
  t.tool("exec_command", "jj st");
  t.tool("exec_command", "cargo test", { state: "running" });
  const data = t.data();
  expect(planTurn(segmentTurns(data)[0]!, data.entries).log).toEqual([
    { kind: "group", key: 2, members: [2, 3], tools: [2, 3] },
  ]);
});

test("a group keeps its key as it grows, so the reader's open state survives a new member", () => {
  const t = transcript();
  t.user("go");
  t.tool("exec_command", "jj st");
  t.tool("exec_command", "cargo check");
  const before = t.data();
  expect(planTurn(segmentTurns(before)[0]!, before.entries).log).toEqual([{ kind: "group", key: 2, members: [2, 3], tools: [2, 3] }]);
  t.think();
  t.tool("exec_command", "cargo test", { state: "running" });
  const after = t.data();
  expect(planTurn(segmentTurns(after)[0]!, after.entries).log).toEqual([{ kind: "group", key: 2, members: [2, 3, 4, 5], tools: [2, 3, 5] }]);
});

test("a group dissolves when a member turns into a landmark", () => {
  const t = transcript();
  t.user("go");
  t.tool("exec_command", "jj st");
  const test = t.tool("exec_command", "cargo test", { state: "running" });
  const running = t.data();
  expect(planTurn(segmentTurns(running)[0]!, running.entries).log).toEqual([{ kind: "group", key: 2, members: [2, 3], tools: [2, 3] }]);
  Object.assign(t.entries[test - 1]!, { state: "failed", significance: "landmark" });
  const failed = t.data();
  expect(planTurn(segmentTurns(failed)[0]!, failed.entries).log).toEqual([{ kind: "entry", id: 2 }, { kind: "entry", id: 3 }]);
});

test("a batch's children extend it in place, whatever arrived between them", () => {
  const t = transcript();
  t.user("go");
  const batch = t.tool("exec", "4 tools", { significance: "landmark" });
  t.tool("read", "a.rs", { parent: batch });
  t.tool("exec_command", "cargo test", { parent: batch, state: "failed" });
  t.other({ kind: "directed_message", from: "agent 2", to: "root", body: "hi", delivery: "delivered", thread: 1, messages: [] });
  t.say("Waiting on the batch.");
  t.tool("read", "b.rs", { parent: batch });
  t.tool("exec_command", "cargo test", { parent: batch });
  t.tool("read", "c.rs");
  t.tool("read", "d.rs");
  t.say("done");
  t.done();
  // A child that arrives after the end still belongs to its parent's turn.
  t.tool("read", "late.rs", { parent: batch });
  const data = t.data();
  const [turn] = segmentTurns(data);
  expect(turn).toMatchObject({ end: 12, trailing: [], body: [2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 13] });
  const plan = planTurn(turn!, data.entries);
  expect(plan.log).toEqual([
    { kind: "entry", id: 2 },
    { kind: "entry", id: 5 },
    { kind: "entry", id: 6 },
    { kind: "group", key: 9, members: [9, 10], tools: [9, 10] },
  ]);
  expect(plan.children.get(batch)).toEqual([3, 4, 7, 8, 13]);
  expect(plan.answer).toBe(11);
  expect(plan.steps).toBe(8);
  expect([...plan.recovered]).toEqual([4]);
});

test("error notices stay pinned and failures that were retried successfully are recovered", () => {
  const t = transcript();
  t.user("go");
  t.tool("exec_command", "cargo test", { state: "failed" });
  t.tool("apply_patch", "a.rs", { significance: "landmark" });
  t.tool("exec_command", "cargo test");
  t.other({ kind: "error", message: "stream reset" });
  t.done();
  const data = t.data();
  const plan = planTurn(segmentTurns(data)[0]!, data.entries);
  expect([...plan.recovered]).toEqual([2]);
  expect(plan.unrecovered).toEqual([]);
  expect([...plan.pinned]).toEqual([5]);
});
