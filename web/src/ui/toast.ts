let region: HTMLElement | null = null;

/** Shows a transient, non-blocking message for failures that have no inline home. */
export function toast(message: string, tone: "info" | "warning" | "danger" = "info") {
  if (!region) {
    region = document.createElement("div");
    region.className = "toasts";
    region.setAttribute("role", "status");
    region.setAttribute("aria-live", "polite");
    document.body.append(region);
  }
  const item = document.createElement("div");
  item.className = `toast ${tone}`;
  item.textContent = message;
  region.append(item);
  setTimeout(() => {
    item.classList.add("leaving");
    setTimeout(() => item.remove(), 200);
  }, 4200);
}
