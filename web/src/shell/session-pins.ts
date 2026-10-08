import type { SessionSummary } from "../core/wire";

/** Live sessions by recent activity, with pinned ones kept above the rest. */
export function orderSessions(live: readonly SessionSummary[], pinned: ReadonlySet<string>): SessionSummary[] {
  const recent = [...live].sort((a, b) => b.last_activity_unix_ms - a.last_activity_unix_ms);
  return [...recent.filter((session) => pinned.has(session.id)), ...recent.filter((session) => !pinned.has(session.id))];
}
