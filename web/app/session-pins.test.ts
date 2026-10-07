import { expect, test } from "bun:test";
import { orderSessions } from "./session-pins";
import type { SessionSummary } from "./wire";

const session = (id: string, activity: number) => ({ id, last_activity_unix_ms: activity }) as SessionSummary;

test("pinned sessions stay on top, and each group follows recent activity", () => {
  const live = [session("a", 1), session("b", 5), session("c", 3), session("d", 9)];
  expect(orderSessions(live, new Set(["a", "c"])).map((s) => s.id)).toEqual(["c", "a", "d", "b"]);
});

test("with nothing pinned the order is by activity, and unknown pins are ignored", () => {
  const live = [session("a", 1), session("b", 2)];
  expect(orderSessions(live, new Set()).map((s) => s.id)).toEqual(["b", "a"]);
  expect(orderSessions(live, new Set(["gone"])).map((s) => s.id)).toEqual(["b", "a"]);
});
