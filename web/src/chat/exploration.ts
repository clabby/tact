import type { WireEntry } from "../core/wire";

export type ToolEntry = Extract<WireEntry, { kind: "tool" }>;

/** What a low-signal call looked at, which names it in an "Explored" step. */
export type ExplorationKind = "file" | "search" | "listing" | "web" | "memory" | "session" | "agents";

const KINDS: Record<string, ExplorationKind> = {
  read: "file", read_file: "file", view: "file", view_image: "file", open: "file", cat: "file",
  grep: "search", glob: "search", search: "search", find: "search", rg: "search", find_sessions: "search",
  list: "listing", ls: "listing", list_dir: "listing", list_directory: "listing",
  web__run: "web", web_search: "web", web_fetch: "web", fetch: "web",
  read_session: "session", current_session: "session",
  wait_agent: "agents", list_agents: "agents",
};

/**
 * Whether a call only looks around (reads, searches, listings, lookups) and can fold into an
 * "Explored" step. Calls that write, execute, or patch never qualify, and neither does a failed
 * call: those always keep their own row. Memory calls qualify only for their read-only operations,
 * named first in their summary.
 */
export function explorationKind(entry: WireEntry): ExplorationKind | null {
  if (entry.kind !== "tool" || entry.parent !== null || entry.state === "failed") return null;
  const name = entry.name.toLowerCase();
  if (name === "memory") return /^(scan|read)\b/.test(entry.summary) ? "memory" : null;
  return KINDS[name] ?? null;
}

const NOUNS: Record<ExplorationKind, [string, string]> = {
  file: ["file", "files"],
  search: ["search", "searches"],
  listing: ["listing", "listings"],
  web: ["web lookup", "web lookups"],
  memory: ["memory lookup", "memory lookups"],
  session: ["session lookup", "session lookups"],
  agents: ["agent check", "agent checks"],
};

/**
 * "9 files, 4 searches": what a run of exploring calls covered, largest first. Files count once
 * per path, whatever line ranges were read.
 */
export function explorationLabel(entries: readonly ToolEntry[]) {
  const counts = new Map<ExplorationKind, number>();
  const paths = new Set<string>();
  for (const entry of entries) {
    const kind = explorationKind(entry);
    if (!kind) continue;
    if (kind === "file") {
      const path = entry.summary.replace(/:\d+(-\d+)?$/, "");
      if (paths.has(path)) continue;
      paths.add(path);
    }
    counts.set(kind, (counts.get(kind) ?? 0) + 1);
  }
  return [...counts]
    .sort((a, b) => b[1] - a[1])
    .map(([kind, count]) => `${count} ${NOUNS[kind][count === 1 ? 0 : 1]}`)
    .join(", ");
}
