import { expect, test } from "bun:test";
import type { Connection } from "./store";
import { StreamClient, type EventSourceLike } from "./stream";
import { ManualTimers, settle } from "./test-support";
import type { StreamEvent } from "./wire";

class FakeSource implements EventSourceLike {
  onopen: ((event: Event) => void) | null = null;
  onerror: ((event: Event) => void) | null = null;
  closed = false;
  private listeners = new Map<string, (event: MessageEvent<string>) => void>();

  addEventListener(type: string, listener: (event: MessageEvent<string>) => void) {
    this.listeners.set(type, listener);
  }

  close() {
    this.closed = true;
  }

  emit(type: string, data: unknown) {
    this.listeners.get(type)?.({ data: JSON.stringify(data) } as MessageEvent<string>);
  }

  fail() {
    this.onerror?.(new Event("error"));
  }
}

function harness(authorized: () => Promise<boolean> = async () => true) {
  const timers = new ManualTimers();
  const sources: FakeSource[] = [];
  const events: StreamEvent[] = [];
  const connections: Connection[] = [];
  const client = new StreamClient({
    url: "/api/stream",
    connect: () => {
      const source = new FakeSource();
      sources.push(source);
      return source;
    },
    onEvent: (event) => events.push(event),
    onConnection: (connection) => connections.push(connection),
    authorized,
    timers,
    random: () => 1,
  });
  return { client, timers, sources, events, connections };
}

test("named events are parsed and delivered", () => {
  const { client, sources, events } = harness();
  client.start();

  sources[0]!.emit("hello", { protocol_version: 8, client_hint: "x" });
  sources[0]!.emit("workspace", { version: "3" });

  expect(events).toEqual([
    { type: "hello", data: { protocol_version: 8, client_hint: "x" } },
    { type: "workspace", data: { version: "3" } },
  ]);
});

test("a dropped stream reconnects with growing backoff that resets on hello", async () => {
  const { client, timers, sources, connections } = harness();
  client.start();

  sources[0]!.fail();
  await settle();
  expect(sources[0]!.closed).toBe(true);
  expect(connections).toEqual(["connecting", "reconnecting"]);
  expect(timers.scheduled).toEqual([500]);
  timers.advance(500);
  expect(sources).toHaveLength(2);

  sources[1]!.fail();
  await settle();
  expect(timers.scheduled).toEqual([1000]);
  timers.advance(1000);

  sources[2]!.emit("hello", { protocol_version: 8, client_hint: "x" });
  sources[2]!.fail();
  await settle();
  expect(timers.scheduled).toEqual([500]);
});

test("backoff is capped", async () => {
  const { client, timers, sources } = harness();
  client.start();
  for (let attempt = 0; attempt < 10; attempt += 1) {
    sources.at(-1)!.fail();
    await settle();
    timers.advance(timers.scheduled[0]!);
  }
  sources.at(-1)!.fail();
  await settle();
  expect(timers.scheduled).toEqual([15_000]);
});

test("a revoked login locks instead of retrying", async () => {
  const { client, timers, sources, connections } = harness(async () => false);
  client.start();

  sources[0]!.fail();
  await settle();

  expect(connections.at(-1)).toBe("locked");
  expect(timers.scheduled).toEqual([]);
});

test("an unreachable server keeps retrying", async () => {
  const { client, timers, sources } = harness(async () => { throw new Error("offline"); });
  client.start();

  sources[0]!.fail();
  await settle();

  expect(timers.scheduled).toHaveLength(1);
});

test("reconnectNow skips the remaining backoff; events from a replaced source are ignored", async () => {
  const { client, timers, sources, events } = harness();
  client.start();
  sources[0]!.fail();
  await settle();

  client.reconnectNow();
  expect(sources).toHaveLength(2);
  expect(timers.scheduled).toEqual([]);

  sources[0]!.emit("workspace", { version: "stale" });
  expect(events).toEqual([]);
});

test("stop closes the stream and cancels reconnection", async () => {
  const { client, timers, sources } = harness();
  client.start();
  sources[0]!.fail();
  await settle();

  client.stop();
  expect(timers.scheduled).toEqual([]);
});
