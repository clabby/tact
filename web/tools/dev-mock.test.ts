import { expect, test } from "bun:test";
import { MockRefusal, MockTact } from "./dev-mock";
import type { SessionSnapshot, WireEntry } from "../src/core/wire";

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
