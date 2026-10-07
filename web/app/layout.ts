/** Window-local layout state. Nothing here is shared with other windows. */

export type Viewport = "phone" | "tablet" | "desktop";

/** The two full-width views of the main area: the conversation, and the review of its changes. */
export type View = "chat" | "review";

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
  | { type: "view"; view: View }
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
      return action.view === layout.view ? layout : { ...layout, view: action.view, drawerOpen: false };
    case "escape":
      return layout.drawerOpen ? { ...layout, drawerOpen: false } : layout;
  }
}
