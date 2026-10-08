import { expect, test } from "bun:test";
import { layoutReducer, loadLayout, viewportFor, type Layout } from "./layout";

const layout = (overrides: Partial<Layout> = {}): Layout => ({ ...loadLayout(390), ...overrides });

test("viewports follow the window width", () => {
  expect([viewportFor(390), viewportFor(900), viewportFor(1440)]).toEqual(["phone", "tablet", "desktop"]);
});

test("crossing a breakpoint closes the drawer", () => {
  const open = layout({ drawerOpen: true });
  expect(layoutReducer(open, { type: "resize", width: 1000 })).toMatchObject({ viewport: "tablet", drawerOpen: false });
  expect(layoutReducer(open, { type: "resize", width: 400 })).toBe(open);
});

test("the sidebar folds on desktops and opens as a drawer elsewhere", () => {
  const desktop = layout({ viewport: "desktop" });
  const folded = layoutReducer(desktop, { type: "toggle-sidebar" });
  expect(folded).toMatchObject({ sidebarCollapsed: true, drawerOpen: false });
  expect(layoutReducer(folded, { type: "toggle-sidebar" }).sidebarCollapsed).toBe(false);
  expect(layoutReducer(layout(), { type: "toggle-sidebar" })).toMatchObject({ drawerOpen: true, sidebarCollapsed: false });
});

test("desktops have no drawer", () => {
  const desktop = layout({ viewport: "desktop" });
  expect(layoutReducer(desktop, { type: "toggle-drawer" })).toBe(desktop);
});

test("switching views closes the drawer and starts on the chat", () => {
  expect(loadLayout(1440).view).toBe("chat");
  const next = layoutReducer(layout({ drawerOpen: true }), { type: "view", view: "review" });
  expect(next).toMatchObject({ view: "review", drawerOpen: false });
  expect(layoutReducer(next, { type: "view", view: "review" })).toBe(next);
});

test("escape closes only the drawer", () => {
  const both = layout({ drawerOpen: true, view: "review" });
  const closed = layoutReducer(both, { type: "escape" });
  expect(closed).toMatchObject({ drawerOpen: false, view: "review" });
  expect(layoutReducer(closed, { type: "escape" })).toBe(closed);
});
