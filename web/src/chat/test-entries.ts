import { transcriptData } from "../core/store";
import type { PatchStats, Significance, ToolOutcome, ToolState, WireEntry } from "../core/wire";

/**
 * Builds transcripts for tests: each helper appends one entry with the next id. A tool call is
 * routine unless it failed or `significance` says otherwise.
 */
export function transcript() {
  const entries: WireEntry[] = [];
  const add = (body: Record<string, unknown>) => {
    const entry = { parent: null, ...body, id: entries.length + 1, revision: 1 } as unknown as WireEntry;
    entries.push(entry);
    return entry.id;
  };
  const builder = {
    entries,
    data: () => transcriptData(entries),
    user: (text: string, at_ms?: number) => add({ kind: "user", text, at_ms }),
    say: (text: string) => add({ kind: "assistant", text, complete: true, commentary: false }),
    think: (text = "thinking") => add({ kind: "reasoning", text }),
    tool: (name: string, summary: string, options: { state?: ToolState; significance?: Significance; outcome?: ToolOutcome; stats?: PatchStats; duration?: number; parent?: number; at_ms?: number } = {}) =>
      add({
        kind: "tool", name, summary, state: options.state ?? "succeeded", duration_ns: options.duration ?? 1_000_000,
        substeps: [], child_count: 0, has_detail: true, outcome: options.outcome ?? null, stats: options.stats ?? null,
        significance: options.significance ?? (options.state === "failed" ? "landmark" : "routine"),
        parent: options.parent ?? null, at_ms: options.at_ms,
      }),
    done: (duration_ns = 60_000_000_000, at_ms?: number) => add({ kind: "turn_completed", duration_ns, at_ms }),
    other: (body: Record<string, unknown>) => add(body),
  };
  return builder;
}
