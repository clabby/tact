/** Window-local layout state. Nothing here is shared with other windows. */

export type Viewport = "phone" | "tablet" | "desktop";

/**
 * The full-width views of the main area: the active session's conversation, the review of its
 * changes, and the Monitor over every live session.
 */
export type View = "chat" | "review" | "monitor";

/**
 * The Monitor exists only while at least this many sessions are live: with one, its chat already
 * shows everything. The server's monitor checks follow the same rule.
 */
export const MONITOR_MIN_LIVE = 2;

export function monitorAvailable(liveSessions: number) {
  return liveSessions >= MONITOR_MIN_LIVE;
}

/** Whether the palette offers to open the Monitor: it exists and is not already shown. */
export function offersMonitor(layout: Layout, liveSessions: number) {
  return layout.view !== "monitor" && monitorAvailable(liveSessions);
}

export type Layout = {
  viewport: Viewport;
  /** The sidebar drawer on phones and tablets; the sidebar is always shown on desktops. */
  drawerOpen: boolean;
  /** Whether a desktop has the sidebar folded away. */
  sidebarCollapsed: boolean;
  view: View;
};

export function viewportFor(width: number): Viewport {
  if (width < 768) return "phone";
  if (width < 1200) return "tablet";
  return "desktop";
}

export type LayoutAction =
  | { type: "resize"; width: number }
  | { type: "toggle-drawer"; open?: boolean }
  | { type: "toggle-sidebar" }
  /** Shows `view`; the Monitor only while `liveSessions` lets it exist. */
  | { type: "view"; view: View; liveSessions: number }
  /** The number of live sessions changed. */
  | { type: "live"; count: number }
  | { type: "escape" };

export function loadLayout(width: number, sidebarCollapsed = false): Layout {
  return { viewport: viewportFor(width), drawerOpen: false, sidebarCollapsed, view: "chat" };
}

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
    case "toggle-sidebar":
      // Desktops fold the sidebar away; narrower windows open or close it as a drawer.
      return layout.viewport === "desktop"
        ? { ...layout, sidebarCollapsed: !layout.sidebarCollapsed }
        : { ...layout, drawerOpen: !layout.drawerOpen };
    case "view":
      // A command offered while the Monitor existed may run after a session closed.
      if (action.view === layout.view || (action.view === "monitor" && !monitorAvailable(action.liveSessions))) return layout;
      return { ...layout, view: action.view, drawerOpen: false };
    case "live":
      // A Monitor left with too few live sessions gives way to the chat.
      return layout.view === "monitor" && !monitorAvailable(action.count) ? { ...layout, view: "chat" } : layout;
    case "escape":
      return layout.drawerOpen ? { ...layout, drawerOpen: false } : layout;
  }
}
