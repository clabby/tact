import type { MonitorCheck, SessionActivity, SessionNote, SessionSummary } from "../core/wire";

/** How long a running session may go without new activity, and no call in flight, before it counts as quiet. */
export const QUIET_MS = 10 * 60_000;

/**
 * Why the Monitor places a session where it does, from the most to the least urgent. An error in
 * the live state outranks everything. The monitor model's notes add judgement the live state
 * cannot (waiting on a question, stuck, off track), and that judgement outranks an unread reply,
 * which would otherwise present a session that went wrong as one with news.
 */
export const CONCERNS = ["error", "needs_input", "stuck", "off_track", "unread", "retrying", "quiet", "ready", "working", "idle"] as const;
export type Concern = (typeof CONCERNS)[number];

/** The Monitor's sections: sessions that need the user, sessions at work, and the rest. */
export type MonitorGroup = "needs" | "working" | "idle";

/** How the grid orders sessions: by group, or by the monitor's health score, lowest first. */
export type MonitorOrder = "attention" | "health";

export type Assessment = {
  concern: Concern;
  group: MonitorGroup;
  /** The latest check's note on the session, if it has one. */
  note: SessionNote | null;
  /** The session changed after the check read it, so the note describes an earlier moment. */
  noteEarlier: boolean;
};

const GROUPS: Record<Concern, MonitorGroup> = {
  error: "needs", needs_input: "needs", stuck: "needs", off_track: "needs", unread: "needs", retrying: "needs", quiet: "needs",
  ready: "idle", working: "working", idle: "idle",
};

const GROUP_ORDER: readonly MonitorGroup[] = ["needs", "working", "idle"];

/**
 * Places one live session. A note that predates an idle session's latest activity describes a
 * turn that has since ended, so it no longer counts; a running session's notes keep counting
 * because its activity moves on continuously. "Needs input" and "ready" describe an idle agent and
 * are ignored while the session runs.
 */
export function assess(summary: SessionSummary, activity: SessionActivity | undefined, check: MonitorCheck | null, now: number): Assessment {
  const running = summary.state === "running";
  const note = check?.sessions.find((candidate) => candidate.session === summary.id) ?? null;
  const noteEarlier = note !== null && summary.last_activity_unix_ms > check!.as_of_ms;
  const verdict = note && (running || !noteEarlier) ? note.attention : null;
  const idleVerdict = running ? null : verdict;
  const status = activity?.status ?? null;
  const quiet = running
    && activity?.tool?.state !== "running"
    && (activity?.agents.running ?? 0) === 0
    && now - summary.last_activity_unix_ms >= QUIET_MS;
  const concern: Concern =
    summary.state === "error" || status?.kind === "error" || (!running && activity?.error) ? "error"
    : idleVerdict === "needs_input" ? "needs_input"
    : verdict === "stuck" ? "stuck"
    : verdict === "off_track" ? "off_track"
    : !running && summary.unread ? "unread"
    : status?.kind === "retrying" ? "retrying"
    : quiet ? "quiet"
    : idleVerdict === "ready" ? "ready"
    : running ? "working"
    : "idle";
  return { concern, group: GROUPS[concern], note, noteEarlier };
}

export type RankedSession<Activity extends SessionActivity = SessionActivity> = {
  summary: SessionSummary;
  activity: Activity | undefined;
  assessment: Assessment;
};

/**
 * Orders the live sessions by group (needs the user, working, idle) or by health (lowest first,
 * sessions without a score last) and then newest opened first: the live list is in opening order,
 * so it is reversed. Cards move only when their group or score changes, never because one of them
 * did something.
 */
export function rankSessions<Activity extends SessionActivity>(
  live: readonly SessionSummary[],
  activity: ReadonlyMap<string, Activity>,
  check: MonitorCheck | null,
  now: number,
  order: MonitorOrder = "attention",
): RankedSession<Activity>[] {
  const rank = (assessment: Assessment) => order === "health"
    ? assessment.note?.health ?? Infinity
    : GROUP_ORDER.indexOf(assessment.group);
  return live
    .map((summary, opened) => {
      const assessment = assess(summary, activity.get(summary.id), check, now);
      return { summary, activity: activity.get(summary.id), assessment, rank: rank(assessment), opened };
    })
    .sort((a, b) => a.rank - b.rank || b.opened - a.opened)
    .map(({ summary, activity, assessment }) => ({ summary, activity, assessment }));
}

/** The figures the sidebar's Monitor item shows: sessions that need the user, and sessions running. */
export function monitorCounts(
  live: readonly SessionSummary[],
  activity: ReadonlyMap<string, SessionActivity>,
  check: MonitorCheck | null,
  now: number,
) {
  return {
    needs: live.filter((summary) => assess(summary, activity.get(summary.id), check, now).group === "needs").length,
    running: live.filter((summary) => summary.state === "running").length,
  };
}
