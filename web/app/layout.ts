/** Window-local layout state. Nothing here is shared with other windows. */

export type Viewport = "phone" | "tablet" | "desktop";

export type Layout = {
  viewport: Viewport;
  /** The sidebar drawer on phones and tablets; the sidebar is always shown on desktops. */
  drawerOpen: boolean;
  panelOpen: boolean;
  /** Preferred width of the side panel on desktops, in CSS pixels. */
  panelWidth: number;
};

export const SIDEBAR_WIDTH = 272;
export const MIN_CHAT_WIDTH = 440;
export const MIN_PANEL_WIDTH = 360;

export function viewportFor(width: number): Viewport {
  if (width < 768) return "phone";
  if (width < 1200) return "tablet";
  return "desktop";
}

/** The panel width that fits a window `width` wide while leaving the chat usable. */
export function clampPanelWidth(requested: number, width: number) {
  const available = width - SIDEBAR_WIDTH - MIN_CHAT_WIDTH;
  return Math.round(Math.max(MIN_PANEL_WIDTH, Math.min(requested, available)));
}

export type LayoutAction =
  | { type: "resize"; width: number }
  | { type: "toggle-drawer"; open?: boolean }
  | { type: "toggle-panel"; open?: boolean }
  | { type: "panel-width"; width: number; windowWidth: number }
  | { type: "escape" };

export function layoutReducer(layout: Layout, action: LayoutAction): Layout {
  switch (action.type) {
    case "resize": {
      const viewport = viewportFor(action.width);
      if (viewport === layout.viewport) return layout;
      return { ...layout, viewport, drawerOpen: false };
    }
    case "toggle-drawer": {
      const drawerOpen = layout.viewport !== "desktop" && (action.open ?? !layout.drawerOpen);
      return drawerOpen === layout.drawerOpen ? layout : { ...layout, drawerOpen };
    }
    case "toggle-panel": {
      const panelOpen = action.open ?? !layout.panelOpen;
      if (panelOpen === layout.panelOpen) return layout;
      return { ...layout, panelOpen, drawerOpen: panelOpen ? false : layout.drawerOpen };
    }
    case "panel-width":
      return { ...layout, panelWidth: clampPanelWidth(action.width, action.windowWidth) };
    case "escape":
      // Esc closes the topmost overlay only: the drawer, then a panel shown as a sheet.
      if (layout.drawerOpen) return { ...layout, drawerOpen: false };
      if (layout.panelOpen && layout.viewport !== "desktop") return { ...layout, panelOpen: false };
      return layout;
  }
}

const STORAGE_KEY = "tact.web.layout.v1";

export function loadLayout(storage: Pick<Storage, "getItem">, width: number): Layout {
  let saved: Partial<Pick<Layout, "panelOpen" | "panelWidth">> = {};
  try {
    saved = JSON.parse(storage.getItem(STORAGE_KEY) ?? "{}") as typeof saved;
  } catch {
    // A corrupt preference falls back to defaults.
  }
  const viewport = viewportFor(width);
  return {
    viewport,
    drawerOpen: false,
    panelOpen: viewport === "desktop" && saved.panelOpen === true,
    panelWidth: clampPanelWidth(typeof saved.panelWidth === "number" ? saved.panelWidth : 520, width),
  };
}

export function saveLayout(storage: Pick<Storage, "setItem">, layout: Layout) {
  storage.setItem(STORAGE_KEY, JSON.stringify({ panelOpen: layout.panelOpen, panelWidth: layout.panelWidth }));
}
