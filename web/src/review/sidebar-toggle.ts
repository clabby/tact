/** Remembers per browser whether the reviewer folded the sidebar away. */
const SIDEBAR_KEY = "tact.web.review-sidebar";

type SidebarStorage = Pick<Storage, "getItem" | "setItem">;

/**
 * Binds the toolbar button that folds away the changed files and comments sidebar, so the diff
 * takes the panel's full width. On narrow panels the sidebar is one of the Diff, Files, and
 * Comments views instead, so the button and the folded state only apply to wide ones.
 */
export function bindSidebarToggle(root: HTMLElement, storage: SidebarStorage) {
  const button = root.querySelector<HTMLButtonElement>("#sidebar-toggle");
  const panel = root.querySelector<HTMLElement>("#changes-panel");
  if (!button || !panel) return;
  const show = (shown: boolean) => {
    panel.toggleAttribute("data-sidebar-hidden", !shown);
    button.setAttribute("aria-expanded", String(shown));
    const label = shown ? "Hide files and comments" : "Show files and comments";
    button.setAttribute("aria-label", label);
    button.title = label;
  };
  show(storage.getItem(SIDEBAR_KEY) !== "hidden");
  button.addEventListener("click", () => {
    const shown = button.getAttribute("aria-expanded") !== "true";
    show(shown);
    storage.setItem(SIDEBAR_KEY, shown ? "shown" : "hidden");
  });
}
