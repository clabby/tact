import type { WireEntry } from "../core/wire";

export type ToolEntry = Extract<WireEntry, { kind: "tool" }>;

/**
 * Whether a call can fold into a run of routine work: the server classed it routine and it has a
 * row of its own rather than extending a Code Mode batch.
 */
export function isRoutine(entry: WireEntry): entry is ToolEntry {
  return entry.kind === "tool" && entry.parent === null && entry.significance === "routine";
}

/** What a routine call counts as in the label of its run. */
type RunKind = "command" | "cell" | "shell input" | "file" | "web" | "memory" | "session" | "agents" | "other";

/** The tools Tact runs, by what they count as; any other tool counts as a generic tool call. */
const KINDS: Record<string, RunKind> = {
  exec_command: "command", exec: "cell", write_stdin: "shell input", view_image: "file", web__run: "web",
  memory: "memory", read_session: "session", find_sessions: "session", current_session: "session",
  wait_agent: "agents", list_agents: "agents",
};

const NOUNS: Record<RunKind, [string, string]> = {
  command: ["command", "commands"],
  cell: ["code cell", "code cells"],
  "shell input": ["shell input", "shell inputs"],
  file: ["file", "files"],
  web: ["web lookup", "web lookups"],
  memory: ["memory lookup", "memory lookups"],
  session: ["session lookup", "session lookups"],
  agents: ["agent check", "agent checks"],
  other: ["tool call", "tool calls"],
};

const kindOf = (call: ToolEntry) => KINDS[call.name] ?? "other";
const runs = (kind: RunKind) => kind === "command" || kind === "cell";

/**
 * The header of a folded run. A run that ran a shell command or Code Mode cell "Ran"; one that
 * only looked around "Explored". While a call runs the header names it, as "Running" for a
 * command or cell and "Exploring" otherwise; once the run settles it counts what the run covered,
 * largest first ("3 commands, 2 files"), counting each file once.
 */
export function runLabel(calls: readonly ToolEntry[]): { verb: string; summary: string } {
  const live = [...calls].reverse().find((call) => call.state === "running");
  if (live) return { verb: runs(kindOf(live)) ? "Running" : "Exploring", summary: live.summary };
  const counts = new Map<RunKind, number>();
  const paths = new Set<string>();
  for (const call of calls) {
    const kind = kindOf(call);
    if (kind === "file") {
      if (paths.has(call.summary)) continue;
      paths.add(call.summary);
    }
    counts.set(kind, (counts.get(kind) ?? 0) + 1);
  }
  const summary = [...counts]
    .sort((a, b) => b[1] - a[1])
    .map(([kind, count]) => `${count} ${NOUNS[kind][count === 1 ? 0 : 1]}`)
    .join(", ");
  return { verb: [...counts.keys()].some(runs) ? "Ran" : "Explored", summary };
}

/**
 * How long a folded run has taken: from its earliest call's start to now while a call runs, else to
 * the end of its latest call. Everything is measured on the server's clock, anchored at a running
 * call's reported elapsed time, so the browser's clock cannot skew it. `live` marks a span that is
 * still growing; `null` means the server sent no start times.
 */
export function runSpan(calls: readonly ToolEntry[]): { ns: number; live: boolean } | null {
  const starts = calls.flatMap((call) => (call.at_ms == null ? [] : [call.at_ms]));
  if (starts.length !== calls.length || !calls.length) return null;
  const first = Math.min(...starts);
  const running = calls.find((call) => call.state === "running" && call.elapsed_ns != null);
  if (running) return { ns: running.elapsed_ns! + (running.at_ms! - first) * 1e6, live: true };
  const end = Math.max(...calls.map((call) => call.at_ms! + (call.duration_ns ?? 0) / 1e6));
  return { ns: (end - first) * 1e6, live: false };
}
