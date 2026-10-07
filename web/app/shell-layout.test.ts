import { expect, test } from "bun:test";
import { clampPanelWidth, layoutReducer, loadLayout, saveLayout, viewportFor, type Layout } from "./layout";

const layout = (overrides: Partial<Layout> = {}): Layout =>
  ({ viewport: "phone", drawerOpen: false, panelOpen: false, panelWidth: 520, ...overrides });

function memoryStorage(initial: Record<string, string> = {}) {
  const items = new Map(Object.entries(initial));
  return { getItem: (key: string) => items.get(key) ?? null, setItem: (key: string, value: string) => void items.set(key, value) };
}

test("viewports switch at the phone and desktop breakpoints", () => {
  expect([390, 767, 768, 1199, 1200].map(viewportFor)).toEqual(["phone", "phone", "tablet", "tablet", "desktop"]);
});

test("the panel never squeezes the chat below its minimum width", () => {
  expect(clampPanelWidth(900, 1440)).toBe(1440 - 272 - 440);
  expect(clampPanelWidth(100, 1440)).toBe(360);
  expect(clampPanelWidth(500, 1440)).toBe(500);
});

test("crossing a breakpoint closes the drawer", () => {
  const open = layout({ drawerOpen: true });
  expect(layoutReducer(open, { type: "resize", width: 500 })).toBe(open);
  expect(layoutReducer(open, { type: "resize", width: 1000 })).toMatchObject({ viewport: "tablet", drawerOpen: false });
});

test("desktops have no drawer, and opening the panel closes it elsewhere", () => {
  expect(layoutReducer(layout({ viewport: "desktop" }), { type: "toggle-drawer" }).drawerOpen).toBe(false);
  const opened = layoutReducer(layout({ drawerOpen: true }), { type: "toggle-panel" });
  expect(opened).toMatchObject({ panelOpen: true, drawerOpen: false });
});

test("escape closes the drawer, then a panel sheet, but not a desktop panel", () => {
  const both = layout({ drawerOpen: true, panelOpen: true });
  const first = layoutReducer(both, { type: "escape" });
  expect(first).toMatchObject({ drawerOpen: false, panelOpen: true });
  expect(layoutReducer(first, { type: "escape" }).panelOpen).toBe(false);
  const desktop = layout({ viewport: "desktop", panelOpen: true });
  expect(layoutReducer(desktop, { type: "escape" })).toBe(desktop);
});

test("panel preferences persist, and corrupt storage falls back to defaults", () => {
  const storage = memoryStorage();
  saveLayout(storage, layout({ viewport: "desktop", panelOpen: true, panelWidth: 600 }));
  expect(loadLayout(storage, 1440)).toEqual({ viewport: "desktop", drawerOpen: false, panelOpen: true, panelWidth: 600 });
  expect(loadLayout(storage, 390).panelOpen).toBe(false);
  expect(loadLayout(memoryStorage({ "tact.web.layout.v1": "{" }), 1440)).toMatchObject({ panelOpen: false, panelWidth: 520 });
});
