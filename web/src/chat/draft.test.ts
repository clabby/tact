import { expect, test } from "bun:test";
import { DraftSync } from "./draft";
import { ManualTimers, settle } from "../core/test-support";

const ORIGIN = "web:1";

function harness(initial = { rev: 1, text: "" }) {
  const timers = new ManualTimers();
  const writes: string[] = [];
  const remote: { text: string; origin: string | null }[] = [];
  let release: { resolve: (receipt: { rev: number }) => void; reject: (error: Error) => void }[] = [];
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
      return new Promise<{ rev: number }>((resolve, reject) => release.push({ resolve, reject }));
    },
    onRemote: (text, origin) => remote.push({ text, origin }),
  });
  const acknowledge = async (revision: number | Error = 2) => {
    const pending = release;
    release = [];
    for (const write of pending) {
      if (revision instanceof Error) write.reject(revision);
      else write.resolve({ rev: revision });
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
  await acknowledge(3);

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
  await acknowledge(3);
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
  await acknowledge(4);
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

test("submit uses the committed receipt without waiting for an echo", async () => {
  const { sync, timers, acknowledge } = harness();
  sync.edit("ship it");
  const revision = sync.settledRev();
  await acknowledge(7);

  expect(await revision).toBe(7);
  expect(sync.text).toBe("ship it");
  expect(sync.dirty).toBe(false);
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
  await acknowledge(1);
  sync.receive({ rev: 1, text: "hello after restart", origin: ORIGIN });

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
  await acknowledge(1);
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

      await acknowledge(oldWriteFails ? new Error("old connection closed") : 41);
      expect(writes).toEqual(["mine", "mine"]);
      await acknowledge(1);
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

  await acknowledge(1);
  expect(writes).toEqual(["old", "latest"]);
  await acknowledge(1);
  sync.receive({ rev: 1, text: "latest", origin: ORIGIN });

  expect(await revision).toBe(1);
  expect(sync.text).toBe("latest");
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

test("a failed replay cannot submit the snapshot's different text", async () => {
  const { sync, acknowledge } = harness({ rev: 40, text: "mine" });
  sync.receive({ rev: 0, text: "other", origin: null });
  const result = sync.settledRev().then(() => "submitted", (error: Error) => error.message);
  await acknowledge(new Error("offline"));
  await acknowledge(new Error("offline"));

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
  await acknowledge(1);
  sync.receive({ rev: 1, text: "base漢", origin: ORIGIN });

  expect(await sync.settledRev()).toBe(1);
  expect(writes).toEqual(["base漢"]);
});

test("a reset restores the shared draft without waiting for submit", async () => {
  const { sync, writes, remote, acknowledge } = harness({ rev: 40, text: "mine" });
  sync.receive({ rev: 0, text: "", origin: null });
  expect(writes).toEqual(["mine"]);
  await acknowledge(1);
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

    await acknowledge(oldWriteFails ? new Error("old connection closed") : 41);
    expect(writes).toEqual(["mine", "mine"]);
    await acknowledge(1);
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
  await acknowledge(1);
  sync.receive({ rev: 1, text: "mine", origin: ORIGIN });
  expect(sync.dirty).toBe(false);
});

test("a missed echo cannot pin the composer after a newer reconnect snapshot", async () => {
  const { sync, timers, writes, acknowledge } = harness();
  sync.edit("mine");
  timers.advance(75);
  await acknowledge();
  sync.receive({ rev: 3, text: "terminal later", origin: null });

  expect(sync.text).toBe("terminal later");
  expect(sync.dirty).toBe(false);
  expect(await sync.settledRev()).toBe(3);
  expect(writes).toEqual(["mine"]);
});

test("a snapshot queued before a save receipt neither resets nor replaces the saved draft", async () => {
  const { sync, timers, writes, remote, acknowledge } = harness();
  sync.edit("mine");
  timers.advance(75);
  await acknowledge(4);
  sync.receive({ rev: 3, text: "older terminal text", origin: null });

  expect(sync.text).toBe("mine");
  expect(await sync.settledRev()).toBe(4);
  expect(writes).toEqual(["mine"]);
  expect(remote).toEqual([]);
  sync.receive({ rev: 5, text: "newer terminal text", origin: "terminal" });
  expect(sync.text).toBe("newer terminal text");
});

test("a retry that saves unchanged server text can settle without any new event", async () => {
  const { sync, timers, writes, acknowledge } = harness();
  sync.edit("mine");
  timers.advance(75);
  await acknowledge(new Error("response lost after commit"));
  const revision = sync.settledRev();
  await acknowledge(2);

  expect(await revision).toBe(2);
  expect(sync.dirty).toBe(false);
  expect(writes).toEqual(["mine", "mine"]);
  sync.receive({ rev: 3, text: "later", origin: "terminal" });
  expect(sync.text).toBe("later");
});

test("a terminal edit after commit wins even when it arrives before the receipt", async () => {
  const { sync, timers, remote, acknowledge } = harness();
  sync.edit("mine");
  timers.advance(75);
  sync.receive({ rev: 3, text: "later", origin: "terminal" });
  expect(sync.text).toBe("mine");
  await acknowledge(2);

  expect(remote).toEqual([{ text: "later", origin: "terminal" }]);
  expect(sync.text).toBe("later");
  expect(await sync.settledRev()).toBe(3);
});

test("an old matching echo cannot acknowledge a newer in-flight edit", async () => {
  const { sync, timers, writes, acknowledge } = harness();
  sync.edit("first");
  timers.advance(75);
  await acknowledge(2);
  sync.edit("latest");
  const revision = sync.settledRev();
  sync.receive({ rev: 2, text: "first", origin: ORIGIN });
  expect(sync.dirty).toBe(true);
  expect(sync.text).toBe("latest");
  await acknowledge(3);

  expect(await revision).toBe(3);
  expect(writes).toEqual(["first", "latest"]);
  expect(sync.text).toBe("latest");
});

test("submit drains edits made while its save request is pending", async () => {
  const { sync, writes, acknowledge } = harness();
  sync.edit("first");
  const revision = sync.settledRev();
  sync.edit("latest");
  await acknowledge(2);
  expect(writes).toEqual(["first", "latest"]);
  await acknowledge(3);

  expect(await revision).toBe(3);
  expect(sync.text).toBe("latest");
});

test("composition started during a save prevents submitting the unfinished input", async () => {
  const { sync, writes, acknowledge } = harness();
  sync.edit("base");
  const revision = sync.settledRev().then(() => "submitted", () => "unsaved");
  sync.compositionStart();
  await acknowledge(2);
  expect(await revision).toBe("unsaved");
  sync.compositionEnd("base漢");
  const completed = sync.settledRev();
  await acknowledge(3);

  expect(await completed).toBe(3);
  expect(writes).toEqual(["base", "base漢"]);
  expect(sync.text).toBe("base漢");
});

test("an obsolete receipt or failure cannot dirty a matching reset snapshot", async () => {
  for (const outcome of [41, new Error("old request failed")]) {
    const { sync, timers, writes, acknowledge } = harness({ rev: 40, text: "" });
    sync.edit("mine");
    timers.advance(75);
    sync.receive({ rev: 0, text: "mine", origin: null });
    await acknowledge(outcome);

    expect(await sync.settledRev()).toBe(0);
    expect(sync.dirty).toBe(false);
    expect(writes).toEqual(["mine"]);
  }
});

test("an edit whose debounce expires during composition is saved when composition ends", async () => {
  const { sync, timers, writes, acknowledge } = harness();
  sync.edit("mine");
  sync.compositionStart();
  timers.advance(75);
  sync.receive({ rev: 2, text: "terminal", origin: "terminal" });
  sync.compositionEnd("mine");

  expect(writes).toEqual(["mine"]);
  await acknowledge(3);
  expect(sync.text).toBe("mine");
  expect(await sync.settledRev()).toBe(3);
});

test("a delayed historical value cannot acknowledge a failed edit back to that value", async () => {
  for (const origin of [ORIGIN, null]) {
    const { sync, timers, writes, acknowledge } = harness();
    sync.edit("first");
    timers.advance(75);
    await acknowledge(2);
    sync.edit("second");
    timers.advance(75);
    await acknowledge(3);
    sync.edit("first");
    timers.advance(75);
    await acknowledge(new Error("offline before commit"));

    sync.receive({ rev: 2, text: "first", origin });
    sync.receive({ rev: 3, text: "second", origin: ORIGIN });
    expect(sync.text).toBe("first");
    expect(sync.dirty).toBe(true);
    sync.retry();
    expect(writes).toEqual(["first", "second", "first", "first"]);
    await acknowledge(4);
    expect(sync.dirty).toBe(false);
    expect(await sync.settledRev()).toBe(4);
  }
});

test("composition defers an edit queued behind a completed write without losing its retry", async () => {
  for (const terminalEdit of [false, true]) {
    const { sync, timers, writes, remote, acknowledge } = harness();
    sync.edit("a");
    timers.advance(75);
    sync.edit("ab");
    timers.advance(75);
    const flushing = sync.flush();
    sync.compositionStart();
    await acknowledge(2);
    await flushing;
    if (terminalEdit) sync.receive({ rev: 3, text: "terminal", origin: "terminal" });
    sync.compositionEnd("ab");

    expect(sync.text).toBe("ab");
    expect(remote).toEqual([]);
    expect(writes).toEqual(["a", "ab"]);
    await acknowledge(terminalEdit ? 4 : 3);
    expect(sync.dirty).toBe(false);
    expect(await sync.settledRev()).toBe(terminalEdit ? 4 : 3);
  }
});
