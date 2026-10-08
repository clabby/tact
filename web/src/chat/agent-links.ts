import { firstLine } from "../core/format";
import type { TranscriptData } from "../core/store";
import type { Subagent } from "../core/wire";
import { toolLabel } from "./tool-detail";

/**
 * The agent a `spawn_agent` call started, read from its result: a receipt object, its JSON text, or
 * text parts holding that JSON. Null when the result names no agent.
 */
export function spawnedAgentId(result: unknown): number | null {
  let value = result;
  if (Array.isArray(value)) {
    value = value.map((part) => (typeof part === "object" && part !== null && "text" in part ? String(part.text) : "")).join("");
  }
  if (typeof value === "string") {
    try {
      value = JSON.parse(value);
    } catch {
      return null;
    }
  }
  const id = typeof value === "object" && value !== null ? (value as { agent_id?: unknown }).agent_id : undefined;
  return typeof id === "number" && Number.isInteger(id) ? id : null;
}

/**
 * One line on what an agent is doing or did: a failure's error, a finished agent's result, else its
 * latest transcript activity, else its task.
 */
export function agentNote(agent: Subagent, transcript: TranscriptData | undefined): string {
  const status = agent.status;
  if (status.state === "failed") return firstLine(status.error);
  if (status.state === "completed" && status.output !== null && status.output !== undefined) {
    return firstLine(typeof status.output === "string" ? status.output : JSON.stringify(status.output));
  }
  const order = transcript?.order ?? [];
  for (let index = order.length - 1; index >= 0; index -= 1) {
    const entry = transcript!.entries.get(order[index]!);
    if (entry?.kind === "tool") return `${toolLabel(entry.name, entry.child_count)} ${entry.summary}`;
    if (entry?.kind === "assistant" && entry.text.trim()) return firstLine(entry.text);
    if (entry?.kind === "reasoning") return "Thinking";
  }
  return firstLine(agent.task);
}
