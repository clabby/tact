import { expect, test } from "bun:test";
import { MockRefusal, MockTact } from "./dev-mock";
import { healthReading, healthTrend } from "../src/monitor/health";
import type { MonitorState, SessionSnapshot, StreamEvents, WireEntry } from "../src/core/wire";

test("renaming a mock session targets its ID and publishes its canonical title", () => {
  const mock = new MockTact();
  const session = mock.active!;
  const updates: StreamEvents["live"][] = [];
  mock.subscribe((name, data) => {
    if (name === "live") updates.push(data as StreamEvents["live"]);
  });
  const { session: other } = mock.command("open_session", { new: {} }, 1) as { session: string };
  mock.command("rename_session", { session, title: "  Chosen title  " }, 1);
  expect(updates.at(-1)!.sessions.find((summary) => summary.id === session)?.title).toBe("Chosen title");
  expect(updates.at(-1)!.sessions.find((summary) => summary.id === other)?.title).toBe("New chat");
  expect(mock.active).toBe(other);
  mock.command("activate", { session }, 1);
  const snapshot = mock.greeting().find((event) => event.name === "snapshot")!.data as SessionSnapshot;
  expect(snapshot.title).toBe("Chosen title");
  expect(() => mock.command("rename_session", { session, title: "   " }, 1)).toThrow(MockRefusal);
});

test("the mock follows the shared-draft contract: tagged echoes and revision-checked submit", async () => {
  const mock = new MockTact();
  mock.pace = 0;
  const session = mock.active!;
  const drafts: { rev: number; text: string; origin: string | null }[] = [];
  mock.subscribe((name, data) => {
    if (name === "draft") drafts.push(data as (typeof drafts)[number]);
  });

  const receipt = mock.command("set_draft", { session, text: "hello" }, 7);
  const { rev } = drafts.at(-1)!;
  expect(receipt).toEqual({ rev });
  expect(mock.command("set_draft", { session, text: "hello" }, 7)).toEqual(receipt);
  expect(drafts).toHaveLength(1);
  expect(drafts.at(-1)).toMatchObject({ text: "hello", origin: "web:7" });

  expect(() => mock.command("submit", { session, rev: rev - 1 }, 7)).toThrow(MockRefusal);
  mock.command("submit", { session, rev }, 7);
  expect(drafts.at(-1)).toMatchObject({ rev: rev + 1, text: "" });

  mock.command("interrupt", { session }, 7);
});

test("the mock offers the repository's checkouts and opens chats in them", () => {
  const mock = new MockTact();
  mock.pace = 0;
  const reply = mock.workspaces(mock.active!);
  expect(reply.checkouts.map((checkout) => [checkout.label, checkout.kind, checkout.current])).toEqual([
    ["main", "git", true], ["ws2", "git", false], ["review-ui", "jj", false],
  ]);
  const ws2 = reply.checkouts[1]!.path;
  const { session } = mock.command("open_session", { new: { workspace: ws2 } }, 1) as { session: string };
  expect(mock.live().sessions.find((summary) => summary.id === session)?.workspace).toBe(ws2);
  expect(mock.workspaces(session).checkouts.find((checkout) => checkout.current)?.path).toBe(ws2);
  expect(() => mock.command("open_session", { new: { workspace: "/tmp/elsewhere" } }, 1)).toThrow(MockRefusal);
  expect(() => mock.reviewCheckout(session, "/tmp/elsewhere")).toThrow(MockRefusal);
  expect(mock.reviewCheckout(session, undefined).path).toBe(ws2);
});

test("the mock publishes agent threads to the session and to each participant's transcript", () => {
  const mock = new MockTact();
  const session = mock.active!;
  const threads = (entries: WireEntry[]) =>
    entries.flatMap((entry) => (entry.kind === "directed_message" ? [entry.thread] : []));
  const snapshot = mock.greeting().find((event) => event.name === "snapshot")!.data as SessionSnapshot;
  expect(threads(snapshot.entries)).toEqual([1, 2, 3]);
  expect(threads(mock.agentTranscript(session, 2).entries)).toEqual([1, 2]);
  expect(threads(mock.agentTranscript(session, 4).entries)).toEqual([1, 3]);
  const failed = snapshot.entries.find((entry) => entry.kind === "directed_message" && entry.thread === 3);
  expect(failed).toMatchObject({ delivery: "failed", messages: [{ from: 3, to: 4, delivery: "failed" }] });
});

test("a mock session's first prompt names it with a pending title until the generated one lands", async () => {
  const mock = new MockTact();
  mock.pace = 0;
  const { session } = mock.command("open_session", { new: {} }, 1) as { session: string };
  const titles: [string, boolean][] = [];
  mock.subscribe((name, data) => {
    const summary = name === "live" && (data as StreamEvents["live"]).sessions.find((candidate) => candidate.id === session);
    if (summary) titles.push([summary.title, summary.title_pending]);
  });
  mock.command("set_draft", { session, text: "Harden the flaky bridge test. It fails on CI." }, 1);
  mock.command("submit", { session, rev: 1 }, 1);
  expect(titles.at(-1)).toEqual(["Harden the flaky bridge test. It fails on CI.", true]);
  await Bun.sleep(5);
  expect(titles.at(-1)).toEqual(["Harden the flaky bridge test", false]);
});
test("a fresh stream learns every session's activity right after the live list", () => {
  const mock = new MockTact();
  const greeting = mock.greeting();
  expect(greeting.map((event) => event.name)).toEqual(["hello", "live", "activity", "active", "snapshot"]);
  expect(greeting[0]!.data).toMatchObject({ protocol_version: 11 });
  const { sessions } = greeting[2]!.data as StreamEvents["activity"];
  const live = (greeting[1]!.data as StreamEvents["live"]).sessions;
  expect(sessions.map((activity) => activity.session)).toEqual(live.map((summary) => summary.id));
  const main = sessions.find((activity) => activity.session === mock.active)!;
  // The newest plan was updated inside a Code Mode cell, after an older top-level one.
  expect(main).toMatchObject({
    status: { kind: "waiting_for_background_work" }, agents: { running: 3, total: 5 },
    plan: { done: 2, total: 4, current: "Make the reply wait for the loop to apply the command" },
  });
  expect(main.turn_elapsed_ns).toBeGreaterThan(0);
});

test("the mock broadcasts activity for sessions other than the active one", async () => {
  const mock = new MockTact();
  mock.pace = 0;
  const background = mock.live().sessions.find((summary) => summary.title === "Flaky release pipeline test")!.id;
  const activity: StreamEvents["activity"][] = [];
  mock.subscribe((name, data) => {
    if (name === "activity") activity.push(data as StreamEvents["activity"]);
  });
  const { rev } = mock.command("set_draft", { session: background, text: "Check the queue." }, 1) as { rev: number };
  mock.command("submit", { session: background, rev }, 1);
  await Bun.sleep(30);
  const prompts = activity.map((event) => event.sessions.find((entry) => entry.session === background)?.prompt?.text);
  expect(prompts).toContain("Check the queue.");
});

test("the mock monitor model runs one check at a time and notes every live session", async () => {
  const mock = new MockTact();
  mock.pace = 0;
  const states: MonitorState[] = [];
  mock.subscribe((name, data) => {
    if (name === "monitor") states.push(data as MonitorState);
  });
  expect(mock.monitor(false).running).toBe(true);
  expect(mock.monitor(true).running).toBe(true);
  await Bun.sleep(5);
  expect(states.map((candidate) => candidate.running)).toEqual([true, false]);
  const { check } = states.at(-1)!;
  expect(check!.sessions.map((note) => note.session)).toEqual(mock.live().sessions.map((summary) => summary.id));
  expect(check!.sessions.find((note) => note.attention === "needs_input")).toMatchObject({ reason: expect.any(String), suggestion: expect.any(String) });
  expect(check!.sessions.filter((note) => note.attention === "ok").every((note) => note.reason === null)).toBe(true);
});
test("a mock check describes the sessions as they were when it started, without the ones closed since", async () => {
  const mock = new MockTact();
  mock.pace = 0;
  const seeded = mock.live().sessions;
  const wire = seeded.find((summary) => summary.title.startsWith("Wire"))!.id;
  const closed = seeded.find((summary) => summary.title.startsWith("Sketch"))!.id;
  expect(mock.monitor(false).running).toBe(true);
  const { session: opened } = mock.command("open_session", { new: {} }, 1) as { session: string };
  mock.command("rename_session", { session: wire, title: "Renamed after the check started" }, 1);
  mock.command("close_session", { session: closed, force: true }, 1);
  await Bun.sleep(5);

  const notes = mock.monitor(false).check!.sessions;
  expect(notes.map((note) => note.session)).toEqual(seeded.map((summary) => summary.id).filter((id) => id !== closed));
  expect(notes.some((note) => note.session === opened)).toBe(false);
  expect(notes.find((note) => note.session === wire)!.goal).toBe("Make draft echoes follow the acknowledgement without blocking the loop");
});

test("the mock monitor model checks only while at least two sessions are live", () => {
  const mock = new MockTact();
  mock.pace = 0;
  const [kept, ...others] = mock.live().sessions.map((summary) => summary.id);
  for (const session of others) mock.command("close_session", { session, force: true }, 1);
  expect(mock.live().sessions.map((summary) => summary.id)).toEqual([kept]);
  expect([mock.monitor(false).running, mock.monitor(true).running]).toEqual([false, false]);
  mock.command("open_session", { new: {} }, 1);
  expect(mock.monitor(false).running).toBe(true);
});

test("the mock monitor model's notes give goals and show every block type and live source", async () => {
  const mock = new MockTact();
  mock.pace = 0;
  mock.monitor(false);
  await Bun.sleep(5);
  const notes = mock.monitor(false).check!.sessions;
  expect(notes.every((note) => note.goal !== null && note.blocks.length <= 4)).toBe(true);
  // The seeded sessions show these; the ones the screenshot script opens add progress and the prompt.
  const blocks = notes.flatMap((note) => note.blocks);
  expect(new Set(blocks.map((block) => block.type))).toEqual(new Set(["text", "metric", "checklist", "history", "question", "code", "live"]));
  expect(new Set(blocks.flatMap((block) => block.type === "live" ? [block.source] : []))).toEqual(new Set(["agents", "plan", "message", "tool", "error"]));
  expect(new Set(blocks.flatMap((block) => block.type === "history" ? block.items.map((item) => item.outcome) : []))).toEqual(new Set(["worked", "failed", "running", "unclear"]));
});

test("the mock monitor model's notes on the seeded sessions cover every health band and both trends", async () => {
  const mock = new MockTact();
  mock.pace = 0;
  mock.monitor(false);
  await Bun.sleep(5);
  const notes = mock.monitor(false).check!.sessions;
  const scored = notes.flatMap((note) => (note.health === null ? [] : [note.health]));
  expect(new Set(scored.map((health) => healthReading(health).segments))).toEqual(new Set([1, 2, 3, 4, 5]));
  expect(new Set(notes.map(healthTrend))).toEqual(new Set(["up", "down", null]));
});
