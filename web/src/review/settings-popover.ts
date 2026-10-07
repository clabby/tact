import { icon } from "./icons";
import {
  loadReviewSettings,
  saveReviewSettings,
  type ReviewSettings,
  type SyntaxTheme,
} from "./review-settings";

/**
 * The review settings popover. It owns the reviewer's display settings: it loads them, keeps its
 * controls and the layout toggle in sync with them, and persists every change before asking the
 * panel to apply it.
 */
export class SettingsPopover {
  private settings = loadReviewSettings(window.localStorage, document.cookie);
  /** Closes the popover; the panel calls it for every click outside the popover. */
  readonly close = () => {
    const popover = this.root.querySelector<HTMLElement>("#settings-popover");
    if (!popover || popover.hidden) return;
    popover.hidden = true;
    this.root.querySelector("#settings-button")?.setAttribute("aria-expanded", "false");
  };

  constructor(
    private readonly root: HTMLElement,
    private readonly applySettings: () => void,
  ) {}

  get current(): ReviewSettings {
    return this.settings;
  }

  markup() {
    return `<button class="icon-button settings-button" id="settings-button" aria-label="Review settings" aria-expanded="false">
              ${icon("settings")}
            </button>
            <div class="settings-popover" id="settings-popover" hidden>
              <div class="settings-heading">Review settings</div>
              <label>
                <span>Syntax theme</span>
                <select data-setting="syntaxTheme">
                  <option value="system">System</option>
                  <option value="pierre-light">Pierre Light</option>
                  <option value="pierre-light-soft">Pierre Light Soft</option>
                  <option value="pierre-dark">Pierre Dark</option>
                  <option value="pierre-dark-soft">Pierre Dark Soft</option>
                </select>
              </label>
              <label>
                <span>Diff layout</span>
                <select data-setting="diffStyle">
                  <option value="unified">Unified</option>
                  <option value="split">Split</option>
                </select>
              </label>
              <label class="toggle-setting"><span>Wrap long lines</span><input type="checkbox" data-setting="wrapLines"></label>
              <label class="toggle-setting"><span>Line numbers</span><input type="checkbox" data-setting="lineNumbers"></label>
              <div class="settings-footer">Snapshot <span id="generation"></span></div>
            </div>`;
  }

  bind() {
    const button = this.root.querySelector<HTMLButtonElement>("#settings-button");
    const popover = this.root.querySelector<HTMLElement>("#settings-popover");
    button?.addEventListener("click", (event) => {
      event.stopPropagation();
      if (!popover) return;
      popover.hidden = !popover.hidden;
      button.setAttribute("aria-expanded", String(!popover.hidden));
    });
    popover?.addEventListener("click", (event) => event.stopPropagation());
    for (const button of this.root.querySelectorAll<HTMLElement>("[data-diff-style]")) {
      button.addEventListener("click", () => {
        const select = this.root.querySelector<HTMLSelectElement>("[data-setting=diffStyle]");
        if (!select || select.value === button.dataset.diffStyle) return;
        select.value = button.dataset.diffStyle!;
        select.dispatchEvent(new Event("change"));
      });
    }
    for (const control of this.root.querySelectorAll<HTMLInputElement | HTMLSelectElement>("[data-setting]")) {
      control.addEventListener("change", () => {
        this.readControls();
        this.syncControls();
        saveReviewSettings(window.localStorage, this.settings, (cookie) => {
          document.cookie = cookie;
        });
        this.applySettings();
      });
    }
  }

  syncControls() {
    const theme = this.root.querySelector<HTMLSelectElement>("[data-setting=syntaxTheme]");
    const layout = this.root.querySelector<HTMLSelectElement>("[data-setting=diffStyle]");
    const wrap = this.root.querySelector<HTMLInputElement>("[data-setting=wrapLines]");
    const lineNumbers = this.root.querySelector<HTMLInputElement>("[data-setting=lineNumbers]");
    if (theme) theme.value = this.settings.syntaxTheme;
    if (layout) layout.value = this.settings.diffStyle;
    for (const button of this.root.querySelectorAll<HTMLElement>("[data-diff-style]")) {
      button.setAttribute("aria-pressed", String(button.dataset.diffStyle === this.settings.diffStyle));
    }
    if (wrap) wrap.checked = this.settings.wrapLines;
    if (lineNumbers) lineNumbers.checked = this.settings.lineNumbers;
  }

  private readControls() {
    const theme = this.root.querySelector<HTMLSelectElement>("[data-setting=syntaxTheme]");
    const layout = this.root.querySelector<HTMLSelectElement>("[data-setting=diffStyle]");
    const wrap = this.root.querySelector<HTMLInputElement>("[data-setting=wrapLines]");
    const lineNumbers = this.root.querySelector<HTMLInputElement>("[data-setting=lineNumbers]");
    this.settings = {
      syntaxTheme: (theme?.value ?? "system") as SyntaxTheme,
      diffStyle: layout?.value === "split" ? "split" : "unified",
      wrapLines: wrap?.checked ?? false,
      lineNumbers: lineNumbers?.checked ?? true,
    };
  }
}
