export type ThemePreference = "system" | "light" | "dark";
export type Theme = "light" | "dark";

const STORAGE_KEY = "tact.web.theme";

/**
 * The window-local colour scheme. It follows the system unless the user picked one, and publishes
 * the resolved theme on `<html data-theme>` where theme.css selects its tokens.
 */
export class ThemeController {
  private preference: ThemePreference;
  private readonly media = matchMedia("(prefers-color-scheme: dark)");
  private listeners = new Set<(theme: Theme) => void>();

  constructor() {
    const saved = localStorage.getItem(STORAGE_KEY);
    this.preference = saved === "light" || saved === "dark" ? saved : "system";
    this.media.addEventListener("change", () => {
      if (this.preference === "system") this.publish();
    });
    this.publish();
  }

  get current(): Theme {
    if (this.preference !== "system") return this.preference;
    return this.media.matches ? "dark" : "light";
  }

  get choice() {
    return this.preference;
  }

  set(preference: ThemePreference) {
    this.preference = preference;
    if (preference === "system") localStorage.removeItem(STORAGE_KEY);
    else localStorage.setItem(STORAGE_KEY, preference);
    this.publish();
  }

  /** Cycles system → light → dark. */
  cycle() {
    this.set(({ system: "light", light: "dark", dark: "system" } as const)[this.preference]);
  }

  subscribe(listener: (theme: Theme) => void) {
    this.listeners.add(listener);
    return () => void this.listeners.delete(listener);
  }

  private publish() {
    const theme = this.current;
    document.documentElement.dataset.theme = theme;
    document.querySelector('meta[name="theme-color"]')?.setAttribute("content", theme === "dark" ? "#111214" : "#fbfbfa");
    for (const listener of this.listeners) listener(theme);
  }
}
