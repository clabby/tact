import { describe, expect, test } from "bun:test";
import { CLAMP_LINES, deliveryNote, deliverySummary, needsClamp, party, sentMessage } from "./directed";
import type { AgentMessage, Subagent } from "../core/wire";

const agent = (id: number, role: string): Subagent => ({
  id, parent: null, session_id: `s${id}`, role, task: "", model: "sol", thinking: "high", reasoning_mode: "standard", status: { state: "running" },
});
const agents = [agent(2, "protocol auditor"), agent(4, "docs verifier")];

const message = (id: number, fields: Partial<AgentMessage> = {}): AgentMessage => ({
  id, from: 4, to: 2, purpose: "coordinate", priority: "deferred", in_reply_to: null, body: "", delivery: "delivered", detail: null, ...fields,
});

describe("party", () => {
  test("names agents by role, the transcript's owner as me, other roots as root, and unknown agents by id", () => {
    expect(party(2, { agents }).label).toBe("protocol auditor");
    expect(party(4, { viewer: 4, agents })).toMatchObject({ label: "me", title: "Me: docs verifier · #4 · sol" });
    expect(party(2, { viewer: 4, agents }).label).toBe("protocol auditor");
    expect(party(null, { viewer: 4, agents }).label).toBe("root");
    expect(party(null, { agents }).label).toBe("me");
    expect(party(9, { agents })).toMatchObject({ label: "#9", color: "var(--faint)" });
  });
});

describe("needsClamp", () => {
  test("clamps bodies taller than the limit, counting wrapped lines", () => {
    expect(needsClamp(Array(CLAMP_LINES).fill("line").join("\n"))).toBe(false);
    expect(needsClamp(Array(CLAMP_LINES + 1).fill("line").join("\n"))).toBe(true);
    expect(needsClamp(Array(CLAMP_LINES).fill("line").join("\n") + "\n\n")).toBe(false);
    expect(needsClamp("x".repeat(96 * CLAMP_LINES))).toBe(false);
    expect(needsClamp("x".repeat(96 * CLAMP_LINES + 1))).toBe(true);
  });
});

describe("delivery", () => {
  test("a failure anywhere outranks messages in flight, which outrank delivered ones", () => {
    const failed = message(1, { delivery: "failed", detail: "agent 4 has failed" });
    const pending = message(2, { delivery: "admitted", detail: "queued" });
    const delivered = message(3);
    expect(deliverySummary([failed, pending, delivered])).toEqual({ state: "failed", label: "Delivery failed: agent 4 has failed" });
    expect(deliverySummary([failed, { ...failed, id: 4, detail: "closed" }]).label).toBe("2 deliveries failed: closed");
    expect(deliverySummary([delivered, pending])).toEqual({ state: "admitted", label: "Not yet delivered: queued" });
    expect(deliverySummary([delivered]).state).toBe("delivered");
    expect(deliverySummary([delivered, message(4, { delivery: "unknown" })]).state).toBe("unknown");
    expect(deliverySummary([]).state).toBe("unknown");
  });

  test("each message notes how it was taken or why it failed", () => {
    expect(deliveryNote(message(1, { delivery: "admitted", detail: "queued" }))).toBe("queued");
    expect(deliveryNote(message(1, { detail: "steered" }))).toBe("delivered · steered");
    expect(deliveryNote(message(1, { delivery: "failed", detail: "closed" }))).toBe("failed: closed");
    expect(deliveryNote(message(1, { delivery: "unknown" }))).toBe("");
  });
});

describe("sentMessage", () => {
  const args = { agent_id: 2, message: "Check the docs.", purpose: "question", priority: "urgent", in_reply_to: 7 };

  test("reads the call's arguments and its JSON receipt, sent by the transcript's owner", () => {
    expect(sentMessage(args, JSON.stringify({ message_id: 12, thread_id: 3, disposition: "queued" }), 4)).toEqual({
      id: 12, from: 4, to: 2, purpose: "question", priority: "urgent", in_reply_to: 7,
      body: "Check the docs.", delivery: "admitted", detail: "queued",
    });
    expect(sentMessage({ agent_id: 2, message: "hi", purpose: "chat" }, null, undefined))
      .toMatchObject({ from: null, purpose: "coordinate", priority: "deferred", in_reply_to: null, delivery: "unknown" });
  });

  test("a result that is not a receipt is the failure, and malformed arguments are not a message", () => {
    expect(sentMessage(args, "agent 2 is closed", undefined)).toMatchObject({ delivery: "failed", detail: "agent 2 is closed" });
    expect(sentMessage({ agent_id: "2", message: "hi" }, null, undefined)).toBeNull();
  });
});
