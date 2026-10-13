import { beforeAll, describe, expect, test } from "bun:test";
import { Window } from "happy-dom";
import type { SessionSummary, SiblingInstance } from "../core/wire";
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

beforeAll(async () => {
  const window = new Window();
  Object.assign(globalThis, { document: window.document, localStorage: window.localStorage });
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
