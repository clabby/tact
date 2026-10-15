import { expect, test } from "bun:test";
import { MockRefusal, MockTact } from "./dev-mock";
import type { SessionSnapshot, StreamEvents, WireEntry } from "../src/core/wire";

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

  mock.command("set_draft", { session, text: "hello" }, 7);
  const { rev } = drafts.at(-1)!;
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
