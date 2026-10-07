import { glyph } from "./glyphs";

export type Sheet = {
  body: HTMLElement;
  /** The header's trailing slot, for actions such as Save. */
  actions: HTMLElement;
  close(): void;
  onClose(listener: () => void): void;
};

/**
 * A modal sheet: centred on wide screens, full height from the bottom on phones. Escape, the close
 * button, and a backdrop click dismiss it; focus returns to where it was.
 */
export function openSheet(title: string, options: { wide?: boolean } = {}): Sheet {
  const restore = document.activeElement as HTMLElement | null;
  const dialog = document.createElement("dialog");
  dialog.className = `sheet${options.wide ? " wide" : ""}`;
  dialog.innerHTML = `<header class="sheet-head"><h2></h2><div class="sheet-actions"></div><button type="button" class="icon-button sheet-close" aria-label="Close">${glyph("close")}</button></header><div class="sheet-body"></div>`;
  dialog.querySelector("h2")!.textContent = title;
  dialog.setAttribute("aria-label", title);
  document.body.append(dialog);
  const listeners: (() => void)[] = [];
  const close = () => dialog.close();
  dialog.querySelector(".sheet-close")!.addEventListener("click", close);
  dialog.addEventListener("click", (event) => {
    if (event.target === dialog) close();
  });
  dialog.addEventListener("close", () => {
    dialog.remove();
    for (const listener of listeners) listener();
    restore?.focus({ preventScroll: true });
  });
  dialog.showModal();
  return {
    body: dialog.querySelector(".sheet-body")!,
    actions: dialog.querySelector(".sheet-actions")!,
    close,
    onClose: (listener) => listeners.push(listener),
  };
}

/** Fills \`container\` with a single muted line, e.g. a loading or error message. */
export function sheetMessage(container: HTMLElement, text: string, tone: "muted" | "danger" = "muted") {
  container.innerHTML = `<p class="sheet-message ${tone}"></p>`;
  container.firstElementChild!.textContent = text;
}
