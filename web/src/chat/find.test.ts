import { expect, test } from "bun:test";
import { findMatches, occurrences } from "./find";
import { transcript } from "./test-entries";

test("find matches prompts, messages, tool summaries and output tails, ignoring case", () => {
  const t = transcript();
  t.user("Fix the Bridge");
  t.think("bridge thoughts are not searched");
  t.tool("exec_command", "cargo test", { state: "failed", outcome: { exit_code: 1, tail: ["FAIL web::bridge::echo"], summary: null } });
  t.tool("read", "src/bridge.rs");
  t.say("The bridge is fixed.");
  t.other({ kind: "error", message: "bridge reset" });
  expect(findMatches(t.data(), " BRIDGE ")).toEqual([1, 3, 4, 5, 6]);
  expect(findMatches(t.data(), "   ")).toEqual([]);
});

test("occurrences are found without overlapping", () => {
  expect(occurrences("aaaa Bridge bridge", "bridge")).toEqual([5, 12]);
  expect(occurrences("aaaa", "aa")).toEqual([0, 2]);
});
