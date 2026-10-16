import { afterEach, beforeEach, expect, spyOn, test } from "bun:test";
import { Window, type ICanvasRenderingContext2D } from "happy-dom";
import type { ApiClient } from "../core/api-client";
import { initialState, reduce, type SessionView } from "../core/store";
import { ManualTimers, settle } from "../core/test-support";
import { Composer } from "./composer";

const nativeSetTimeout = globalThis.setTimeout;
const nativeClearTimeout = globalThis.clearTimeout;
let window: Window;
let timers: ManualTimers;
let restoreTimers: () => void;

beforeEach(() => {
  // These tests exercise draft synchronization without rendering the decorative spheres.
  window = new Window({
    settings: {
      canvasAdapter: {
        getContext: () => ({ clearRect() {}, beginPath() {}, arc() {}, fill() {} }) as unknown as ICanvasRenderingContext2D,
        toDataURL: () => "",
        toBlob() {},
      },
    },
  });
  Object.assign(globalThis, {
    document: window.document,
    window,
    innerHeight: window.innerHeight,
    devicePixelRatio: window.devicePixelRatio,
    getComputedStyle: window.getComputedStyle.bind(window),
    requestAnimationFrame: () => 1,
  });

  timers = new ManualTimers();
  const set = spyOn(globalThis, "setTimeout").mockImplementation(((callback: () => void, delay: number) => {
    if (delay === 75 || delay === 3000) return timers.set(callback, delay);
    return nativeSetTimeout(callback, delay);
  }) as typeof setTimeout);
  const clear = spyOn(globalThis, "clearTimeout").mockImplementation((handle) => {
    if (typeof handle === "number") timers.clear(handle);
    else nativeClearTimeout(handle);
  });
  restoreTimers = () => { set.mockRestore(); clear.mockRestore(); };
});

afterEach(async () => {
  restoreTimers();
  await window.happyDOM.close();
});

function session(id: string, rev = 1, text = ""): SessionView {
  const snapshot = {
    session: id, title: id, title_pending: false, model: "sol",
    effort: "high" as const, reasoning_mode: "standard" as const, speed: "standard" as const,
    status: null, queue: [], draft: { rev, text, images: [] }, running: false,
    context: null, subagents: { max_subagents: 0, agents: [] }, entries: [],
  };
  const state = initialState();
  reduce(state, { type: "snapshot", data: snapshot });
  return state.session!;
}

function harness() {
  const commands: { name: string; args: object }[] = [];
  let save: (receipt: { rev: number }) => void;
  const api = {
    origin: "web:1",
    command(name: string, args: object) {
      commands.push({ name, args });
      if (name === "set_draft") return new Promise<{ rev: number }>((resolve) => { save = resolve; });
      return Promise.resolve();
    },
  } as unknown as ApiClient;
  const root = document.createElement("div");
  document.body.replaceChildren(root);
  const composer = new Composer(root, {
    api, catalog: () => null, running: () => false, workspaceChip: () => null, actions: () => [],
    openRecentPrompts() {}, openContext() {}, openSubagents() {}, chooseWorkspace() {},
  });
  const form = root.querySelector("form")!;
  const textarea = root.querySelector("textarea")!;
  const message = root.querySelector<HTMLElement>(".composer-message")!;
  return { composer, form, textarea, message, commands, save: (rev = 2) => save({ rev }) };
}

test("an old session's unsaved submit does not warn the newly shown session", async () => {
  const { composer, form, textarea, message, commands, save } = harness();
  composer.show(session("old"));
  textarea.value = "pending draft";
  textarea.dispatchEvent(new window.Event("input", { bubbles: true }) as unknown as Event);
  form.requestSubmit();
  expect(commands).toEqual([{ name: "set_draft", args: { session: "old", text: "pending draft" } }]);

  composer.show(session("new", 1, "new session draft"));
  save();
  await settle();

  expect(textarea.value).toBe("new session draft");
  expect(message.hidden).toBe(true);
  expect(commands.filter((command) => command.name === "submit")).toEqual([]);
  expect(form.querySelector<HTMLButtonElement>(".send-button")!.disabled).toBe(false);
});

test("a same-session restart snapshot keeps visible text and sends its new revision", async () => {
  const { composer, form, textarea, message, commands, save } = harness();
  composer.show(session("same", 40, "send this"));
  const restarted = session("same", 0);
  composer.show(restarted);
  expect(textarea.value).toBe("send this");
  expect(commands).toEqual([{ name: "set_draft", args: { session: "same", text: "send this" } }]);

  form.requestSubmit();
  save(1);
  await settle();
  restarted.draft = { rev: 1, text: "send this", images: [], origin: "web:1" };
  composer.draftChanged();
  await settle();

  expect(commands).toEqual([
    { name: "set_draft", args: { session: "same", text: "send this" } },
    { name: "submit", args: { session: "same", rev: 1, queue: false } },
  ]);
  expect(textarea.value).toBe("send this");
  expect(message.hidden).toBe(true);
  expect(timers.scheduled).toEqual([]);
});

test("appended review text submits with the server's canonical line endings", async () => {
  for (const newline of ["\r\n", "\r"]) {
    const { composer, form, message, commands, save } = harness();
    const current = session("review");
    composer.show(current);
    composer.append(`[P2] First line.${newline}Second line.`);
    form.requestSubmit();

    const text = "[P2] First line.\nSecond line.";
    expect(commands).toEqual([{ name: "set_draft", args: { session: "review", text } }]);
    save();
    await settle();

    expect(commands.at(-1)).toEqual({ name: "submit", args: { session: "review", rev: 2, queue: false } });
    expect(message.hidden).toBe(true);
    expect(timers.scheduled).toEqual([]);
  }
});
