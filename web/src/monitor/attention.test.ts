import { expect, test } from "bun:test";
import { assess, monitorCounts, QUIET_MS, rankSessions } from "./attention";
import type { MonitorCheck, SessionActivity, SessionNote, SessionSummary } from "../core/wire";

const NOW = 1_800_000_000_000;
const AS_OF = NOW - 60_000;

const summary = (id: string, fields: Partial<SessionSummary> = {}): SessionSummary => ({
  id, title: id, title_pending: false, model: "sol", state: "idle", unread: false, has_draft: false,
  last_activity_unix_ms: AS_OF - 1000, workspace: "/work", ...fields,
});

const activity = (session: string, fields: Partial<SessionActivity> = {}): SessionActivity => ({
  session, status: null, turn_elapsed_ns: null, prompt: null, message: null, tool: null,
  turn: { tools: 0, failed: 0, patches: 0 }, plan: null, error: null, agents: { running: 0, total: 0 }, queued: 0, context: null,
  ...fields,
});

const note = (session: string, attention: SessionNote["attention"], health: number | null = null): SessionNote =>
  ({ session, attention, goal: null, health, previous_health: null, blocks: [], reason: null, suggestion: null });
const check = (...notes: SessionNote[]): MonitorCheck => ({ as_of_ms: AS_OF, generated_at_ms: AS_OF + 5000, model: "luna", sessions: notes });
const running = { state: "running", last_activity_unix_ms: NOW - 1000 } as const;

test("sessions are grouped by what they need, newest opened first within a group, whatever their concern", () => {
  const live = [
    summary("idle"),
    summary("working-1", running),
    summary("error", { state: "error" }),
    summary("ready"),
    summary("unread", { unread: true }),
    summary("quiet", { state: "running", last_activity_unix_ms: NOW - QUIET_MS }),
    summary("needs-input"),
    summary("stuck", running),
    summary("working-2", running),
    summary("off-track", running),
    summary("retrying", running),
  ];
  const activities = new Map([["retrying", activity("retrying", { status: { kind: "retrying", delay_ns: 1e9, next_attempt: 2, max_attempts: 5 } })]]);
  const notes = check(note("ready", "ready"), note("off-track", "off_track"), note("stuck", "stuck"), note("needs-input", "needs_input"));

  const ranked = rankSessions(live, activities, notes, NOW);

  expect(ranked.map(({ summary, assessment }) => [summary.id, assessment.concern, assessment.group])).toEqual([
    ["retrying", "retrying", "needs"],
    ["off-track", "off_track", "needs"],
    ["stuck", "stuck", "needs"],
    ["needs-input", "needs_input", "needs"],
    ["quiet", "quiet", "needs"],
    ["unread", "unread", "needs"],
    ["error", "error", "needs"],
    ["working-2", "working", "working"],
    ["working-1", "working", "working"],
    ["ready", "ready", "idle"],
    ["idle", "idle", "idle"],
  ]);
  expect(monitorCounts(live, activities, notes, NOW)).toEqual({ needs: 7, running: 6 });
});

test("ordered by health, the lowest score comes first, unscored sessions last, and ties newest opened first", () => {
  const live = [summary("none"), summary("low"), summary("high"), summary("unscored"), summary("tie-old"), summary("tie-new")];
  const notes = check(note("low", "ok", 12), note("high", "needs_input", 95), note("unscored", "ok"), note("tie-old", "ok", 60), note("tie-new", "stuck", 60));
  const order = (order: "attention" | "health") => rankSessions(live, new Map(), notes, NOW, order).map(({ summary }) => summary.id);
  expect(order("health")).toEqual(["low", "tie-new", "tie-old", "high", "unscored", "none"]);
  // The attention order ignores health: the sessions that need the user lead, each group newest opened first.
  expect(order("attention")).toEqual(["tie-new", "high", "tie-old", "unscored", "low", "none"]);
});

test("an error reported by the activity outranks the monitor model's verdict", () => {
  const session = summary("a");
  expect(assess(session, activity("a", { error: "stream disconnected" }), check(note("a", "needs_input")), NOW).concern).toBe("error");
  expect(assess(summary("a", running), activity("a", { status: { kind: "error", message: "boom" } }), null, NOW).concern).toBe("error");
});

test("concerns rank error, needs input, stuck, off track, unread, retrying, quiet, ready, working, idle", () => {
  const unread = summary("a", { unread: true });
  const retrying = activity("a", { status: { kind: "retrying", delay_ns: 1e9, next_attempt: 2, max_attempts: 5 } });
  const concern = (session: SessionSummary, attention: SessionNote["attention"] | null, state?: SessionActivity) =>
    assess(session, state, attention ? check(note("a", attention)) : null, NOW).concern;

  expect(concern(unread, "needs_input")).toBe("needs_input");
  // A reply nobody has read hides neither the monitor's judgement that the work is stuck nor that it is off track.
  expect(concern(unread, "stuck")).toBe("stuck");
  expect(concern(unread, "off_track")).toBe("off_track");
  expect(concern(unread, "ready")).toBe("unread");
  expect(concern(summary("a", running), "off_track", retrying)).toBe("off_track");
  expect(concern(summary("a", running), "ok", retrying)).toBe("retrying");
  // A verdict older than the reply describes the turn before it, so the reply is what is new.
  expect(concern(summary("a", { unread: true, last_activity_unix_ms: AS_OF + 1 }), "off_track")).toBe("unread");
});

test("waiting on the user and being ready describe an idle agent, so a running session ignores them", () => {
  const notes = check(note("a", "needs_input"), note("b", "ready"));
  expect(assess(summary("a", running), undefined, notes, NOW).concern).toBe("working");
  expect(assess(summary("b", running), undefined, notes, NOW).concern).toBe("working");
  expect(assess(summary("a"), undefined, notes, NOW).concern).toBe("needs_input");
  expect(assess(summary("b"), undefined, notes, NOW).concern).toBe("ready");
});

test("a note older than the session's latest activity is marked earlier and stops counting once the session is idle", () => {
  const notes = check(note("a", "needs_input"), note("b", "stuck"));
  const later = { last_activity_unix_ms: AS_OF + 1 };

  const idle = assess(summary("a", later), undefined, notes, NOW);
  expect(idle).toMatchObject({ concern: "idle", noteEarlier: true, note: { attention: "needs_input" } });
  expect(assess(summary("b", later), undefined, notes, NOW).concern).toBe("idle");
  expect(assess(summary("b", { ...running, ...later }), undefined, notes, NOW)).toMatchObject({ concern: "stuck", noteEarlier: true });
  expect(assess(summary("b"), undefined, notes, NOW).noteEarlier).toBe(false);
});

test("a running session is quiet only after ten minutes with no call in flight and no agent at work", () => {
  const silent = summary("a", { state: "running", last_activity_unix_ms: NOW - QUIET_MS });
  expect(assess(silent, activity("a"), null, NOW).concern).toBe("quiet");
  expect(assess(silent, undefined, null, NOW).concern).toBe("quiet");
  expect(assess(summary("a", { state: "running", last_activity_unix_ms: NOW - QUIET_MS + 1 }), activity("a"), null, NOW).concern).toBe("working");
  const tool = { name: "exec_command", summary: "cargo test", state: "running" as const, at_ms: null, outcome: null };
  expect(assess(silent, activity("a", { tool }), null, NOW).concern).toBe("working");
  expect(assess(silent, activity("a", { agents: { running: 1, total: 2 } }), null, NOW).concern).toBe("working");
});
