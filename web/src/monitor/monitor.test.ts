import { afterAll, afterEach, beforeAll, expect, test } from "bun:test";
import { Window } from "happy-dom";
import { Store } from "../core/store";
import type { ModelCatalog, MonitorState, SessionActivity, SessionNote, SessionSummary } from "../core/wire";
import type { Monitor as MonitorClass, MonitorHost } from "./monitor";

let Monitor: typeof MonitorClass;
let dom: Window;
let frames: FrameRequestCallback[] = [];
let monitor: MonitorClass | null = null;
/** The globals this file replaces, restored afterwards because later test files share the process. */
let replaced = new Map<string, PropertyDescriptor | undefined>();

/** Stands in for the browser's ResizeObserver, which happy-dom provides without layout. */
class FakeResizeObserver {
  static observers: FakeResizeObserver[] = [];
  readonly targets: Element[] = [];
  constructor(readonly callback: () => void) {
    FakeResizeObserver.observers.push(this);
  }
  observe(target: Element) {
    this.targets.push(target);
  }
  unobserve(target: Element) {
    const index = this.targets.indexOf(target);
    if (index >= 0) this.targets.splice(index, 1);
  }
  disconnect() {
    this.targets.length = 0;
  }
}

beforeAll(async () => {
  dom = new Window();
  // The running marker draws on a canvas, which happy-dom does not implement.
  dom.HTMLCanvasElement.prototype.getContext = (() => ({ clearRect() {}, beginPath() {}, arc() {}, fill() {} })) as never;
  const globals = {
    document: dom.document,
    // The actions menu places itself inside the viewport and closes when the window resizes.
    window: dom,
    innerWidth: 1440,
    innerHeight: 900,
    HTMLElement: dom.HTMLElement,
    devicePixelRatio: 1,
    getComputedStyle: (element: Element) => dom.getComputedStyle(element as never),
    getSelection: () => dom.getSelection(),
    matchMedia: (query: string) => dom.matchMedia(query),
    requestAnimationFrame: (callback: FrameRequestCallback) => frames.push(callback),
    ResizeObserver: FakeResizeObserver,
  };
  replaced = new Map(Object.keys(globals).map((key) => [key, Object.getOwnPropertyDescriptor(globalThis, key)]));
  Object.assign(globalThis, globals);
  ({ Monitor } = await import("./monitor"));
});

afterAll(() => {
  for (const [key, descriptor] of replaced) {
    if (descriptor) Object.defineProperty(globalThis, key, descriptor);
    else delete (globalThis as Record<string, unknown>)[key];
  }
});

afterEach(() => {
  monitor?.setVisible(false);
  monitor = null;
  // The running markers share one animation loop, which ends once a frame finds none on the page.
  document.body.replaceChildren();
  while (frames.length) flush();
  FakeResizeObserver.observers = [];
});

const NOW = Date.now();

const summary = (id: string, fields: Partial<SessionSummary> = {}): SessionSummary => ({
  id, title: `Session ${id}`, title_pending: false, model: "luna", state: "idle", unread: false, has_draft: false,
  last_activity_unix_ms: NOW - 120_000, workspace: "/work", ...fields,
});

const activity = (session: string, fields: Partial<SessionActivity> = {}): SessionActivity => ({
  session, status: null, turn_elapsed_ns: null, prompt: null, message: null, tool: null,
  turn: { tools: 0, failed: 0, patches: 0 }, plan: null, error: null, agents: { running: 0, total: 0 }, queued: 0, context: null,
  ...fields,
});

const monitorState: MonitorState = {
  availability: "ready", detail: null, running: false, error: null, interval_seconds: 120,
  check: {
    as_of_ms: NOW - 60_000, generated_at_ms: NOW - 50_000, model: "luna",
    sessions: [{
      session: "ask", attention: "needs_input", goal: "Pick a config loader", health: 74, previous_health: 52,
      blocks: [
        { type: "question", text: "Which loader do you prefer?", options: ["Keep TOML", "Switch to figment"] },
        {
          type: "history",
          items: [
            { action: "Rebuild the crate", outcome: "running", detail: null },
            { action: "Ran the config tests", outcome: "failed", detail: "2 of 9 failed" },
            { action: "Compared figment", outcome: "unclear", detail: null },
            { action: "Found the TOML call sites", outcome: "worked", detail: "1 site" },
          ],
        },
      ],
      reason: "It ends with a question.", suggestion: "Keep the TOML loader.",
    }],
  },
};

const askNote = monitorState.check!.sessions[0]!;

/** Replaces the check's notes and renders the result. */
function showNotes(store: Store, ...sessions: SessionNote[]) {
  store.dispatch({ type: "monitor", data: { ...monitorState, check: { ...monitorState.check!, sessions } } });
  monitor!.changed();
  flush();
}

/** Replaces one session's activity, keeping the others, and renders the result. */
function changeActivity(store: Store, session: string, fields: Partial<SessionActivity>) {
  const sessions = [...store.state.activity.values()];
  store.dispatch({ type: "activity", data: { sessions: sessions.map((entry) => entry.session === session ? { ...entry, ...fields } : entry) } });
  monitor!.changed();
  flush();
}

const catalog = { models: [{ id: "luna", label: "Luna" }], efforts: [], speeds: [] } as unknown as ModelCatalog;

/** A visible Monitor over four sessions: one waiting on a question, one running, one with a draft, and the active one. */
function mount(overrides: Partial<MonitorHost> = {}, apiOverrides: Record<string, unknown> = {}) {
  const calls: string[] = [];
  const store = new Store();
  store.dispatch({
    type: "live",
    data: {
      active: "mine",
      sessions: [
        summary("mine"),
        summary("busy", { state: "running", last_activity_unix_ms: NOW }),
        summary("draft", { has_draft: true }),
        summary("ask", { workspace: "/work-2" }),
      ],
    },
  });
  store.dispatch({
    type: "activity",
    data: {
      sessions: [
        activity("mine"),
        activity("busy", {
          turn_elapsed_ns: 75e9,
          tool: { name: "exec_command", summary: "cargo test -p tact", state: "running", at_ms: NOW, outcome: null },
          turn: { tools: 12, failed: 1, patches: 2 },
          plan: { done: 2, total: 4, current: "Run the focused tests" },
        }),
        activity("draft"),
        activity("ask", { message: { text: "Which loader do you prefer?", commentary: false, at_ms: NOW - 120_000 } }),
      ],
    },
  });
  store.dispatch({ type: "monitor", data: monitorState });
  const api = {
    monitor: async () => monitorState,
    command: async (name: string, args: { session: string; text?: string; rev?: number }) => {
      calls.push([name, args.session, args.text ?? args.rev].join(" "));
      return name === "set_draft" ? { rev: 7 } : {};
    },
    ...apiOverrides,
  } as unknown as MonitorHost["api"];
  const root = document.createElement("section");
  document.body.append(root);
  monitor = new Monitor(root, {
    api, store, catalog: () => catalog,
    workspaceToken: (path) => (path === "/work" ? null : "ws2"),
    open: (session) => void calls.push(`open ${session}`),
    compose: (session, text) => void calls.push(`compose ${session} ${text}`),
    ...overrides,
  });
  monitor.setVisible(true);
  const card = (id: string) => [...root.querySelectorAll<HTMLElement>(".monitor-card")].find((element) => element.querySelector(".card-title")!.textContent === `Session ${id}`)!;
  return { root, store, calls, card };
}

const text = (element: Element | null) => element?.textContent?.replace(/\s+/g, " ").trim();
/** Runs the frames requested so far: a pending Monitor render and the markers' animation. */
const flush = () => frames.splice(0).forEach((frame) => frame(0));
const titles = (root: HTMLElement) => [...root.querySelectorAll(".monitor-grid > .monitor-card:not([hidden]) .card-title")].map((title) => title.textContent);

test("one grid holds every card, the sessions that need the user first and the newest opened first within a group", () => {
  const { root, card } = mount();
  const filters = () => [...root.querySelectorAll<HTMLElement>(".monitor-filter .segment")].map((button) => [text(button), button.getAttribute("aria-pressed")]);

  expect(titles(root)).toEqual(["Session ask", "Session busy", "Session draft", "Session mine"]);
  expect(filters()).toEqual([["All4", "true"], ["Needs you1", "false"], ["Working1", "false"], ["Idle2", "false"]]);
  const status = root.querySelector<HTMLElement>(".monitor-status-text")!;
  // The model that checked is part of the status, not only its tooltip.
  expect([text(status), status.title]).toEqual(["Checked 1m ago · Luna", "Checked by Luna"]);
});

test("a closed tile shows the badge, what the session is doing, the monitor's first block, and its facts", () => {
  const { card } = mount();
  const ask = card("ask");
  expect(text(ask.querySelector(".card-badge"))).toBe("Needs input");
  expect([text(ask.querySelector(".card-now-text")), text(ask.querySelector(".card-time"))]).toEqual(["Idle", "2m ago"]);
  // A question is not a single line, so the history after it waits in Details.
  const blocks = ask.querySelector<HTMLElement>(".card-blocks")!;
  expect([blocks.hidden, ask.querySelector<HTMLElement>(".card-evidence")!.hidden]).toEqual([false, true]);
  expect([...blocks.querySelectorAll<HTMLElement>(".block")].map((block) => block.dataset.block)).toEqual(["question"]);
  expect(text(blocks.querySelector(".question-text"))).toBe("Which loader do you prefer?");
  expect(text(blocks.querySelector(".blocks-age"))).toBe("checked 1m ago");
  expect(text(ask.querySelector(".card-workspace"))).toBe("ws2");
  expect(text(ask.querySelector(".card-primary"))).toBe("Reply");
  // The monitor's note waits in Details.
  expect(ask.querySelector<HTMLElement>(".card-details")!.hidden).toBe(true);

  const busy = card("busy");
  expect(busy.querySelector<HTMLElement>(".card-badge")!.hidden).toBe(true);
  expect([text(busy.querySelector(".card-now-text")), text(busy.querySelector(".card-now-code"))]).toEqual(["Running", "cargo test -p tact"]);
  expect(text(busy.querySelector(".card-time"))).toMatch(/^turn 1m 1[5-9]s$/);
  // Routine counters are not tags, and there is nothing else to show.
  expect(busy.querySelectorAll(".card-chip")).toHaveLength(0);
  expect(busy.querySelector<HTMLElement>(".card-evidence")!.hidden).toBe(true);
  expect(text(busy.querySelector(".card-primary"))).toBe("Steer");
});

test("an unread reply is badged as new, and only exceptional facts become tags", () => {
  const { store, card } = mount();
  store.dispatch({ type: "live", data: { active: "mine", sessions: store.state.live.map((entry) => entry.id === "mine" ? { ...entry, unread: true } : entry) } });
  store.dispatch({ type: "activity", data: { sessions: [{ ...store.state.activity.get("busy")!, queued: 2, context: { active_tokens: 90_000, window_tokens: 100_000 } }] } });
  monitor!.changed();
  flush();

  expect(text(card("mine").querySelector(".card-badge"))).toBe("New reply");
  expect([...card("busy").querySelectorAll<HTMLElement>(".card-chip")].map((chip) => [chip.textContent, chip.dataset.tone])).toEqual([["2 queued", "muted"], ["90% context", "danger"]]);
  expect([...card("draft").querySelectorAll(".card-chip")].map((chip) => chip.textContent)).toEqual(["Draft"]);
});

test("the filter hides the other groups' cards without detaching them, and says when it is empty", () => {
  const { root, store } = mount();
  const choose = (label: string) => [...root.querySelectorAll<HTMLElement>(".monitor-filter .segment")].find((button) => text(button)!.startsWith(label))!.click();
  const empty = root.querySelector<HTMLElement>(".monitor-empty")!;

  choose("Idle");
  expect(titles(root)).toEqual(["Session draft", "Session mine"]);
  expect(root.querySelectorAll(".monitor-grid > .monitor-card")).toHaveLength(4);
  expect(empty.hidden).toBe(true);
  store.dispatch({ type: "closed", data: { session: "busy" } });
  choose("Working");
  expect(titles(root)).toEqual([]);
  expect([empty.hidden, text(empty)]).toEqual([false, "No session is working."]);
  choose("All");
  expect(titles(root)).toHaveLength(3);
});

test("a running card filtered away stays on the page, hidden, so its marker keeps spinning", () => {
  const { root, card } = mount();
  const choose = (label: string) => [...root.querySelectorAll<HTMLElement>(".monitor-filter .segment")].find((button) => text(button)!.startsWith(label))!.click();
  const busy = card("busy");
  const sphere = busy.querySelector("canvas")!;

  // The markers' shared animation clock forgets a canvas once it leaves the page.
  choose("Idle");
  expect([busy.hidden, sphere.isConnected]).toEqual([true, true]);
  choose("All");
  expect([busy.hidden, busy.querySelector("canvas") === sphere, sphere.isConnected]).toEqual([false, true, true]);
});

test("a tile's health gauge fills its band's segments, names the score and its change, and marks a large move", () => {
  const { store, card } = mount();
  const gauge = (id: string) => card(id).querySelector<HTMLElement>(".card-health")!;
  const reading = (id: string) => {
    const element = gauge(id);
    const trend = element.querySelector<HTMLElement>(".health-trend")!;
    return {
      hidden: element.hidden, segments: element.dataset.segments, tone: element.dataset.tone, label: element.getAttribute("aria-label"),
      trend: trend.hidden ? null : trend.dataset.trend, earlier: element.classList.contains("earlier"),
    };
  };

  expect(reading("ask")).toEqual({ hidden: false, segments: "4", tone: "success", label: "Health 74 of 100, on track, was 52", trend: "up", earlier: false });
  expect(gauge("ask").title).toBe("Health 74 of 100, on track, was 52. It ends with a question.");
  // No note, no gauge.
  expect([gauge("mine").hidden, gauge("busy").hidden]).toEqual([true, true]);

  // The running session moved on after the check, so its gauge dims like the rest of its note.
  showNotes(store, { ...askNote, health: null }, { ...askNote, session: "busy", attention: "ok", reason: null, health: 31, previous_health: 38 });
  expect(gauge("ask").hidden).toBe(true);
  expect(reading("busy")).toEqual({ hidden: false, segments: "2", tone: "danger", label: "Health 31 of 100, struggling, was 38, earlier", trend: null, earlier: true });
  expect(gauge("busy").title).toBe("Health 31 of 100, struggling, was 38, earlier");
});

test("ordering by health puts the lowest score first and unscored sessions last, and follows new scores", () => {
  const { root, store } = mount();
  const order = (label: string) => [...root.querySelectorAll<HTMLElement>(".monitor-order .segment")].find((button) => text(button) === label)!.click();
  const pressed = () => [...root.querySelectorAll<HTMLElement>(".monitor-order .segment")].map((button) => [text(button), button.getAttribute("aria-pressed")]);
  const scored = (session: string, health: number | null): SessionNote => ({ ...askNote, session, attention: session === "ask" ? "needs_input" : "ok", health });
  showNotes(store, scored("ask", 74), scored("busy", 30), scored("draft", null));

  expect(pressed()).toEqual([["Attention", "true"], ["Health", "false"]]);
  expect(titles(root)).toEqual(["Session ask", "Session busy", "Session draft", "Session mine"]);
  order("Health");
  expect(pressed()).toEqual([["Attention", "false"], ["Health", "true"]]);
  expect(titles(root)).toEqual(["Session busy", "Session ask", "Session draft", "Session mine"]);
  showNotes(store, scored("ask", 80), scored("busy", 30), scored("draft", 10));
  expect(titles(root)).toEqual(["Session draft", "Session busy", "Session ask", "Session mine"]);
  order("Attention");
  expect(titles(root)).toEqual(["Session ask", "Session busy", "Session draft", "Session mine"]);
});

test("the actions menu opens the session and closes an idle one, or stops a running one after confirming", async () => {
  const { card, calls } = mount();
  const menu = (id: string) => {
    card(id).querySelector<HTMLElement>(".card-more-actions")!.click();
    return [...document.querySelectorAll<HTMLElement>(".menu .menu-item")];
  };
  const choose = (id: string, label: string) => menu(id).find((item) => text(item) === label)!.click();

  expect(menu("ask").map((item) => text(item))).toEqual(["Open", "Close"]);
  choose("ask", "Open");
  choose("ask", "Close");
  expect(menu("busy").map((item) => text(item))).toEqual(["Open", "Stop"]);
  let confirmed = false;
  Object.assign(globalThis, { confirm: () => confirmed });
  choose("busy", "Stop");
  confirmed = true;
  choose("busy", "Stop");
  await Bun.sleep(0);
  expect(calls).toEqual(["open ask", "close_session ask ", "interrupt busy "]);
});

test("a card opens its session on click or Enter, but not from its own controls", () => {
  const { card, calls } = mount();
  const ask = card("ask");

  ask.click();
  ask.dispatchEvent(new dom.KeyboardEvent("keydown", { key: "Enter", bubbles: true }) as unknown as Event);
  ask.querySelector<HTMLElement>(".card-details-toggle")!.click();
  ask.querySelector<HTMLElement>(".card-primary")!.click();
  ask.querySelector<HTMLElement>(".card-details")!.click();

  expect(calls).toEqual(["open ask", "open ask"]);
});

test("Details holds the monitor's note, steps newest first, the prompt, the reply, the plan, and the counters", () => {
  const { store, card } = mount();
  const sessions = [...store.state.activity.values()];
  store.dispatch({ type: "activity", data: { sessions: sessions.map((entry) => entry.session === "ask" ? { ...entry, prompt: { text: "Compare the loaders.", at_ms: NOW } } : entry) } });
  monitor!.changed();
  flush();
  const ask = card("ask");
  const toggle = ask.querySelector<HTMLElement>(".card-details-toggle")!;
  toggle.click();
  const details = ask.querySelector<HTMLElement>(".card-details")!;
  const steps = [...details.querySelectorAll<HTMLElement>(".details-blocks .block-step")].map((step) => [
    step.dataset.outcome,
    step.querySelector(".block-mark")!.getAttribute("aria-label"),
    text(step.querySelector(".block-step-text")),
    text(step.querySelector(".block-detail")) ?? null,
  ]);

  expect([details.hidden, toggle.getAttribute("aria-expanded")]).toEqual([false, "true"]);
  expect(text(details.querySelector(".details-goal-text"))).toBe("Pick a config loader");
  expect(text(details.querySelector(".details-checked-text"))).toBe("checked 1m ago");
  expect([...details.querySelectorAll<HTMLElement>(".details-blocks > .block")].map((block) => [block.dataset.block, block.dataset.form]))
    .toEqual([["question", "full"], ["history", "full"]]);
  expect(steps).toEqual([
    ["running", "in progress", "Rebuild the crate", null],
    ["failed", "failed", "Ran the config tests", "2 of 9 failed"],
    ["unclear", "unclear", "Compared figment", null],
    ["worked", "worked", "Found the TOML call sites", "1 site"],
  ]);
  expect(text(details.querySelector(".details-reason"))).toBe("It ends with a question.");
  expect(text(details.querySelector(".details-suggestion"))).toBe("Keep the TOML loader.");
  expect(text(details.querySelector(".details-prompt p"))).toBe("Compare the loaders.");
  expect(text(details.querySelector(".details-message p"))).toBe("Which loader do you prefer?");
  expect(text(details.querySelector(".details-where"))).toBe("Luna · /work-2");

  const busy = card("busy");
  busy.querySelector<HTMLElement>(".card-details-toggle")!.click();
  expect(busy.querySelector<HTMLElement>(".details-monitor")!.hidden).toBe(true);
  expect(text(busy.querySelector(".details-plan"))).toBe("Plan 2/4 · Run the focused tests");
  expect(text(busy.querySelector(".details-counters"))).toBe("12 calls · 1 failed · 2 patches");

  toggle.click();
  expect([details.hidden, toggle.getAttribute("aria-expanded")]).toEqual([true, "false"]);
});

test("the monitor's evidence shows Markdown as plain text, while an inserted suggestion keeps it for the agent", () => {
  const { store, card } = mount();
  const check = monitorState.check!;
  store.dispatch({
    type: "monitor",
    data: { ...monitorState, check: { ...check, sessions: [{ ...check.sessions[0]!, reason: "It asked about `todo.txt`.", suggestion: "Summarize `todo.txt`." }] } },
  });
  monitor!.changed();
  flush();
  const ask = card("ask");

  ask.querySelector<HTMLElement>(".card-details-toggle")!.click();
  expect(text(ask.querySelector(".details-reason"))).toBe("It asked about todo.txt.");
  ask.querySelector<HTMLElement>(".card-primary")!.click();
  ask.querySelector<HTMLElement>(".insert-suggestion")!.click();
  expect(ask.querySelector("textarea")!.value).toBe("Summarize `todo.txt`.");
});

test("Insert suggestion puts the text at the cursor for editing and sends nothing", async () => {
  const { card, calls } = mount();
  const ask = card("ask");
  ask.querySelector<HTMLElement>(".card-primary")!.click();
  const textarea = ask.querySelector("textarea")!;
  textarea.value = "Please: ";
  textarea.setSelectionRange(8, 8);
  Object.defineProperty(textarea, "scrollHeight", { configurable: true, value: 90 });

  ask.querySelector<HTMLElement>(".insert-suggestion")!.click();
  await Bun.sleep(0);
  expect(textarea.value).toBe("Please: Keep the TOML loader.");
  // The box grows to show the inserted text, as it does while typing.
  expect(textarea.style.height).toBe("90px");
  expect(calls).toEqual([]);
  // A session with no suggestion offers nothing to insert.
  card("busy").querySelector<HTMLElement>(".card-primary")!.click();
  expect(card("busy").querySelector<HTMLElement>(".insert-suggestion")!.hidden).toBe(true);
});

test("replies write the session's draft and submit that revision; the active session replies in its chat", async () => {
  const { card, calls } = mount();
  const ask = card("ask");

  ask.querySelector<HTMLElement>(".card-primary")!.click();
  expect(ask.querySelector<HTMLElement>(".card-primary")!.hidden).toBe(true);
  const textarea = ask.querySelector("textarea")!;
  textarea.value = "Keep the TOML loader.";
  textarea.dispatchEvent(new dom.KeyboardEvent("keydown", { key: "Enter", bubbles: true }) as unknown as Event);
  await Bun.sleep(0);
  expect(calls).toEqual(["set_draft ask Keep the TOML loader.", "submit ask 7"]);
  expect(ask.querySelector<HTMLElement>(".card-reply")!.hidden).toBe(true);

  // Someone else's unsent draft is never overwritten; the card opens the session instead.
  const draft = card("draft").querySelector<HTMLElement>(".card-primary")!;
  expect(text(draft)).toBe("Open");
  draft.click();
  expect(card("draft").querySelector<HTMLElement>(".card-reply")!.hidden).toBe(true);
  expect(calls.at(-1)).toBe("open draft");

  const mine = card("mine").querySelector<HTMLElement>(".card-primary")!;
  expect(text(mine)).toBe("Reply in chat");
  mine.click();
  expect(calls.slice(-2)).toEqual(["open mine", "compose mine "]);
});

test("changes re-render once per frame, and closed sessions leave", async () => {
  const { root, store } = mount();
  store.dispatch({ type: "closed", data: { session: "busy" } });
  store.dispatch({ type: "closed", data: { session: "draft" } });
  expect(root.querySelectorAll(".monitor-card")).toHaveLength(4);

  const pending = frames.length;
  monitor!.changed();
  monitor!.changed();
  expect(frames).toHaveLength(pending + 1);
  flush();
  expect(titles(root)).toEqual(["Session ask", "Session mine"]);
});

test("a card still drawn as the active session opens it before composing, after another session became active", () => {
  const { store, card, calls } = mount();
  showNotes(store, { ...askNote, session: "mine" });
  // Another tab activates "ask"; the Monitor has not redrawn, so "mine" still looks active.
  store.dispatch({ type: "active", data: { session: "ask" } });
  card("mine").querySelector<HTMLElement>(".question-option")!.click();
  card("mine").querySelector<HTMLElement>(".card-primary")!.click();
  expect(calls).toEqual(["open mine", "compose mine Keep TOML", "open mine", "compose mine "]);
});

test("a closed session's card stops observing its layout, while the others keep theirs", () => {
  const { store } = mount();
  store.dispatch({ type: "closed", data: { session: "busy" } });
  monitor!.changed();
  flush();
  const observers = FakeResizeObserver.observers.filter((observer) => observer.targets.length > 0);
  // Counts, since a failing comparison of DOM nodes would print the whole document.
  expect(observers.length).toBe(3);
  expect(observers.flatMap((observer) => observer.targets).filter((target) => !target.isConnected).length).toBe(0);
});

test("a reply box left open while its session became the active one sends to the composer, not the shared draft", async () => {
  const { store, card, calls } = mount();
  const ask = card("ask");
  ask.querySelector<HTMLElement>(".card-primary")!.click();
  ask.querySelector("textarea")!.value = "Keep the TOML loader.";

  // The session is opened in Chat, where a composer draft is being written, and the Monitor shown again.
  store.dispatch({ type: "live", data: { active: "ask", sessions: [...store.state.live.map((entry) => entry.id === "ask" ? { ...entry, has_draft: true } : entry)] } });
  monitor!.changed();
  flush();
  expect(ask.querySelector<HTMLElement>(".card-reply")!.hidden).toBe(false);
  ask.querySelector("textarea")!.dispatchEvent(new dom.KeyboardEvent("keydown", { key: "Enter", bubbles: true }) as unknown as Event);
  await Bun.sleep(0);

  expect(calls).toEqual(["open ask", "compose ask Keep the TOML loader."]);
  expect(ask.querySelector<HTMLElement>(".card-reply")!.hidden).toBe(true);
});

test("a monitor reply is dropped when newer state arrived while it was in flight", async () => {
  let reply!: (state: MonitorState) => void;
  const pending = new Promise<MonitorState>((resolve) => (reply = resolve));
  const { store } = mount({}, { monitor: () => pending });
  const newer: MonitorState = { ...monitorState, check: { ...monitorState.check!, generated_at_ms: NOW } };

  store.dispatch({ type: "monitor", data: newer });
  monitor!.setVisible(false);
  reply(monitorState);
  await Bun.sleep(0);
  expect(store.state.monitor).toBe(newer);
});

test("a monitor reply is applied when nothing arrived while it was in flight", async () => {
  const newer: MonitorState = { ...monitorState, running: true };
  const { store } = mount({}, { monitor: async () => newer });
  await Bun.sleep(0);
  expect(store.state.monitor).toBe(newer);
});

test("a short message of many lines reads as cut off once layout shows it is, with no More row", () => {
  const lines = ["one", "two", "three", "four", "five", "six"].join("\n");
  const { store, card } = mount();
  showNotes(store, { ...askNote, blocks: [] });
  store.dispatch({ type: "activity", data: { sessions: [{ ...store.state.activity.get("ask")!, message: { text: lines, commentary: false, at_ms: NOW } }] } });
  monitor!.changed();
  flush();
  const evidence = card("ask").querySelector<HTMLElement>(".card-evidence")!;
  const box = evidence.querySelector<HTMLElement>(".evidence-text")!;
  const inner = evidence.querySelector<HTMLElement>(".evidence-inner")!;
  // The box's limit can change without the text changing, so both are watched.
  const observer = FakeResizeObserver.observers.find((candidate) => candidate.targets.includes(inner))!;
  expect(observer.targets).toContain(box);

  Object.defineProperty(box, "clientHeight", { configurable: true, value: 120 });
  Object.defineProperty(inner, "offsetHeight", { configurable: true, value: 120 });
  observer.callback();
  expect(evidence.classList.contains("clipped")).toBe(false);

  Object.defineProperty(box, "clientHeight", { configurable: true, value: 80 });
  observer.callback();
  expect(evidence.classList.contains("clipped")).toBe(true);
  // The full reply is in Details, so the closed tile spends no row on a link to it.
  expect(evidence.querySelector(".evidence-more")).toBeNull();
  expect(evidence.querySelector<HTMLElement>(".evidence-meta")!.hidden).toBe(true);
});

test("a stuck session's reason carries the monitor's age, dimmed once the session has moved on", () => {
  const { store, card } = mount();
  const check = monitorState.check!;
  const stuck = { ...check.sessions[0]!, session: "busy", attention: "stuck" as const, blocks: [], reason: "It has rerun the same test six times." };
  store.dispatch({ type: "monitor", data: { ...monitorState, check: { ...check, sessions: [stuck] } } });
  monitor!.changed();
  flush();
  const evidence = card("busy").querySelector<HTMLElement>(".card-evidence")!;

  expect(text(card("busy").querySelector(".card-badge"))).toBe("Stuck");
  expect([evidence.dataset.kind, text(evidence.querySelector(".evidence-inner"))]).toEqual(["reason", "It has rerun the same test six times."]);
  expect(text(evidence.querySelector(".evidence-age"))).toBe("checked 1m ago · earlier");
  expect(evidence.classList.contains("earlier")).toBe(true);
});

test("a deterministic error takes the body ahead of the monitor's blocks", () => {
  const { store, card } = mount();
  store.dispatch({ type: "live", data: { active: "mine", sessions: store.state.live.map((entry) => entry.id === "ask" ? { ...entry, state: "error" as const } : entry) } });
  changeActivity(store, "ask", { error: "connection reset by peer" });
  const ask = card("ask");

  expect(ask.querySelector<HTMLElement>(".card-blocks")!.hidden).toBe(true);
  expect([ask.querySelector<HTMLElement>(".card-evidence")!.dataset.kind, text(ask.querySelector(".evidence-inner"))]).toEqual(["error", "connection reset by peer"]);
});

test("a note older than an idle session's latest activity gives way to its reply; a running session keeps it, dimmed", () => {
  const { store, card } = mount();
  const busyNote: SessionNote = { ...askNote, session: "busy", attention: "ok", blocks: [{ type: "metric", label: "Failing tests", value: "3", previous: "5", tone: "warning" }] };
  showNotes(store, askNote, busyNote);
  store.dispatch({ type: "live", data: { active: "mine", sessions: store.state.live.map((entry) => entry.id === "ask" ? { ...entry, last_activity_unix_ms: NOW } : entry) } });
  monitor!.changed();
  flush();

  const ask = card("ask");
  expect(ask.querySelector<HTMLElement>(".card-blocks")!.hidden).toBe(true);
  expect(text(ask.querySelector(".evidence-inner"))).toBe("Which loader do you prefer?");
  const blocks = card("busy").querySelector<HTMLElement>(".card-blocks")!;
  expect([blocks.hidden, blocks.classList.contains("earlier"), text(blocks.querySelector(".blocks-age"))]).toEqual([false, true, "checked 1m ago · earlier"]);
  expect(text(blocks.querySelector(".metric-value"))).toBe("3");
});

test("a question's option opens the reply with the option in it, and sends nothing", async () => {
  const { store, card, calls } = mount();
  const ask = card("ask");
  ask.querySelectorAll<HTMLElement>(".question-option")[1]!.click();
  await Bun.sleep(0);

  expect(ask.querySelector<HTMLElement>(".card-reply")!.hidden).toBe(false);
  expect(ask.querySelector("textarea")!.value).toBe("Switch to figment");
  expect(calls).toEqual([]);

  // The active session's draft is the composer's, so the option goes there.
  showNotes(store, { ...askNote, session: "mine" });
  card("mine").querySelector<HTMLElement>(".question-option")!.click();
  expect(calls).toEqual(["open mine", "compose mine Keep TOML"]);
});

test("a stuck or off-track tile leads with the note's warning or danger text, whatever block the model put first", () => {
  const { store, card } = mount();
  const busy = card("busy");
  const shown = () => [...busy.querySelectorAll<HTMLElement>(".card-blocks .block")].map((block) => block.dataset.block);
  const passing = { type: "metric", label: "Tests passing", value: "1/1", previous: null, tone: "success" } as const;
  const problem = (tone: "warning" | "danger") => ({ type: "text", text: "It made the test pass by changing what add returns.", tone }) as const;
  const blocks: SessionNote["blocks"] = [passing, problem("danger")];
  showNotes(store, askNote, { ...askNote, session: "busy", attention: "off_track", reason: "It changed add to fit the test.", blocks });
  expect(text(busy.querySelector(".card-badge"))).toBe("Off track");
  expect(shown()).toEqual(["text"]);
  expect(text(busy.querySelector(".block-text-body"))).toBe("It made the test pass by changing what add returns.");

  showNotes(store, askNote, { ...askNote, session: "busy", attention: "stuck", reason: "It keeps rerunning the test.", blocks: [passing, problem("warning")] });
  expect(shown()).toEqual(["text"]);
  // Without a concern, the model's order stands.
  showNotes(store, askNote, { ...askNote, session: "busy", attention: "ok", reason: null, blocks });
  expect(shown()).toEqual(["metric"]);
});

test("text changed while a reply is being sent stays in the box, whether typed or inserted", async () => {
  let finish = () => {};
  const { card } = mount({}, {
    command: async (name: string) => {
      if (name === "submit") await new Promise<void>((resolve) => { finish = resolve; });
      return name === "set_draft" ? { rev: 7 } : {};
    },
  });
  const ask = card("ask");
  const textarea = ask.querySelector("textarea")!;
  const reply = ask.querySelector<HTMLElement>(".card-reply")!;
  const send = async () => {
    textarea.dispatchEvent(new dom.KeyboardEvent("keydown", { key: "Enter", bubbles: true }) as unknown as Event);
    await Bun.sleep(0);
  };
  ask.querySelector<HTMLElement>(".card-primary")!.click();
  textarea.value = "first";
  await send();
  textarea.value = "second, not sent";
  finish();
  await Bun.sleep(0);
  expect([textarea.value, reply.hidden]).toEqual(["second, not sent", false]);

  await send();
  ask.querySelectorAll<HTMLElement>(".question-option")[1]!.click();
  finish();
  await Bun.sleep(0);
  expect([textarea.value, reply.hidden]).toEqual(["second, not sentSwitch to figment", false]);
});

test("a send that fails after saving the draft keeps its error until dismissed, then offers Open", async () => {
  const { store, card } = mount({}, {
    command: async (name: string) => {
      if (name === "submit") throw new Error("This command is not available remotely.");
      return { rev: 7 };
    },
  });
  const ask = card("ask");
  const reply = ask.querySelector<HTMLElement>(".card-reply")!;
  const error = ask.querySelector<HTMLElement>(".card-reply-error")!;
  ask.querySelector<HTMLElement>(".card-primary")!.click();
  ask.querySelector("textarea")!.value = "/copy 2";
  ask.querySelector("textarea")!.dispatchEvent(new dom.KeyboardEvent("keydown", { key: "Enter", bubbles: true }) as unknown as Event);
  await Bun.sleep(0);
  expect(error.hidden).toBe(false);

  // The text it saved now shows as the session's draft.
  store.dispatch({ type: "live", data: { active: "mine", sessions: store.state.live.map((entry) => entry.id === "ask" ? { ...entry, has_draft: true } : entry) } });
  monitor!.changed();
  flush();
  expect([error.hidden, reply.hidden]).toEqual([false, false]);

  ask.querySelector<HTMLElement>(".cancel-reply")!.click();
  expect([error.hidden, reply.hidden, text(ask.querySelector(".card-primary"))]).toEqual([true, true, "Open"]);
});

test("a live message replaced while its card lives is no longer observed", () => {
  const { store } = mount();
  showNotes(store, askNote, { ...askNote, session: "busy", attention: "ok", blocks: [{ type: "live", source: "message" }] });
  for (const message of ["First.", "Second.", "Third."]) changeActivity(store, "busy", { message: { text: message, commentary: false, at_ms: NOW } });
  const observed = FakeResizeObserver.observers.flatMap((observer) => observer.targets);
  // Counts, since a failing comparison of DOM nodes would print the whole document.
  expect(observed.filter((target) => target.isConnected && target.closest(".blocks-list")).length).toBe(2);
  expect(observed.filter((target) => !target.isConnected).length).toBe(0);
});

test("a stuck or off-track tile whose blocks report no problem shows the monitor's reason instead", () => {
  const { store, card } = mount();
  const busy = card("busy");
  const blocks = () => [...busy.querySelectorAll<HTMLElement>(".card-blocks:not([hidden]) .block")].map((block) => text(block));
  const evidence = busy.querySelector<HTMLElement>(".card-evidence")!;
  const passing = { type: "metric", label: "Tests passing", value: "18", previous: null, tone: "success" } as const;
  const stuck = (...shown: SessionNote["blocks"]) =>
    showNotes(store, askNote, { ...askNote, session: "busy", attention: "stuck", reason: "It has rerun the same test six times.", blocks: shown });

  stuck(passing);
  expect(blocks()).toEqual([]);
  expect([evidence.hidden, evidence.dataset.kind, text(evidence.querySelector(".evidence-inner"))]).toEqual([false, "reason", "It has rerun the same test six times."]);

  // A failing metric or a failed newest step reports the problem itself, so it leads.
  stuck(passing, { ...passing, label: "Failing tests", value: "3", tone: "danger" });
  expect(blocks()).toEqual(["Failing tests3", "Tests passing18"]);
  stuck(passing, { type: "history", items: [{ action: "Ran the focused tests", outcome: "failed", detail: "1 failed" }] });
  expect(blocks()[0]).toContain("Ran the focused tests");
  expect(evidence.hidden).toBe(true);

  // A checklist shows its active item, so a failed one beside it does not explain the concern.
  const steps = (...states: ("done" | "active" | "failed")[]) =>
    ({ type: "checklist", title: null, items: states.map((state, index) => ({ label: `Step ${index + 1}`, state })) }) as const;
  stuck(steps("failed", "active"));
  expect([blocks(), evidence.dataset.kind]).toEqual([[], "reason"]);
  stuck(passing, steps("done", "failed"));
  expect(blocks()[0]).toContain("Step 2");
});

test("a live tool block that would repeat the call the live line names gives way to the next block, or to the fallback", () => {
  const { store, card } = mount();
  const busy = card("busy");
  const shown = () => [...busy.querySelectorAll<HTMLElement>(".card-blocks .block")].map((block) => block.dataset.block);
  const metric = { type: "metric", label: "Failing tests", value: "3", previous: "5", tone: "warning" } as const;
  showNotes(store, askNote, { ...askNote, session: "busy", attention: "ok", blocks: [{ type: "live", source: "tool" }, metric] });
  expect(text(busy.querySelector(".card-now-code"))).toBe("cargo test -p tact");
  expect(shown()).toEqual(["metric"]);

  // A Code Mode cell reads as "Running code" with its summary, which names the same call.
  changeActivity(store, "busy", { tool: { name: "exec", summary: "3 tools", state: "running", at_ms: NOW, outcome: null } });
  showNotes(store, askNote, { ...askNote, session: "busy", attention: "ok", goal: "Run the focused tests", blocks: [{ type: "live", source: "tool" }] });
  expect(busy.querySelector<HTMLElement>(".card-blocks")!.hidden).toBe(true);
  expect([busy.querySelector<HTMLElement>(".card-evidence")!.dataset.kind, text(busy.querySelector(".evidence-inner"))]).toEqual(["goal", "Run the focused tests"]);

  // Once the call has finished, the live line moves on and the block shows its outcome.
  changeActivity(store, "busy", { tool: { name: "exec", summary: "3 tools", state: "succeeded", at_ms: NOW, outcome: null } });
  expect(shown()).toEqual(["live-tool"]);
});

test("live blocks follow the session's activity as it changes, and Details leaves out what they show", () => {
  const { store, card } = mount();
  showNotes(store, askNote, { ...askNote, session: "busy", attention: "ok", blocks: [{ type: "live", source: "tool" }, { type: "live", source: "plan" }] });
  const busy = card("busy");
  const tool = () => busy.querySelector<HTMLElement>(".card-blocks .block-tool")!;
  // The live line names the running call, so the closed tile starts at the plan.
  expect([...busy.querySelectorAll<HTMLElement>(".card-blocks .block")].map((block) => block.dataset.block)).toEqual(["live-plan"]);

  changeActivity(store, "busy", { tool: { name: "exec_command", summary: "cargo test -p tact", state: "succeeded", at_ms: NOW, outcome: { exit_code: 0, tail: [], summary: "18 passed" } } });
  expect([tool().dataset.outcome, text(tool().querySelector(".tool-command")), text(tool().querySelector(".block-detail"))]).toEqual(["worked", "cargo test -p tact", "18 passed"]);

  busy.querySelector<HTMLElement>(".card-details-toggle")!.click();
  expect(text(busy.querySelector(".details-blocks .block-plan .plan-current"))).toBe("Run the focused tests");
  expect(busy.querySelector<HTMLElement>(".details-plan")!.hidden).toBe(true);
  changeActivity(store, "busy", { plan: { done: 3, total: 4, current: "Write the changelog" } });
  expect(text(busy.querySelector(".details-blocks .block-plan .plan-current"))).toBe("Write the changelog");
});

test("Details leaves out the reply and the prompt when live blocks already show them in full", () => {
  const { store, card } = mount();
  changeActivity(store, "ask", { prompt: { text: "Compare the loaders.", at_ms: NOW } });
  showNotes(store, { ...askNote, blocks: [{ type: "live", source: "message" }, { type: "live", source: "prompt" }] });
  const ask = card("ask");
  ask.querySelector<HTMLElement>(".card-details-toggle")!.click();

  expect(text(ask.querySelector(".details-blocks .block-message-full"))).toBe("Which loader do you prefer?");
  expect(text(ask.querySelector(".details-blocks .block-prompt"))).toBe("Compare the loaders.");
  expect([ask.querySelector<HTMLElement>(".details-message")!.hidden, ask.querySelector<HTMLElement>(".details-prompt")!.hidden]).toEqual([true, true]);
});

test("open Details takes the place of the closed body, so each block shows once, in full", () => {
  const { store, card } = mount();
  const ask = card("ask");
  const toggle = ask.querySelector<HTMLElement>(".card-details-toggle")!;
  const shownQuestions = () => [...ask.querySelectorAll<HTMLElement>(".block-question")].filter((block) => !block.closest("[hidden]"));

  toggle.click();
  expect(ask.querySelector<HTMLElement>(".card-blocks")!.hidden).toBe(true);
  expect(shownQuestions().map((block) => block.dataset.form)).toEqual(["full"]);
  // It stays that way as the tile refreshes.
  monitor!.changed();
  flush();
  expect(shownQuestions().map((block) => block.dataset.form)).toEqual(["full"]);
  toggle.click();
  expect(shownQuestions().map((block) => block.dataset.form)).toEqual(["compact"]);

  // Evidence that Details also holds, such as the reply, gives way too; an error, which Details does not repeat, stays.
  showNotes(store, { ...askNote, blocks: [] });
  toggle.click();
  expect(ask.querySelector<HTMLElement>(".card-evidence")!.hidden).toBe(true);
  toggle.click();
  store.dispatch({ type: "live", data: { active: "mine", sessions: store.state.live.map((entry) => entry.id === "ask" ? { ...entry, state: "error" as const } : entry) } });
  changeActivity(store, "ask", { error: "connection reset by peer" });
  toggle.click();
  expect([ask.querySelector<HTMLElement>(".card-evidence")!.hidden, text(ask.querySelector(".evidence-inner"))]).toEqual([false, "connection reset by peer"]);
});

test("a question's options act only while the session waits on it; otherwise they are plain text", () => {
  const { store, card } = mount();
  const options = (id: string) => [...card(id).querySelectorAll<HTMLElement>(".question-option, .question-choice")].map((option) => option.tagName.toLowerCase());
  expect(options("ask")).toEqual(["button", "button"]);

  // A running session has moved on from its question, including one that was waiting on it until now.
  showNotes(store, askNote, { ...askNote, session: "busy" });
  expect(options("busy")).toEqual(["span", "span"]);
  store.dispatch({ type: "live", data: { active: "mine", sessions: store.state.live.map((entry) => entry.id === "ask" ? { ...entry, state: "running" as const } : entry) } });
  monitor!.changed();
  flush();
  expect(options("ask")).toEqual(["span", "span"]);
  store.dispatch({ type: "live", data: { active: "mine", sessions: store.state.live.map((entry) => entry.id === "ask" ? { ...entry, state: "idle" as const } : entry) } });

  // An idle session that has done something since the check: Details shows the question without actions.
  store.dispatch({ type: "live", data: { active: "mine", sessions: store.state.live.map((entry) => entry.id === "ask" ? { ...entry, last_activity_unix_ms: NOW } : entry) } });
  monitor!.changed();
  flush();
  card("ask").querySelector<HTMLElement>(".card-details-toggle")!.click();
  expect(options("ask")).toEqual(["span", "span"]);
});
