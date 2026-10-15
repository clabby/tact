import { afterEach, beforeAll, describe, expect, test } from "bun:test";
import { Window } from "happy-dom";
import type { PersistedSession, SessionSummary, SiblingInstance } from "../core/wire";
import type { Sidebar as SidebarClass, SidebarHost } from "./sidebar";

const host: SidebarHost = {
  api: {} as SidebarHost["api"],
  catalog: () => null,
  navigated() {},
  setTheme() {},
  themeChoice: () => "system",
  resolvedTheme: () => "dark",
  workspaceToken: () => null,
};

const session = (id: string, state: SessionSummary["state"]) => ({ id, state, title: id, model: "gpt-6.1-sol", workspace: "/work" }) as SessionSummary;
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
  test("counts live and running sessions across every instance", () => {
    const summary = footerSummary(
      [session("a", "idle"), session("b", "idle")],
      [instance(1, true), instance(2), instance(1)],
    );

    expect(summary).toBe("4 live · 3 running");
  });

  test("omits the running count when nothing runs", () => {
    expect(footerSummary([session("a", "idle")], [instance(0, true), instance(0)])).toBe("2 live");
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
