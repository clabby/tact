import type { ToolEntry } from "./exploration";

/**
 * Splits a turn's failed calls into recovered and unrecovered ones. A failure is recovered when a
 * later call in the same turn with the same tool name and the same summary succeeded: the agent
 * retried the command (perhaps after fixing something) and it passed.
 */
export function classifyFailures(tools: readonly ToolEntry[]) {
  const recovered = new Set<number>();
  const unrecovered: number[] = [];
  const succeededLater = new Set<string>();
  for (let index = tools.length - 1; index >= 0; index -= 1) {
    const tool = tools[index]!;
    const key = `${tool.name}\u0000${tool.summary}`;
    if (tool.state === "succeeded") succeededLater.add(key);
    else if (tool.state === "failed") {
      if (succeededLater.has(key)) recovered.add(tool.id);
      else unrecovered.unshift(tool.id);
    }
  }
  return { recovered, unrecovered };
}
