import { expect, test } from "bun:test";
import { DraftSync } from "./draft";
import { ManualTimers, settle } from "../core/test-support";

const ORIGIN = "web:1";

function harness(initial = { rev: 1, text: "" }) {
  const timers = new ManualTimers();
  const writes: string[] = [];
  const remote: { text: string; origin: string | null }[] = [];
  let release: { resolve: () => void; reject: (error: Error) => void }[] = [];
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
      return new Promise<void>((resolve, reject) => release.push({ resolve, reject }));
    },
    onRemote: (text, origin) => remote.push({ text, origin }),
  });
  const acknowledge = async (error?: Error) => {
    const pending = release;
    release = [];
    for (const write of pending) {
      if (error) write.reject(error);
      else write.resolve();
    }
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

test("a restored session can submit after the server restarts its draft revisions", async () => {
  const { sync, timers, acknowledge } = harness({ rev: 40, text: "" });
  sync.receive({ rev: 0, text: "", origin: null });
  sync.edit("hello after restart");

  const revision = sync.settledRev();
  await settle();
  await acknowledge();
  sync.receive({ rev: 1, text: "hello after restart", origin: ORIGIN });
  timers.advance(3000);

  expect(await revision).toBe(1);
  expect(sync.text).toBe("hello after restart");
  expect(sync.dirty).toBe(false);
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

test("a revision reset preserves a draft saved in the old process", async () => {
  const { sync, writes, remote, acknowledge } = harness({ rev: 40, text: "keep this draft" });
  sync.receive({ rev: 0, text: "", origin: null });
  expect(sync.text).toBe("keep this draft");
  expect(sync.dirty).toBe(true);
  expect(remote).toEqual([]);

  const revision = sync.settledRev();
  await acknowledge();
  sync.receive({ rev: 1, text: "keep this draft", origin: ORIGIN });

  expect(await revision).toBe(1);
  expect(writes).toEqual(["keep this draft"]);
});

test("a reset waits for the old HTTP write before replaying the visible draft", async () => {
  for (const submitBeforeReset of [false, true]) {
    for (const oldWriteFails of [false, true]) {
      const { sync, timers, writes, acknowledge } = harness({ rev: 40, text: "" });
      sync.edit("mine");
      timers.advance(75);
      let revision = submitBeforeReset ? sync.settledRev() : null;
      sync.receive({ rev: 0, text: "", origin: null });
      revision ??= sync.settledRev();
      expect(writes).toEqual(["mine"]);

      await acknowledge(oldWriteFails ? new Error("old connection closed") : undefined);
      expect(writes).toEqual(["mine", "mine"]);
      await acknowledge();
      sync.receive({ rev: 1, text: "mine", origin: ORIGIN });

      expect(await revision).toBe(1);
      expect(sync.dirty).toBe(false);
    }
  }
});

test("a reset replays the latest edit queued behind an old write", async () => {
  const { sync, timers, writes, acknowledge } = harness({ rev: 40, text: "" });
  sync.edit("old");
  timers.advance(75);
  sync.edit("latest");
  timers.advance(75);
  sync.receive({ rev: 0, text: "", origin: null });
  const revision = sync.settledRev();

  await acknowledge();
  expect(writes).toEqual(["old", "latest"]);
  await acknowledge();
  sync.receive({ rev: 1, text: "latest", origin: ORIGIN });

  expect(await revision).toBe(1);
  expect(sync.text).toBe("latest");
});

test("a submit awaiting an old echo restarts when the revision counter resets", async () => {
  for (const timedOut of [false, true]) {
    const { sync, timers, writes, acknowledge } = harness({ rev: 40, text: "" });
    sync.edit("mine");
    let submitted = false;
    const revision = sync.settledRev().then((rev) => { submitted = true; return rev; });
    await acknowledge();
    expect(timers.scheduled).toEqual([3000]);

    if (timedOut) timers.advance(3000);
    sync.receive({ rev: 0, text: "", origin: null });
    await settle();
    expect(submitted).toBe(false);
    expect(writes).toEqual(["mine", "mine"]);
    await acknowledge();
    sync.receive({ rev: 1, text: "mine", origin: ORIGIN });

    expect(await revision).toBe(1);
    expect(timers.scheduled).toEqual([]);
  }
});

test("a reconnect snapshot can acknowledge an echo missed while disconnected", async () => {
  const { sync, timers, acknowledge } = harness();
  sync.edit("mine");
  const revision = sync.settledRev();
  await acknowledge();
  sync.receive({ rev: 2, text: "mine", origin: null });

  expect(await revision).toBe(2);
  expect(sync.dirty).toBe(false);
  expect(timers.scheduled).toEqual([]);
});

test("a missing echo after reset cannot submit the snapshot's different text", async () => {
  const { sync, timers, acknowledge } = harness({ rev: 40, text: "mine" });
  sync.receive({ rev: 0, text: "other", origin: null });
  const result = sync.settledRev().then(() => "submitted", (error: Error) => error.message);
  await acknowledge();
  timers.advance(3000);

  expect(await result).toBe("The draft has not been saved yet. Try sending again.");
  expect(sync.text).toBe("mine");
});

test("a reset preserves an active IME composition until its completed edit", async () => {
  const { sync, timers, writes, remote, acknowledge } = harness({ rev: 40, text: "base" });
  sync.compositionStart();
  sync.receive({ rev: 0, text: "", origin: null });
  expect(sync.isComposing).toBe(true);
  expect(writes).toEqual([]);
  expect(remote).toEqual([]);
  sync.compositionEnd("base漢");
  timers.advance(75);
  await acknowledge();
  sync.receive({ rev: 1, text: "base漢", origin: ORIGIN });

  expect(await sync.settledRev()).toBe(1);
  expect(writes).toEqual(["base漢"]);
});

test("a reset restores the shared draft without waiting for submit", async () => {
  const { sync, writes, remote, acknowledge } = harness({ rev: 40, text: "mine" });
  sync.receive({ rev: 0, text: "", origin: null });
  expect(writes).toEqual(["mine"]);
  await acknowledge();
  sync.receive({ rev: 1, text: "mine", origin: ORIGIN });
  expect(sync.dirty).toBe(false);
  sync.receive({ rev: 2, text: "from terminal", origin: "terminal" });
  expect(remote).toEqual([{ text: "from terminal", origin: "terminal" }]);
});

test("a reset replays after the old write finishes without waiting for submit", async () => {
  for (const oldWriteFails of [false, true]) {
    const { sync, timers, writes, acknowledge } = harness({ rev: 40, text: "" });
    sync.edit("mine");
    timers.advance(75);
    sync.receive({ rev: 0, text: "", origin: null });
    expect(writes).toEqual(["mine"]);

    await acknowledge(oldWriteFails ? new Error("old connection closed") : undefined);
    expect(writes).toEqual(["mine", "mine"]);
    await acknowledge();
    sync.receive({ rev: 1, text: "mine", origin: ORIGIN });
    expect(sync.dirty).toBe(false);
  }
});

test("an old write confirmed after reset needs no replay and releases remote edits", async () => {
  const { sync, timers, writes, remote, acknowledge } = harness({ rev: 40, text: "" });
  sync.edit("mine");
  timers.advance(75);
  sync.receive({ rev: 0, text: "", origin: null });
  sync.receive({ rev: 1, text: "mine", origin: ORIGIN });
  await acknowledge();

  expect(writes).toEqual(["mine"]);
  expect(sync.dirty).toBe(false);
  sync.receive({ rev: 2, text: "from terminal", origin: "terminal" });
  expect(remote).toEqual([{ text: "from terminal", origin: "terminal" }]);
});

test("ending an unchanged IME composition restores the reset draft", async () => {
  const { sync, writes, acknowledge } = harness({ rev: 40, text: "mine" });
  sync.compositionStart();
  sync.receive({ rev: 0, text: "", origin: null });
  expect(writes).toEqual([]);
  sync.compositionEnd("mine");
  expect(writes).toEqual(["mine"]);
  await acknowledge();
  sync.receive({ rev: 1, text: "mine", origin: ORIGIN });
  expect(sync.dirty).toBe(false);
});
