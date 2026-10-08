import { glyph } from "../ui/glyphs";
import { findMatches, occurrences } from "./find";
import type { Transcript } from "./chat";

type HighlightRegistry = { set(name: string, highlight: unknown): void; delete(name: string): void };
type HighlightConstructor = new (...ranges: Range[]) => unknown;

/**
 * Find in the transcript: an inline bar over the chat that searches the loaded entries, steps
 * through matching entries with Enter and Shift+Enter, and reveals each one, unfolding turns and
 * groups and opening a command whose output matched. Matches in the current entry are highlighted
 * with the CSS Custom Highlight API where the browser has it.
 */
export class FindBar {
  private readonly element: HTMLElement;
  private readonly input: HTMLInputElement;
  private readonly count: HTMLElement;
  private matches: number[] = [];
  private index = -1;
  private rehighlight = 0;

  constructor(host: HTMLElement, private readonly transcript: Transcript) {
    this.element = document.createElement("div");
    this.element.className = "find-bar";
    this.element.setAttribute("role", "search");
    this.element.hidden = true;
    this.element.innerHTML = `${glyph("search")}<input type="text" placeholder="Find in transcript" aria-label="Find in transcript" spellcheck="false">
      <span class="find-count" aria-live="polite"></span>
      <button type="button" class="icon-button small find-prev" aria-label="Previous match" title="Previous (Shift Enter)">${glyph("arrow-up")}</button>
      <button type="button" class="icon-button small find-next" aria-label="Next match" title="Next (Enter)">${glyph("arrow-down")}</button>
      <button type="button" class="icon-button small find-close" aria-label="Close find" title="Close (Esc)">${glyph("close")}</button>`;
    host.append(this.element);
    this.input = this.element.querySelector("input")!;
    this.count = this.element.querySelector(".find-count")!;
    this.input.addEventListener("input", () => this.search());
    this.input.addEventListener("keydown", (event) => {
      if (event.key === "Enter") {
        event.preventDefault();
        this.step(event.shiftKey ? -1 : 1);
      } else if (event.key === "Escape") {
        // Esc here closes the bar; it must not reach the app, where it would arm an interrupt.
        event.preventDefault();
        event.stopPropagation();
        this.close();
      }
    });
    this.element.querySelector(".find-prev")!.addEventListener("click", () => this.step(-1));
    this.element.querySelector(".find-next")!.addEventListener("click", () => this.step(1));
    this.element.querySelector(".find-close")!.addEventListener("click", () => this.close());
  }

  get isOpen() {
    return !this.element.hidden;
  }

  open() {
    this.element.hidden = false;
    this.input.focus();
    this.input.select();
    if (this.input.value) this.search();
  }

  close() {
    this.element.hidden = true;
    this.matches = [];
    this.index = -1;
    clearTimeout(this.rehighlight);
    highlights()?.delete("find");
    highlights()?.delete("find-current");
  }

  private search() {
    const data = this.transcript.data();
    const query = this.input.value;
    const previous = this.matches[this.index];
    this.matches = data ? findMatches(data, query) : [];
    // Typing more keeps the current match when it still matches; otherwise the search starts from
    // the newest match, nearest the end the reader usually is at.
    const kept = previous === undefined ? -1 : this.matches.indexOf(previous);
    this.index = kept !== -1 ? kept : this.matches.length - 1;
    this.show();
  }

  private step(direction: 1 | -1) {
    if (!this.matches.length) return;
    this.index = (this.index + direction + this.matches.length) % this.matches.length;
    this.show();
  }

  private show() {
    const query = this.input.value.trim();
    this.count.textContent = this.matches.length ? `${this.index + 1}/${this.matches.length}` : query ? "No results" : "";
    this.element.classList.toggle("empty", query !== "" && !this.matches.length);
    highlights()?.delete("find");
    highlights()?.delete("find-current");
    const id = this.matches[this.index];
    if (id === undefined) return;
    let element = this.transcript.reveal(id, { scroll: false });
    if (element && !element.textContent?.toLowerCase().includes(query.toLowerCase())) {
      // The match is in a command's output, which shows once its row is open.
      this.transcript.openTool(id);
      element = this.transcript.reveal(id, { scroll: false });
    }
    if (!element) return;
    const first = this.highlight(element, query);
    const target = first?.getBoundingClientRect() ?? element.getBoundingClientRect();
    const scroller = element.closest<HTMLElement>(".transcript-scroller") ?? element.parentElement!;
    const box = scroller.getBoundingClientRect();
    if (target.top < box.top + 48 || target.bottom > box.bottom - 48) {
      scroller.scrollTo({ top: scroller.scrollTop + target.top - box.top - box.height / 3, behavior: "instant" });
    }
    // Settled Markdown upgrades from plain text as it nears the viewport, which replaces the nodes
    // the highlight points into.
    clearTimeout(this.rehighlight);
    this.rehighlight = window.setTimeout(() => {
      if (this.isOpen && this.matches[this.index] === id && element!.isConnected) this.highlight(element!, query);
    }, 300);
  }

  /** Highlights `query` in `element`'s text and returns the first match's range. */
  private highlight(element: HTMLElement, query: string): Range | null {
    const ranges: Range[] = [];
    const walker = document.createTreeWalker(element, NodeFilter.SHOW_TEXT, {
      acceptNode: (node) => (node.parentElement?.closest(".entry-link, .tool-name") ? NodeFilter.FILTER_REJECT : NodeFilter.FILTER_ACCEPT),
    });
    for (let node = walker.nextNode(); node; node = walker.nextNode()) {
      for (const start of occurrences(node.textContent ?? "", query)) {
        const range = document.createRange();
        range.setStart(node, start);
        range.setEnd(node, start + query.trim().length);
        ranges.push(range);
      }
    }
    const registry = highlights();
    const Highlight = (globalThis as { Highlight?: HighlightConstructor }).Highlight;
    if (registry && Highlight && ranges.length) {
      registry.set("find", new Highlight(...ranges.slice(1)));
      registry.set("find-current", new Highlight(ranges[0]!));
    }
    return ranges[0] ?? null;
  }
}

function highlights(): HighlightRegistry | undefined {
  return (globalThis.CSS as unknown as { highlights?: HighlightRegistry } | undefined)?.highlights;
}
