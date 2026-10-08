import { expect, test } from "bun:test";
import { firstUnseen, latestEntry, newSinceLabel, readSeen, writeSeen } from "./seen";
import { transcript } from "./test-entries";

test("the first unseen entry is the first top-level one after the last seen", () => {
  const t = transcript();
  t.user("a");
  t.tool("exec", "2 tools");
  t.tool("read", "x", { parent: 2 });
  t.say("b");
  expect(firstUnseen(t.data(), { id: 2, at: 0 })).toBe(4);
  expect(firstUnseen(t.data(), { id: 4, at: 0 })).toBeNull();
  expect(latestEntry(t.data())).toBe(4);
});

test("the last seen entry round-trips through storage and tolerates junk", () => {
  const store = new Map<string, string>();
  const storage = { getItem: (key: string) => store.get(key) ?? null, setItem: (key: string, value: string) => void store.set(key, value) };
  expect(readSeen(storage, "s1")).toBeNull();
  writeSeen(storage, "s1", { id: 7, at: 1000 });
  expect(readSeen(storage, "s1")).toEqual({ id: 7, at: 1000 });
  store.set("tact.web.seen.s1", "{oops");
  expect(readSeen(storage, "s1")).toBeNull();
});

test("the marker says how long ago the reader last looked", () => {
  const now = Date.UTC(2026, 9, 8, 12);
  expect(newSinceLabel(now - 12 * 60_000, now)).toBe("New since 12m ago");
  expect(newSinceLabel(now - 10_000, now)).toBe("New since moments ago");
});
