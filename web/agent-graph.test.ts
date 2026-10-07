import { expect, test } from "bun:test";
import { isActive, layoutAgents } from "./agent-graph";
import type { Subagent } from "./wire";

const agent = (id: number, parent: number | null, state: Subagent["status"]["state"] = "running"): Subagent => ({
  id, parent, session_id: `s${id}`, role: `agent ${id}`, task: "", model: "sol", thinking: "high",
  status: { state } as Subagent["status"],
});

const at = (layout: ReturnType<typeof layoutAgents>, id: number) => layout.nodes.find((node) => node.agent.id === id)!;

test("a parent is centred over its children and depth sets the row", () => {
  const layout = layoutAgents([agent(1, null), agent(2, 1), agent(3, 1), agent(4, 2)]);
  const [one, two, three, four] = [1, 2, 3, 4].map((id) => at(layout, id));
  expect(one.y).toBeLessThan(two.y);
  expect(two.y).toBe(three.y);
  expect(four.y).toBeGreaterThan(two.y);
  expect(one.x).toBe((two.x + three.x) / 2);
  expect(four.x).toBe(two.x);
  expect(layout.edges).toHaveLength(3);
  expect(layout.edges).toContainEqual({ from: 2, to: 4 });
});

test("agents with an unlisted parent are roots side by side", () => {
  const layout = layoutAgents([agent(5, 99), agent(6, null)]);
  expect(at(layout, 5).y).toBe(at(layout, 6).y);
  expect(at(layout, 5).x).toBeLessThan(at(layout, 6).x);
  expect(layout.edges).toEqual([]);
});

test("a parent cycle still places every agent once", () => {
  const layout = layoutAgents([agent(1, 2), agent(2, 1), agent(3, 3)]);
  expect(layout.nodes).toHaveLength(3);
  expect(new Set(layout.nodes.map((node) => `${node.x},${node.y}`)).size).toBe(3);
});

test("an empty roster has no extent, and only pending or running agents are active", () => {
  expect(layoutAgents([])).toEqual({ nodes: [], edges: [], width: 0, height: 0 });
  expect(["pending", "running", "closing", "completed", "closed"].map((state) => isActive(agent(1, null, state as Subagent["status"]["state"])))).toEqual([true, true, true, false, false]);
});
