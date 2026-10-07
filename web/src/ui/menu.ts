export type MenuItem = {
  label: string;
  /** A glyph for the leading slot, instead of a colour dot. */
  icon?: string;
  /** Dim secondary text after the label, e.g. a path. */
  hint?: string;
  detail?: string;
  /** A CSS colour for the leading dot, e.g. a model hue. */
  swatch?: string;
  checked?: boolean;
  danger?: boolean;
  disabled?: boolean;
  /** A heading shown above the first item of each run of items with the same section. */
  section?: string;
  run(): void;
};

let closeOpenMenu: (() => void) | null = null;

/**
 * Opens a popover menu anchored to `anchor`, with arrow-key navigation, type-to-select, and
 * dismissal on Escape, outside click, or selection. Only one menu is open at a time; focus returns
 * to the anchor on close.
 */
export function openMenu(anchor: HTMLElement, items: MenuItem[], label: string) {
  closeOpenMenu?.();
  const menu = document.createElement("div");
  menu.className = "menu";
  menu.setAttribute("role", "menu");
  menu.setAttribute("aria-label", label);
  const buttons = items.map((item, index) => {
    if (item.section && item.section !== items[index - 1]?.section) {
      const heading = document.createElement("div");
      heading.className = "menu-section";
      heading.setAttribute("role", "presentation");
      heading.textContent = item.section;
      menu.append(heading);
    }
    const button = document.createElement("button");
    button.type = "button";
    button.className = "menu-item";
    button.setAttribute("role", item.checked === undefined ? "menuitem" : "menuitemradio");
    if (item.checked !== undefined) button.setAttribute("aria-checked", String(item.checked));
    button.disabled = item.disabled ?? false;
    button.classList.toggle("danger", item.danger ?? false);
    button.innerHTML = `<span class="menu-swatch"></span><span class="menu-label"></span><span class="menu-hint"></span><span class="menu-detail"></span>`;
    const swatch = button.querySelector<HTMLElement>(".menu-swatch")!;
    if (item.icon) swatch.outerHTML = item.icon;
    else if (item.swatch) swatch.style.background = item.swatch;
    else swatch.remove();
    button.querySelector(".menu-label")!.textContent = item.label;
    const hint = button.querySelector(".menu-hint")!;
    if (item.hint) hint.textContent = item.hint;
    else hint.remove();
    button.querySelector(".menu-detail")!.textContent = [item.detail, item.checked ? "✓" : ""].filter(Boolean).join(" ");
    button.addEventListener("click", () => {
      close();
      item.run();
    });
    menu.append(button);
    return button;
  });

  document.body.append(menu);
  position(menu, anchor);
  anchor.setAttribute("aria-expanded", "true");

  const focusable = () => buttons.filter((button) => !button.disabled);
  const move = (step: number) => {
    const enabled = focusable();
    const index = enabled.indexOf(document.activeElement as HTMLButtonElement);
    enabled[(index + step + enabled.length) % enabled.length]?.focus();
  };
  const onKey = (event: KeyboardEvent) => {
    if (event.key === "Escape") {
      event.preventDefault();
      event.stopPropagation();
      close();
    } else if (event.key === "ArrowDown") {
      event.preventDefault();
      move(1);
    } else if (event.key === "ArrowUp") {
      event.preventDefault();
      move(-1);
    } else if (event.key === "Tab") {
      close();
    } else if (event.key.length === 1) {
      const match = focusable().find((button) =>
        button.textContent?.trim().toLowerCase().startsWith(event.key.toLowerCase()));
      match?.focus();
    }
  };
  const onPointer = (event: PointerEvent) => {
    if (!menu.contains(event.target as Node) && !anchor.contains(event.target as Node)) close(false);
  };
  const close = (restoreFocus = true) => {
    if (closeOpenMenu !== close) return;
    closeOpenMenu = null;
    menu.remove();
    anchor.setAttribute("aria-expanded", "false");
    document.removeEventListener("keydown", onKey, true);
    document.removeEventListener("pointerdown", onPointer, true);
    window.removeEventListener("resize", onResize);
    if (restoreFocus) anchor.focus();
  };
  const onResize = () => close(false);
  closeOpenMenu = close;
  document.addEventListener("keydown", onKey, true);
  document.addEventListener("pointerdown", onPointer, true);
  window.addEventListener("resize", onResize);
  (buttons.find((button, index) => items[index]!.checked && !button.disabled) ?? focusable()[0])?.focus();
}

/** Places the menu below the anchor, or above when there is no room, inside the viewport. */
function position(menu: HTMLElement, anchor: HTMLElement) {
  const rect = anchor.getBoundingClientRect();
  const width = menu.offsetWidth;
  const height = menu.offsetHeight;
  const margin = 8;
  const below = rect.bottom + 6 + height <= innerHeight - margin;
  const top = below ? rect.bottom + 6 : Math.max(margin, rect.top - 6 - height);
  const left = Math.min(Math.max(margin, rect.left), innerWidth - width - margin);
  menu.style.top = `${top}px`;
  menu.style.left = `${left}px`;
}
