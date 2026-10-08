import { expect, test } from "bun:test";
import { agentNote, spawnedAgentId } from "./agent-links";
import { transcript } from "./test-entries";
import type { Subagent } from "../core/wire";

test("the spawned agent is read from a receipt in any of its shapes", () => {
  expect(spawnedAgentId({ agent_id: 4, model: "sol" })).toBe(4);
  expect(spawnedAgentId('{"agent_id":7,"role":"x"}')).toBe(7);
  expect(spawnedAgentId([{ type: "text", text: '{"agent_id":2}' }])).toBe(2);
  expect(spawnedAgentId("agent limit reached")).toBeNull();
  expect(spawnedAgentId(null)).toBeNull();
});

test("an agent's note prefers its error or result, then its latest activity, then its task", () => {
  const agent = (status: Subagent["status"]): Subagent => ({
    id: 2, parent: null, session_id: "s", role: "auditor", task: "Audit the protocol.\nIn depth.", model: "sol", thinking: "high", status,
  });
  const t = transcript();
  t.user("Audit the protocol.");
  t.tool("exec_command", "rg -n version");
  expect(agentNote(agent({ state: "running" }), t.data())).toBe("Shell rg -n version");
  expect(agentNote(agent({ state: "running" }), undefined)).toBe("Audit the protocol.");
  expect(agentNote(agent({ state: "failed", error: "permission denied" }), t.data())).toBe("permission denied");
  expect(agentNote(agent({ state: "completed", output: "No gaps found." }), t.data())).toBe("No gaps found.");
});
