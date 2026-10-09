import type { Subagent, TransientStatus } from "./wire";

/** Compact durations the way the TUI prints them: 840ms, 4.2s, 6m 42s, 1h 05m. */
export function formatDuration(nanoseconds: number) {
  const ms = nanoseconds / 1e6;
  if (ms < 1000) return `${Math.max(0, Math.round(ms))}ms`;
  const seconds = ms / 1000;
  if (seconds < 10) return `${seconds.toFixed(1)}s`;
  if (seconds < 60) return `${Math.round(seconds)}s`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ${String(Math.round(seconds % 60)).padStart(2, "0")}s`;
  return `${Math.floor(minutes / 60)}h ${String(minutes % 60).padStart(2, "0")}m`;
}

/** "now", "5m", "3h", "2d", then a short date. */
export function formatAge(unixMs: number, now = Date.now()) {
  const seconds = Math.max(0, (now - unixMs) / 1000);
  if (seconds < 45) return "now";
  if (seconds < 3600) return `${Math.round(seconds / 60)}m`;
  if (seconds < 86_400) return `${Math.round(seconds / 3600)}h`;
  if (seconds < 7 * 86_400) return `${Math.round(seconds / 86_400)}d`;
  return new Date(unixMs).toLocaleDateString(undefined, { month: "short", day: "numeric" });
}

/** The live status line's text, or null when there is nothing to say. */
export function statusLabel(status: TransientStatus | null): string | null {
  if (!status) return null;
  switch (status.kind) {
    case "thinking": return "Thinking";
    case "responding": return "Responding";
    case "warming": return "Starting";
    case "waiting_for_background_work": return "Waiting for background work";
    case "compacting": return "Compacting context";
    case "connecting": return "Connecting";
    case "reconnecting": return "Reconnecting";
    case "tool": return `Running ${status.name}`;
    case "retrying":
      return `Retrying in ${formatDuration(status.delay_ns)} · attempt ${status.next_attempt} of ${status.max_attempts}`;
    case "error": return status.message;
  }
  return null;
}

/**
 * Whether the composer must refuse input. A manual compaction is not a turn, so the session reports
 * "compacting" while idle and the server rejects every edit until it finishes; an automatic
 * compaction runs inside a turn, where typing and queueing stay allowed.
 */
export function inputBlocked(status: TransientStatus | null, running: boolean): boolean {
  return status?.kind === "compacting" && !running;
}

/** The CSS custom property holding a model's hue, following the TUI's model palette. */
export function modelColor(model: string) {
  const family = ["luna", "sol", "astra", "haiku", "sonnet", "opus", "fable"].find((name) => model.includes(name));
  return family ? `var(--model-${family})` : "var(--muted)";
}

export function effortColor(effort: string) {
  return ["low", "medium", "high", "xhigh", "max"].includes(effort) ? `var(--effort-${effort})` : "var(--muted)";
}

/** The first line of `text`, trimmed for one-line previews. */
export function firstLine(text: string, limit = 120) {
  const line = text.trim().split("\n", 1)[0] ?? "";
  return line.length > limit ? `${line.slice(0, limit - 1)}…` : line;
}

/** 950, 12.4k, 1.2M. */
export function formatTokens(tokens: number) {
  if (tokens < 1000) return String(tokens);
  if (tokens < 1_000_000) return `${(tokens / 1000).toFixed(tokens < 10_000 ? 1 : 0)}k`;
  return `${(tokens / 1_000_000).toFixed(1)}M`;
}

export function agentModelLabel(agent: Subagent) {
  return [agent.model, agent.thinking, agent.reasoning_mode === "pro" ? "pro" : ""].filter(Boolean).join(" · ");
}
