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

describe("sidebar history scope", () => {
  afterEach(() => {
    localStorage.clear();
    document.body.innerHTML = "";
    delete (globalThis as { window?: unknown }).window;
    delete (globalThis as { innerWidth?: unknown }).innerWidth;
    delete (globalThis as { innerHeight?: unknown }).innerHeight;
  });

  test("lists every checkout by default and the active session's workspace on request", async () => {
    // The scope menu positions itself inside the viewport.
    Object.assign(globalThis, { window: dom, innerWidth: 1024, innerHeight: 768 });
    const scopes: (string | undefined)[] = [];
    const api = {
      query: async (_name: string, args: { scope?: string }) => {
        scopes.push(args.scope);
        return { sessions: [], next_cursor: null, live_matches: {} };
      },
    } as unknown as SidebarHost["api"];
    const root = document.createElement("aside");
    document.body.append(root);
    const sidebar = new Sidebar(root, { ...host, api });
    const live = (workspace: string) => sidebar.setLive([{ ...session("a", "idle"), workspace }], "a");
    live("/work");
    await Bun.sleep(0);
    const scope = root.querySelector<HTMLButtonElement>(".history-scope")!;
    const empty = () => root.querySelector(".history-list .list-empty")!.textContent;

    expect(scopes).toEqual(["repository"]);
    expect(scope.textContent).toBe("All checkouts");
    expect(empty()).toBe("No saved sessions");

    scope.click();
    [...document.querySelectorAll<HTMLButtonElement>(".menu-item")].find((item) => item.textContent?.includes("This workspace"))!.click();
    await Bun.sleep(0);

    expect(scopes).toEqual(["repository", "workspace"]);
    expect(scope.textContent).toBe("This workspace");
    expect(empty()).toBe("No saved sessions in this workspace");

    live("/work");
    live("/work-checkout");
    await Bun.sleep(0);

    expect(scopes).toEqual(["repository", "workspace", "workspace"]);
  });
});
