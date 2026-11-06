import { afterEach, beforeAll, describe, expect, test } from "bun:test";
import { Window } from "happy-dom";
import type { PersistedSession, SessionSummary, SiblingInstance } from "../core/wire";
import type { Sidebar as SidebarClass, SidebarHost } from "./sidebar";

const host: SidebarHost = {
  api: {} as SidebarHost["api"],
  catalog: () => null,
  navigated() {},
  renameSession() {},
  setTheme() {},
  themeChoice: () => "system",
  resolvedTheme: () => "dark",
  workspaceToken: () => null,
};

const session = (id: string, state: SessionSummary["state"], last_activity_unix_ms = 0) =>
  ({ id, state, title: id, model: "gpt-6.1-sol", workspace: "/work", last_activity_unix_ms }) as SessionSummary;
const instance = (running: number, current = false) =>
  ({ pid: running, port: 7878 + running, workspace: "/work", live: 1, running, current }) as SiblingInstance;

let Sidebar: typeof SidebarClass;
let dom: Window;

beforeAll(async () => {
  dom = new Window();
  Object.assign(globalThis, { document: dom.document, localStorage: dom.localStorage });
  ({ Sidebar } = await import("./sidebar"));
});

function footerSummary(live: SessionSummary[], instances: SiblingInstance[]) {
  const root = document.createElement("aside");
  const sidebar = new Sidebar(root, host);
  sidebar.setLive(live, null);
  sidebar.renderFooter("open", instances);
  return root.querySelector(".instance-text small")!.textContent;
}

describe("sidebar footer", () => {
  test("counts live instances and running sessions across every instance", () => {
    const summary = footerSummary(
      [session("a", "idle"), session("b", "idle")],
      [instance(1, true), instance(2), instance(1)],
    );

    expect(summary).toBe("3 live · 3 running");
  });

  test("omits the running count when nothing runs", () => {
    expect(footerSummary([session("a", "idle")], [instance(0, true), instance(0)])).toBe("2 live");
  });

  test("counts this instance before sibling instances are known", () => {
    expect(footerSummary([session("a", "idle"), session("b", "idle")], [])).toBe("1 live");
  });
});

describe("sidebar search", () => {
  afterEach(() => {
    delete (globalThis as { window?: unknown }).window;
  });

  test("shows where history and live sessions matched the conversation", async () => {
    // The search field debounces History queries with `window.setTimeout`.
    Object.assign(globalThis, { window: dom });
    const saved = { session_id: "saved", model: "gpt-6.1-sol", workspace: "/work", preview: "same first prompt", snippet: "…the lexer now handles unicode" } as PersistedSession;
    const api = {
      query: async (_name: string, args: { query?: string }) => args.query
        ? { sessions: [saved], next_cursor: null, live_matches: { b: "a lexer in the live one" } }
        : { sessions: [], next_cursor: null, live_matches: {} },
    } as unknown as SidebarHost["api"];
    const root = document.createElement("aside");
    const sidebar = new Sidebar(root, { ...host, api });
    sidebar.setLive([session("a", "idle"), session("b", "idle")], null);

    const search = root.querySelector<HTMLInputElement>(".chat-search input")!;
    search.value = "lexer";
    search.dispatchEvent(new dom.Event("input") as unknown as Event);
    await Bun.sleep(250);

    const rows = (list: string) => [...root.querySelectorAll(`${list} .session-row`)].map((row) => [
      row.querySelector(".session-title")!.textContent,
      row.querySelector(".session-snippet")?.textContent,
    ]);
    expect(rows(".live-list")).toEqual([["b", "a lexer in the live one"]]);
    expect(rows(".history-list")).toEqual([["same first prompt", "…the lexer now handles unicode"]]);
  });
});

test("refreshes cached history titles after the initial history load", async () => {
  let preview = "Original title";
  let calls = 0;
  let finishInitial: () => void;
  const initial = new Promise<void>((resolve) => { finishInitial = resolve; });
  const api = {
    async query() {
      calls += 1;
      if (calls === 1) await initial;
      return {
        sessions: [{ session_id: "saved", preview, model: "sol", workspace: "/work", started_at_unix_ms: 1 } as PersistedSession],
        next_cursor: null,
      };
    },
  } as unknown as SidebarHost["api"];
  const root = document.createElement("aside");
  const sidebar = new Sidebar(root, { ...host, api });
  await sidebar.refreshHistory();
  expect(calls).toBe(1);
  finishInitial!();
  await Promise.resolve();
  await Promise.resolve();
  expect(root.querySelector(".history-row .session-title")!.textContent).toBe("Original title");
  preview = "Generated title";
  await sidebar.refreshHistory();
  expect(calls).toBe(2);
  expect(root.querySelector(".history-row .session-title")!.textContent).toBe("Generated title");
});

describe("sidebar titles", () => {
  test("live rows shimmer while their title is pending and reveal only when it resolves", () => {
    const root = document.createElement("aside");
    const sidebar = new Sidebar(root, host);
    const title = () => root.querySelector(".live-list .session-title")!;
    const classes = () => ["title-pending", "title-reveal"].filter((name) => title().classList.contains(name));
    const live = (text: string, title_pending: boolean) => sidebar.setLive([{ ...session("a", "idle"), title: text, title_pending }], null);

    live("Settled title", false);
    expect(classes()).toEqual([]);
    live("Fix the parser", true);
    expect(classes()).toEqual(["title-pending"]);
    live("Fix the parser", true);
    expect(classes()).toEqual(["title-pending"]);
    live("Parser repair", false);
    expect(title().textContent).toBe("Parser repair");
    expect(classes()).toEqual(["title-reveal"]);
  });

  test("history rows awaiting a generated title shimmer", async () => {
    const api = {
      query: async () => ({
        sessions: [{ session_id: "saved", preview: "first prompt", title_pending: true, model: "sol", workspace: "/work", started_at_unix_ms: 1 } as PersistedSession],
        next_cursor: null,
      }),
    } as unknown as SidebarHost["api"];
    const root = document.createElement("aside");
    new Sidebar(root, { ...host, api });
    await Bun.sleep(0);
    expect(root.querySelector(".history-row .session-title")!.classList.contains("title-pending")).toBe(true);
  });
});

describe("sidebar updates while sessions work", () => {
  const liveRows = (root: HTMLElement) => [...root.querySelectorAll<HTMLElement>(".live-list .session-row")];

  test("activity keeps rows, and their clicks, in place while updating the age", () => {
    const root = document.createElement("aside");
    const sidebar = new Sidebar(root, host);
    const now = Date.now();
    sidebar.setLive([session("a", "idle", now - 3 * 60_000), session("b", "idle", now - 3_600_000)], null);
    const [first, second] = liveRows(root);

    sidebar.setLive([session("a", "idle", now - 5 * 60_000), session("b", "idle", now - 3_600_000)], null);

    expect(liveRows(root)).toEqual([first!, second!]);
    expect(first!.querySelector(".session-age")!.textContent).toBe("5m");
  });

  test("a row is rebuilt when what it shows changes", () => {
    const root = document.createElement("aside");
    const sidebar = new Sidebar(root, host);
    sidebar.setLive([session("a", "idle")], null);
    const [before] = liveRows(root);

    sidebar.setLive([session("a", "error")], null);

    expect(liveRows(root)[0]).not.toBe(before!);
    expect(liveRows(root)[0]!.dataset.state).toBe("error");
  });

  test("rows keep their places while the pointer is over the list", () => {
    const root = document.createElement("aside");
    const sidebar = new Sidebar(root, host);
    const list = root.querySelector(".live-list")!;
    const order = () => liveRows(root).map((row) => row.dataset.session);
    sidebar.setLive([session("a", "idle", 2), session("b", "idle", 1)], null);
    list.dispatchEvent(new dom.Event("pointerenter") as unknown as Event);

    sidebar.setLive([session("a", "idle", 2), session("b", "idle", 3), session("c", "idle", 4)], null);
    expect(order()).toEqual(["a", "b", "c"]);

    list.dispatchEvent(new dom.Event("pointerleave") as unknown as Event);
    expect(order()).toEqual(["c", "b", "a"]);
  });

  test("live updates leave History rows alone unless a session joins the live list", async () => {
    const api = {
      query: async () => ({
        sessions: [{ session_id: "saved", preview: "saved", model: "sol", workspace: "/work", started_at_unix_ms: 1 } as PersistedSession],
        next_cursor: null,
        live_matches: {},
      }),
    } as unknown as SidebarHost["api"];
    const root = document.createElement("aside");
    const sidebar = new Sidebar(root, { ...host, api });
    sidebar.setLive([session("a", "idle", 1)], null);
    await Bun.sleep(0);
    const historyRow = () => root.querySelector(".history-row");
    const before = historyRow();

    sidebar.setLive([session("a", "idle", 2)], null);
    sidebar.setLive([session("a", "error", 3)], "a");
    expect(historyRow()).toBe(before);

    sidebar.setLive([session("a", "error", 3), session("saved", "idle", 4)], "a");
    expect(historyRow()).toBeNull();
  });

  test("the footer is rebuilt only when what it shows changes", () => {
    const root = document.createElement("aside");
    const sidebar = new Sidebar(root, host);
    sidebar.renderFooter("open", [instance(0, true)]);
    const footerButton = () => root.querySelector(".instance-button")!;
    const button = footerButton();

    sidebar.renderFooter("open", [instance(0, true)]);
    expect(footerButton()).toBe(button);

    sidebar.renderFooter("reconnecting", [instance(0, true)]);
    expect(footerButton()).not.toBe(button);
  });

  test("a clicked session is highlighted at once and repeated clicks activate it once", async () => {
    const activations: unknown[] = [];
    const api = {
      query: async () => ({ sessions: [], next_cursor: null, live_matches: {} }),
      command: async (name: string, args: unknown) => void activations.push([name, args]),
    } as unknown as SidebarHost["api"];
    const root = document.createElement("aside");
    const sidebar = new Sidebar(root, { ...host, api });
    sidebar.setLive([session("a", "idle", 2), session("b", "idle", 1)], "a");
    const row = (id: string) => root.querySelector<HTMLElement>(`.session-row[data-session="${id}"]`)!;

    for (let click = 0; click < 3; click += 1) row("b").querySelector<HTMLButtonElement>(".session-main")!.click();
    await Bun.sleep(0);
    expect(activations).toEqual([["activate", { session: "b" }]]);
    expect(row("b").classList.contains("active")).toBe(true);
    expect(row("a").classList.contains("active")).toBe(false);

    // An update sent before the switch landed does not move the highlight back.
    sidebar.setLive([session("a", "idle", 2), session("b", "idle", 1)], "a");
    expect(row("b").classList.contains("active")).toBe(true);

    sidebar.setLive([session("a", "idle", 2), session("b", "idle", 1)], "b");
    expect(row("b").classList.contains("active")).toBe(true);
    expect(row("a").classList.contains("active")).toBe(false);
  });
});
