import { describeError } from "./api-client";
import { glyph, type GlyphName } from "./glyphs";

export type PaletteCommand = {
  id: string;
  title: string;
  group: string;
  icon?: GlyphName;
  /** Right-aligned context, e.g. the current value or a shortcut. */
  hint?: string;
  keywords?: string;
  /** A second line, e.g. a prompt's age and workspace. */
  detail?: string;
  run(): void;
};

/** Supplies the commands that apply right now; called each time the palette filters. */
export type PaletteProvider = () => PaletteCommand[];

/** A picker over items the server ranks (recent prompts, history); queried as the user types. */
export type PickerSource = {
  placeholder: string;
  empty: string;
  load(query: string, signal: AbortSignal): Promise<PaletteCommand[]>;
};

/**
 * Scores how well `query` matches `text` as an in-order subsequence; null means no match.
 * Matches at word starts and consecutive runs score higher, so "nc" ranks "New chat" first.
 */
export function matchScore(query: string, text: string): number | null {
  const needle = query.trim().toLowerCase();
  if (!needle) return 0;
  const haystack = text.toLowerCase();
  let score = 0;
  let position = -1;
  let run = 0;
  for (const character of needle) {
    if (character === " ") continue;
    const found = haystack.indexOf(character, position + 1);
    if (found < 0) return null;
    run = found === position + 1 ? run + 1 : 0;
    const wordStart = found === 0 || /[\s/_\-.:]/.test(haystack[found - 1]!);
    score += 1 + run * 2 + (wordStart ? 4 : 0) - Math.min(found - position - 1, 6) * 0.25;
    position = found;
  }
  return score;
}

/** Ranks registry commands for `query`: title matches first, then group and keyword matches. */
export function rankCommands(commands: readonly PaletteCommand[], query: string): PaletteCommand[] {
  if (!query.trim()) return [...commands];
  return commands
    .flatMap((command, order) => {
      const title = matchScore(query, command.title);
      const any = matchScore(query, `${command.title} ${command.group} ${command.keywords ?? ""}`);
      if (title === null && any === null) return [];
      return [{ command, order, score: title === null ? any! : title + 100 }];
    })
    .sort((a, b) => b.score - a.score || a.order - b.order)
    .map(({ command }) => command);
}

/**
 * The command palette (Cmd/Ctrl+K). Every feature contributes commands to one registry; the same
 * dialog also serves server-ranked pickers such as recent prompts.
 */
export class Palette {
  private providers: PaletteProvider[] = [];
  private dialog: HTMLDialogElement;
  private input: HTMLInputElement;
  private list: HTMLElement;
  private results: PaletteCommand[] = [];
  private selected = 0;
  private picker: PickerSource | null = null;
  private request: AbortController | null = null;
  private restoreFocus: HTMLElement | null = null;

  constructor() {
    this.dialog = document.createElement("dialog");
    this.dialog.className = "palette";
    this.dialog.setAttribute("aria-label", "Command palette");
    this.dialog.innerHTML = `<div class="palette-search">${glyph("search")}<input type="text" role="combobox" aria-expanded="true" aria-controls="palette-list" aria-autocomplete="list" autocomplete="off" spellcheck="false"><kbd>esc</kbd></div><ul id="palette-list" class="palette-list" role="listbox"></ul>`;
    document.body.append(this.dialog);
    this.input = this.dialog.querySelector("input")!;
    this.list = this.dialog.querySelector(".palette-list")!;
    this.input.addEventListener("input", () => void this.filter());
    this.input.addEventListener("keydown", (event) => this.onKey(event));
    this.dialog.addEventListener("close", () => {
      this.request?.abort();
      this.restoreFocus?.focus({ preventScroll: true });
    });
    this.dialog.addEventListener("click", (event) => {
      if (event.target === this.dialog) this.dialog.close();
    });
    this.list.addEventListener("click", (event) => {
      const item = (event.target as HTMLElement).closest<HTMLElement>("[data-index]");
      if (item) this.choose(Number(item.dataset.index));
    });
  }

  register(provider: PaletteProvider) {
    this.providers.push(provider);
  }

  /** The commands that apply right now and match `query`, best first. */
  matching(query: string) {
    return rankCommands(this.providers.flatMap((provider) => provider()), query);
  }

  get isOpen() {
    return this.dialog.open;
  }

  /** Opens the command registry. */
  open(query = "") {
    this.show(null, query);
  }

  /** Opens a server-ranked picker. */
  pick(source: PickerSource) {
    this.show(source, "");
  }

  close() {
    this.dialog.close();
  }

  private show(picker: PickerSource | null, query: string) {
    if (this.dialog.open) this.dialog.close();
    this.restoreFocus = document.activeElement as HTMLElement | null;
    this.picker = picker;
    this.input.placeholder = picker?.placeholder ?? "Search sessions and actions";
    this.input.value = query;
    this.dialog.showModal();
    void this.filter();
    this.input.focus();
  }

  private async filter() {
    const query = this.input.value;
    this.request?.abort();
    if (!this.picker) {
      this.results = rankCommands(this.providers.flatMap((provider) => provider()), query);
      this.selected = 0;
      this.render(query.trim() === "", "No matching commands");
      return;
    }
    const picker = this.picker;
    const request = new AbortController();
    this.request = request;
    this.list.setAttribute("aria-busy", "true");
    try {
      const results = await picker.load(query, request.signal);
      if (this.request !== request) return;
      this.results = results;
      this.selected = 0;
      this.render(false, picker.empty);
    } catch (error) {
      if (request.signal.aborted) return;
      this.results = [];
      this.render(false, describeError(error));
    } finally {
      if (this.request === request) this.list.removeAttribute("aria-busy");
    }
  }

  private render(grouped: boolean, empty: string) {
    const items: HTMLElement[] = [];
    let group = "";
    this.results.forEach((command, index) => {
      if (grouped && command.group !== group) {
        group = command.group;
        const heading = document.createElement("li");
        heading.className = "palette-group";
        heading.setAttribute("role", "presentation");
        heading.textContent = group;
        items.push(heading);
      }
      const item = document.createElement("li");
      item.className = "palette-item";
      item.id = `palette-item-${index}`;
      item.dataset.index = String(index);
      item.setAttribute("role", "option");
      item.setAttribute("aria-selected", String(index === this.selected));
      item.innerHTML = `${glyph(command.icon ?? "chevron-right")}<span class="palette-text"><span class="palette-title"></span><span class="palette-detail"></span></span><span class="palette-hint"></span>`;
      item.querySelector(".palette-title")!.textContent = command.title;
      const detail = item.querySelector<HTMLElement>(".palette-detail")!;
      if (command.detail) detail.textContent = command.detail;
      else detail.remove();
      item.querySelector(".palette-hint")!.textContent = command.hint ?? (grouped || this.picker ? "" : command.group);
      items.push(item);
    });
    if (items.length === 0) {
      const message = document.createElement("li");
      message.className = "palette-empty";
      message.textContent = empty;
      items.push(message);
    }
    this.list.replaceChildren(...items);
    this.input.setAttribute("aria-activedescendant", this.results.length ? `palette-item-${this.selected}` : "");
  }

  private onKey(event: KeyboardEvent) {
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      if (!this.results.length) return;
      const step = event.key === "ArrowDown" ? 1 : -1;
      this.selected = (this.selected + step + this.results.length) % this.results.length;
      for (const item of this.list.querySelectorAll<HTMLElement>(".palette-item")) {
        const selected = Number(item.dataset.index) === this.selected;
        item.setAttribute("aria-selected", String(selected));
        if (selected) item.scrollIntoView({ block: "nearest" });
      }
      this.input.setAttribute("aria-activedescendant", `palette-item-${this.selected}`);
    } else if (event.key === "Enter" && !event.isComposing) {
      event.preventDefault();
      this.choose(this.selected);
    }
  }

  private choose(index: number) {
    const command = this.results[index];
    if (!command) return;
    this.restoreFocus = null;
    this.dialog.close();
    command.run();
  }
}
