import type { CodeView, CodeViewLineSelection, FileDiffMetadata } from "@pierre/diffs";
import type { AnnotationMetadata, ReviewDiffItem } from "./annotations";
import { moveSearchTarget, searchReview, type ReviewSearchMatch } from "./review-search";

/** The CSS highlight that marks the current match inside the diff's shadow roots. */
export const SEARCH_HIGHLIGHT = "tact-web-search-match";

export type SearchBarDeps = {
  /** Whether the panel is shown; the find shortcut belongs to the page while it is hidden. */
  visible(): boolean;
  files(): readonly FileDiffMetadata[];
  items(): readonly ReviewDiffItem[];
  viewer(): CodeView<AnnotationMetadata> | undefined;
  seenFiles(): ReadonlySet<string>;
  setCollapsed(item: ReviewDiffItem, collapsed: boolean): void;
  selectMobilePanel(name: "diff"): void;
};

/**
 * Find-in-changes for the virtualized diff. The search bar owns the current match and its
 * highlight, and the temporary view changes it makes to reveal a match: it replaces the line
 * selection and may expand a file marked as seen. Both are put back when the match moves on or
 * the search closes.
 */
export class SearchBar {
  private count = 0;
  private index = 0;
  private match?: ReviewSearchMatch;
  private paused = true;
  private selection?: CodeViewLineSelection | null;
  private expandedItem?: string;
  private returnFocus?: HTMLElement;
  private readonly handleShortcut = (event: KeyboardEvent) => {
    if (event.isComposing || this.root.querySelector("dialog[open]")) return;
    const key = event.key.toLowerCase();
    if ((event.metaKey || event.ctrlKey) && key === "f") {
      const changes = this.root.querySelector<HTMLElement>("#changes-panel");
      if (!this.deps.visible() || !this.root.contains(event.target as Node) || !changes?.matches(".active:not([hidden])")) return;
      event.preventDefault();
      this.open();
      return;
    }
    if ((event.metaKey || event.ctrlKey) && key === "g" && this.isOpen()) {
      event.preventDefault();
      this.move(event.shiftKey ? -1 : 1);
      return;
    }
    if (event.key === "Escape" && this.isOpen()) {
      event.preventDefault();
      this.close();
    }
  };

  constructor(
    private readonly root: HTMLElement,
    private readonly deps: SearchBarDeps,
  ) {}

  bind() {
    document.addEventListener("keydown", this.handleShortcut);
    const input = this.root.querySelector<HTMLInputElement>("#review-search-input");
    input?.addEventListener("input", () => this.update());
    input?.addEventListener("keydown", (event) => {
      if (event.key !== "Enter" || event.isComposing) return;
      event.preventDefault();
      this.move(event.shiftKey ? -1 : 1);
    });
    this.root.querySelector("[data-search-previous]")?.addEventListener("click", () => this.move(-1));
    this.root.querySelector("[data-search-next]")?.addEventListener("click", () => this.move(1));
    this.root.querySelector("[data-search-close]")?.addEventListener("click", () => this.close());
  }

  isOpen() {
    return this.root.querySelector<HTMLElement>("#review-search")?.hidden === false;
  }

  private open() {
    const panel = this.root.querySelector<HTMLElement>("#review-search");
    const input = this.root.querySelector<HTMLInputElement>("#review-search-input");
    if (!panel || !input) return;
    if (panel.hidden) {
      this.returnFocus = deepActiveElement(document);
      this.selection = this.deps.viewer()?.getSelectedLines() ?? null;
      panel.hidden = false;
    }
    input.focus();
    input.select();
  }

  close() {
    const panel = this.root.querySelector<HTMLElement>("#review-search");
    if (!panel || panel.hidden) return;
    const restoreFocus = panel.contains(document.activeElement);
    panel.hidden = true;
    this.paused = true;
    this.updateHighlight();
    this.restoreExpandedItem();
    this.restoreSelection();
    this.selection = undefined;
    if (restoreFocus) {
      const target = this.returnFocus?.isConnected
        ? this.returnFocus
        : this.root.querySelector<HTMLElement>("#diff-view");
      queueMicrotask(() => target?.focus());
    }
    this.returnFocus = undefined;
  }

  reset() {
    this.close();
    const input = this.root.querySelector<HTMLInputElement>("#review-search-input");
    if (input) input.value = "";
    this.count = 0;
    this.index = 0;
    this.match = undefined;
    this.paused = true;
    this.renderStatus();
  }


  /** Recounts matches after the files changed underneath an open search, without moving the view. */
  refreshCount() {
    const query = this.root.querySelector<HTMLInputElement>("#review-search-input")?.value ?? "";
    this.count = searchReview(this.deps.files(), query).count;
    this.index = Math.min(this.index, Math.max(0, this.count - 1));
    this.match = undefined;
    this.paused = true;
    this.updateHighlight();
    this.renderStatus();
  }

  private update() {
    const query = this.root.querySelector<HTMLInputElement>("#review-search-input")?.value ?? "";
    this.restoreExpandedItem();
    this.restoreSelection();
    const result = searchReview(this.deps.files(), query);
    this.count = result.count;
    this.index = 0;
    this.match = result.match;
    this.paused = false;
    this.renderStatus();
    if (result.match) this.reveal();
    else this.updateHighlight();
  }

  private move(direction: -1 | 1) {
    if (this.count === 0) return;
    const [index, occurrence] = moveSearchTarget(
      this.match,
      this.index,
      this.count,
      direction,
    );
    this.index = index;
    const query = this.root.querySelector<HTMLInputElement>("#review-search-input")?.value ?? "";
    this.match = searchReview(this.deps.files(), query, index, occurrence).match;
    this.paused = false;
    this.renderStatus();
    this.reveal();
  }

  private reveal() {
    const match = this.match;
    if (!match) return;
    this.updateHighlight();
    this.deps.selectMobilePanel("diff");
    if (match.kind === "path") {
      this.restoreExpandedItem();
      this.restoreSelection();
      this.deps.viewer()?.scrollTo({
        type: "item",
        id: match.itemId,
        align: "start",
        behavior: "smooth-auto",
      });
      return;
    }

    this.restoreExpandedItem(match.itemId);
    const item = this.deps.items().find((candidate) => candidate.id === match.itemId);
    if (item?.collapsed) {
      this.deps.setCollapsed(item, false);
      if (this.deps.seenFiles().has(item.id)) this.expandedItem = item.id;
    }
    this.deps.viewer()?.setSelectedLines({
      id: match.itemId,
      range: {
        start: match.lineNumber,
        end: match.lineNumber,
        side: match.side,
        endSide: match.side,
      },
    }, { notify: false });
    this.deps.viewer()?.scrollTo({
      type: "line",
      id: match.itemId,
      lineNumber: match.lineNumber,
      side: match.side,
      align: "center",
      behavior: "smooth-auto",
    });
  }

  private updateHighlight(
    root: ShadowRoot | null | undefined = this.deps.viewer()?.getRenderedItems()
      .find((candidate) => candidate.id === this.match?.itemId)?.element.shadowRoot,
  ) {
    CSS.highlights?.delete(SEARCH_HIGHLIGHT);
    const match = this.match;
    if (!CSS.highlights || this.paused || !this.isOpen() || match?.kind !== "content") return;
    const split = root?.querySelector(`[data-${match.side}]`);
    const lineType = match.side === "additions" ? "change-addition" : "change-deletion";
    const line = split?.querySelector<HTMLElement>(`[data-line="${match.lineNumber}"]`)
      ?? root?.querySelector<HTMLElement>(
        `[data-unified] [data-line="${match.lineNumber}"]:is([data-line-type="${lineType}"], [data-line-type="context"])`,
      );
    const range = line ? textRange(line, match.start, match.length) : undefined;
    if (range) CSS.highlights.set(SEARCH_HIGHLIGHT, new Highlight(range));
  }

  private restoreSelection() {
    if (this.selection === undefined) return;
    this.deps.viewer()?.setSelectedLines(this.selection, { notify: false });
  }

  clearSelectedLines() {
    if (this.isOpen()) this.selection = null;
    this.deps.viewer()?.clearSelectedLines();
  }

  private restoreExpandedItem(keepItem?: string) {
    const itemId = this.expandedItem;
    if (!itemId || itemId === keepItem) return;
    this.expandedItem = undefined;
    const item = this.deps.items().find((candidate) => candidate.id === itemId);
    if (!item || !this.deps.seenFiles().has(itemId) || item.collapsed) return;
    this.deps.setCollapsed(item, true);
  }

  private renderStatus() {
    const count = this.root.querySelector<HTMLElement>("#review-search-count");
    const previous = this.root.querySelector<HTMLButtonElement>("[data-search-previous]");
    const next = this.root.querySelector<HTMLButtonElement>("[data-search-next]");
    const hasMatches = this.count > 0;
    if (previous) previous.disabled = !hasMatches;
    if (next) next.disabled = !hasMatches;
    if (!count) return;
    const occurrence = this.match?.kind === "content" && this.match.occurrenceCount > 1
      ? ` · ${this.match.occurrenceIndex + 1} of ${this.match.occurrenceCount} on line`
      : "";
    count.textContent = hasMatches
      ? `${this.index + 1} of ${this.count}${occurrence}`
      : "0 of 0";
  }

  /** Stops listening for the find shortcut. */
  dispose() {
    document.removeEventListener("keydown", this.handleShortcut);
  }

  /** Tracks the reviewer's own line selection so closing the search restores it. */
  selectedLinesChanged(selection: CodeViewLineSelection | null) {
    if (this.isOpen()) this.selection = selection;
  }

  /** Re-applies the match highlight when the viewer renders or unmounts the matched item. */
  itemRendered(itemId: string, root: ShadowRoot | null) {
    if (itemId === this.match?.itemId) this.updateHighlight(root);
  }

  /** The reviewer expanded the file at `path` themselves, so search must not collapse it again. */
  fileExpanded(path: string) {
    if (this.expandedItem === path) this.expandedItem = undefined;
  }
}

function deepActiveElement(root: Document | ShadowRoot): HTMLElement | undefined {
  const active = root.activeElement;
  if (!(active instanceof HTMLElement)) return;
  return active.shadowRoot ? deepActiveElement(active.shadowRoot) ?? active : active;
}

function textRange(root: HTMLElement, start: number, length: number): Range | undefined {
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  let offset = 0;
  let first: Text | undefined;
  let firstOffset = 0;
  for (let node = walker.nextNode(); node; node = walker.nextNode()) {
    if (!(node instanceof Text)) continue;
    const end = offset + node.length;
    if (!first && start < end) {
      first = node;
      firstOffset = start - offset;
    }
    if (first && start + length <= end) {
      const range = document.createRange();
      range.setStart(first, firstOffset);
      range.setEnd(node, start + length - offset);
      return range;
    }
    offset = end;
  }
}
