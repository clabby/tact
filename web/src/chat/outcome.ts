import type { WireEntry } from "../core/wire";
import type { ToolEntry } from "./exploration";
import type { Turn, TurnPlan } from "./turns";

/** One applied patch, or several with the same files, for the turn's change list. */
export type Change = { paths: string[]; additions: number; deletions: number };

/** What a finished turn did, as its outcome strip and Markdown export report it. */
export type TurnOutcome = {
  durationNs: number | null;
  toolCalls: number;
  changes: Change[];
  /** Distinct files the turn's patches touched. */
  files: number;
  additions: number;
  deletions: number;
  /** The last test or build summary a command reported. */
  tests: { summary: string; failed: boolean } | null;
  /** Failed calls that no later call made good. */
  failures: number[];
  /** When the turn ended, in unix milliseconds, when known. */
  endedAt: number | null;
};

export function turnOutcome(turn: Turn, plan: TurnPlan, entries: ReadonlyMap<number, WireEntry>): TurnOutcome {
  const changes = new Map<string, Change>();
  const files = new Set<string>();
  let tests: TurnOutcome["tests"] = null;
  let toolCalls = 0;
  let lastAt: number | null = null;
  for (const id of turn.body) {
    const entry = entries.get(id);
    if (!entry) continue;
    lastAt = entry.at_ms ?? lastAt;
    if (entry.kind !== "tool") continue;
    toolCalls += 1;
    if (entry.outcome?.summary) tests = { summary: entry.outcome.summary, failed: summaryFailed(entry.outcome.summary) };
    const stats = entry.stats;
    if (entry.state !== "succeeded" || !stats) continue;
    // A patch's summary lists its files; when it does not match the count, each file counts apart.
    const paths = entry.summary.split(", ").filter(Boolean);
    const named = paths.length === stats.files;
    for (let index = 0; index < stats.files; index += 1) files.add(named ? paths[index]! : `${id}#${index}`);
    const change = changes.get(entry.summary) ?? { paths: named ? paths : [entry.summary], additions: 0, deletions: 0 };
    change.additions += stats.additions;
    change.deletions += stats.deletions;
    changes.set(entry.summary, change);
  }
  const end = turn.end === null ? undefined : entries.get(turn.end);
  const list = [...changes.values()];
  return {
    durationNs: end?.kind === "turn_completed" ? end.duration_ns : null,
    toolCalls,
    changes: list,
    files: files.size,
    additions: list.reduce((sum, change) => sum + change.additions, 0),
    deletions: list.reduce((sum, change) => sum + change.deletions, 0),
    tests,
    failures: plan.unrecovered,
    endedAt: end?.at_ms ?? lastAt,
  };
}

/** Whether a test or build summary reports a failure ("17 passed, 1 failed", "2 errors"). */
export function summaryFailed(summary: string) {
  return /\b[1-9]\d*\s+(failed|failures?|errors?)\b/i.test(summary);
}

/**
 * What a finished command produced, for its row: a non-zero exit code and the test summary, or
 * else the last line of output. Null when the call has no outcome yet.
 */
export function resultText(entry: ToolEntry): string | null {
  const outcome = entry.outcome;
  if (!outcome) return null;
  const parts: string[] = [];
  if (outcome.exit_code !== null && outcome.exit_code !== 0) parts.push(`exit ${outcome.exit_code}`);
  const detail = outcome.summary ?? outcome.tail.at(-1);
  if (detail) parts.push(detail);
  return parts.length ? parts.join(" · ") : null;
}

/**
 * A finished turn as Markdown for sharing: the prompt, the final answer without the narration
 * that led to it, the files changed, and the test summary.
 */
export function turnMarkdown(prompt: string, answer: string | null, outcome: TurnOutcome) {
  const sections = [`## Prompt\n\n${prompt.trim()}`];
  if (answer?.trim()) sections.push(`## Answer\n\n${answer.trim()}`);
  if (outcome.changes.length) {
    const lines = outcome.changes.map((change) =>
      `- ${change.paths.map((path) => "`" + path + "`").join(", ")} (+${change.additions} −${change.deletions})`);
    const total = `${outcome.files} file${outcome.files === 1 ? "" : "s"} changed, +${outcome.additions} −${outcome.deletions}`;
    sections.push(`## Changes\n\n${lines.join("\n")}\n\n${total}`);
  }
  if (outcome.tests) sections.push(`## Tests\n\n${outcome.tests.summary}`);
  return `${sections.join("\n\n")}\n`;
}
