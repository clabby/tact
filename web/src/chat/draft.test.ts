import { expect, test } from "bun:test";
import { DraftSync } from "./draft";
import { ManualTimers, settle } from "../core/test-support";

const ORIGIN = "web:1";

function harness(initial = { rev: 1, text: "" }) {
  const timers = new ManualTimers();
  const writes: string[] = [];
  const remote: { text: string; origin: string | null }[] = [];
  let release: (() => void)[] = [];
  let failNext = false;
  const sync = new DraftSync(initial, {
    origin: ORIGIN,
    timers,
    delay: 75,
    write: (text) => {
      writes.push(text);
      if (failNext) {
        failNext = false;
        return Promise.reject(new Error("offline"));
      }
      return new Promise<void>((resolve) => release.push(resolve));
    },
    onRemote: (text, origin) => remote.push({ text, origin }),
  });
  const acknowledge = async () => {
    const pending = release;
    release = [];
    for (const resolve of pending) resolve();
    await settle();
  };
  return { sync, timers, writes, remote, acknowledge, failNextWrite: () => { failNext = true; } };
}

test("local edits are written at most once per debounce interval, latest text wins", async () => {
  const { sync, timers, writes, acknowledge } = harness();

  sync.edit("h");
  sync.edit("he");
  sync.edit("hel");
  expect(writes).toEqual([]);
  timers.advance(75);
  expect(writes).toEqual(["hel"]);

  sync.edit("hell");
  sync.edit("hello");
  timers.advance(75);
  expect(writes).toEqual(["hel"]);
  await acknowledge();
  expect(writes).toEqual(["hel", "hello"]);
});

test("the echo of this tab's own writes is never applied", async () => {
  const { sync, timers, remote, acknowledge } = harness();

  sync.edit("a");
  timers.advance(75);
  sync.edit("ab");
  await acknowledge();
  timers.advance(75);
  await acknowledge();

  sync.receive({ rev: 2, text: "a", origin: ORIGIN });
  sync.receive({ rev: 3, text: "ab", origin: ORIGIN });

  expect(remote).toEqual([]);
  expect(sync.text).toBe("ab");
  expect(sync.dirty).toBe(false);
});

test("a remote draft applies at once when the composer is quiet", () => {
  const { sync, remote } = harness();

  sync.receive({ rev: 2, text: "from terminal", origin: "terminal" });

  expect(remote).toEqual([{ text: "from terminal", origin: "terminal" }]);
  expect(sync.text).toBe("from terminal");
});

test("a remote draft is held during IME composition and applied after it if still newest", () => {
  const { sync, remote } = harness({ rev: 1, text: "x" });

  sync.compositionStart();
  sync.receive({ rev: 2, text: "terminal text", origin: "terminal" });
  expect(remote).toEqual([]);

  sync.compositionEnd("x");
  expect(remote).toEqual([{ text: "terminal text", origin: "terminal" }]);
});

test("a remote draft is held while a local edit is pending and dropped if the local write lands later", async () => {
  const { sync, timers, remote, acknowledge } = harness();

  sync.edit("mine");
  sync.receive({ rev: 2, text: "theirs", origin: "terminal" });
  expect(remote).toEqual([]);

  timers.advance(75);
  await acknowledge();
  sync.receive({ rev: 3, text: "mine", origin: ORIGIN });

  expect(remote).toEqual([]);
  expect(sync.text).toBe("mine");
});

test("a remote draft that lands after the local write wins once local writes drain", async () => {
  const { sync, timers, remote, acknowledge } = harness();

  sync.edit("mine");
  timers.advance(75);
  await acknowledge();
  sync.receive({ rev: 2, text: "mine", origin: ORIGIN });
  sync.edit("mine!");
  sync.receive({ rev: 3, text: "theirs", origin: "web:9" });
  expect(remote).toEqual([]);

  timers.advance(75);
  await acknowledge();
  // The server applied "mine!" after "theirs", so "theirs" is superseded.
  sync.receive({ rev: 4, text: "mine!", origin: ORIGIN });
  expect(remote).toEqual([]);
  expect(sync.text).toBe("mine!");
});

test("an own-origin change this tab did not type (a submit clearing the draft) is applied", () => {
  const { sync, remote } = harness({ rev: 4, text: "send me" });

  sync.receive({ rev: 5, text: "", origin: ORIGIN });

  expect(remote).toEqual([{ text: "", origin: ORIGIN }]);
});

test("submit waits for the echo of the last write and uses its revision", async () => {
  const { sync, timers, acknowledge } = harness();
  sync.edit("ship it");

  const rev = sync.settledRev();
  await settle();
  await acknowledge();
  sync.receive({ rev: 7, text: "ship it", origin: ORIGIN });

  expect(await rev).toBe(7);
  expect(timers.scheduled).toEqual([]);
});

test("stale revisions are ignored", () => {
  const { sync, remote } = harness({ rev: 5, text: "current" });

  sync.receive({ rev: 4, text: "old", origin: "terminal" });

  expect(remote).toEqual([]);
  expect(sync.text).toBe("current");
});

test("a failed write keeps the draft dirty and is retried", async () => {
  const { sync, timers, writes, failNextWrite, acknowledge } = harness();
  failNextWrite();

  sync.edit("unsaved");
  timers.advance(75);
  await settle();
  expect(sync.dirty).toBe(true);

  sync.receive({ rev: 2, text: "snapshot", origin: null });
  expect(sync.text).toBe("unsaved");

  sync.retry();
  await acknowledge();
  expect(writes).toEqual(["unsaved", "unsaved"]);
});
