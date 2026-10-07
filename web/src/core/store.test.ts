import { expect, test } from "bun:test";
import { initialState, reduce, type AppState } from "./store";
import type { SessionSnapshot, StreamEvent, WireEntry } from "./wire";

function snapshot(session: string, entries: WireEntry[] = []): SessionSnapshot {
  return {
    session, title: `Session ${session}`, model: "sol", effort: "high", reasoning_mode: "standard", speed: "standard",
    entries, status: null, queue: [], draft: { rev: 1, text: "", images: [] }, running: false, context: null,
    subagents: { max_subagents: 4, agents: [] },
  };
}

const user = (id: number, revision: number, text: string): WireEntry => ({ id, revision, parent: null, kind: "user", text });

function connected(...events: StreamEvent[]): AppState {
  const state = initialState();
  reduce(state, { type: "snapshot", data: snapshot("a", [user(1, 1, "hello")]) });
  for (const event of events) reduce(state, event);
  return state;
}

test("entries are upserted by id and revision; older revisions are ignored", () => {
  const state = connected();

  expect(reduce(state, { type: "entry", data: { session: "a", entry: user(2, 1, "next") } }))
    .toEqual([{ type: "entry", id: 2, added: true }]);
  expect(reduce(state, { type: "entry", data: { session: "a", entry: user(2, 3, "newer") } }))
    .toEqual([{ type: "entry", id: 2, added: false }]);
  expect(reduce(state, { type: "entry", data: { session: "a", entry: user(2, 2, "late") } })).toEqual([]);
  expect(reduce(state, { type: "entry", data: { session: "a", entry: user(2, 3, "duplicate") } })).toEqual([]);

  expect(state.session!.order).toEqual([1, 2]);
  expect(state.session!.entries.get(2)).toMatchObject({ revision: 3, text: "newer" });
});

test("entries arriving out of order are placed by id", () => {
  const state = connected();
  reduce(state, { type: "entry", data: { session: "a", entry: user(5, 1, "five") } });
  reduce(state, { type: "entry", data: { session: "a", entry: user(3, 1, "three") } });

  expect(state.session!.order).toEqual([1, 3, 5]);
});

test("events for a session other than the displayed one are ignored", () => {
  const state = connected();

  expect(reduce(state, { type: "entry", data: { session: "b", entry: user(9, 1, "stray") } })).toEqual([]);
  expect(reduce(state, { type: "draft", data: { session: "b", rev: 9, text: "x", origin: "terminal" } })).toEqual([]);
  expect(state.session!.entries.has(9)).toBe(false);
});

test("live updates do not disturb the transcript", () => {
  const state = connected();
  const session = state.session!;
  const entries = session.entries;

  const changes = reduce(state, {
    type: "live",
    data: {
      active: "a",
      sessions: [{ id: "a", title: "Session a", model: "sol", state: "running", unread: false, has_draft: false, last_activity_unix_ms: 1, workspace: "/src/tact" }],
    },
  });

  expect(changes).toEqual([{ type: "live" }]);
  expect(state.session).toBe(session);
  expect(state.session!.entries).toBe(entries);
  expect(state.live).toHaveLength(1);
});

test("a stale draft revision is dropped and a newer one records its origin", () => {
  const state = connected();

  expect(reduce(state, { type: "draft", data: { session: "a", rev: 1, text: "old", origin: "terminal" } })).toEqual([]);
  expect(reduce(state, { type: "draft", data: { session: "a", rev: 2, text: "new", origin: "web:7" } }))
    .toEqual([{ type: "draft" }]);
  expect(state.session!.draft).toEqual({ rev: 2, text: "new", images: [], origin: "web:7" });
});

test("closing the displayed session clears it until the next active snapshot", () => {
  const state = connected(
    { type: "live", data: { active: "a", sessions: [] } },
  );

  const closed = reduce(state, { type: "closed", data: { session: "a" } });
  expect(closed).toEqual([{ type: "live" }, { type: "session" }]);
  expect(state.session).toBeNull();
  expect(state.active).toBeNull();

  reduce(state, { type: "active", data: { session: "b" } });
  reduce(state, { type: "snapshot", data: snapshot("b", [user(1, 1, "other")]) });
  expect(state.active).toBe("b");
  expect(state.session!.id).toBe("b");
  expect(state.session!.entries.get(1)).toMatchObject({ text: "other" });
});

test("a snapshot keeps only the newest revision of duplicated entries", () => {
  const state = initialState();
  reduce(state, { type: "snapshot", data: snapshot("a", [user(1, 2, "new"), user(1, 1, "old")]) });

  expect(state.session!.order).toEqual([1]);
  expect(state.session!.entries.get(1)).toMatchObject({ text: "new" });
});

test("hello opens the connection and records the protocol version", () => {
  const state = initialState();
  reduce(state, { type: "hello", data: { protocol_version: 8, client_hint: "x" } });

  expect(state.connection).toBe("open");
  expect(state.protocolVersion).toBe(8);
});

test("subagent entries are upserted per agent without touching the main transcript", () => {
  const state = connected();
  const order = [...state.session!.order];

  expect(reduce(state, { type: "subagent_entry", data: { session: "a", agent: 3, entry: user(1, 1, "child") } }))
    .toEqual([{ type: "subagent_entry", agent: 3, id: 1 }]);
  expect(reduce(state, { type: "subagent_entry", data: { session: "a", agent: 3, entry: user(1, 1, "dup") } })).toEqual([]);

  expect(state.session!.agents.get(3)!.entries.get(1)).toMatchObject({ text: "child" });
  expect(state.session!.order).toEqual(order);
});

test("context, roster, and settings events update the session", () => {
  const state = connected();

  reduce(state, { type: "context", data: { session: "a", active_tokens: 1000, window_tokens: 4000 } });
  reduce(state, { type: "subagents", data: { session: "a", max_subagents: 2, agents: [] } });
  reduce(state, { type: "settings", data: { session: "a", model: "opus-5.5", effort: "max", reasoning_mode: "pro", speed: "fast" } });

  expect(state.session).toMatchObject({
    context: { active_tokens: 1000, window_tokens: 4000 },
    subagents: { max_subagents: 2, agents: [] },
    model: "opus-5.5", effort: "max", reasoningMode: "pro", speed: "fast",
  });
});
