import {
  CodeView,
  type CodeViewLineSelection,
  type DiffLineAnnotation,
} from "@pierre/diffs";
import { WorkerPoolManager } from "@pierre/diffs/worker";
import type { AnnotationMetadata, ReviewDiffItem } from "./annotations";
import { commentSelectionCallbacks } from "./comment-selection";
import { icon } from "./icons";
import { diffTheme, type ReviewSettings } from "./review-settings";
import { SEARCH_HIGHLIGHT } from "./search-bar";

export type DiffViewDeps = {
  settings(): ReviewSettings;
  appearance(): "light" | "dark";
  /** The paths the reviewer marked as seen; the seen toggle updates the set in place. */
  seenFiles(): Set<string>;
  seenChanged(path: string, seen: boolean): void;
  renderAnnotation(annotation: DiffLineAnnotation<AnnotationMetadata>): HTMLElement;
  openComposer(selection: CodeViewLineSelection | null): void;
  selectedLinesChanged(selection: CodeViewLineSelection | null): void;
  itemRendered(itemId: string, root: ShadowRoot | null): void;
  /** Actions offered when the range has no changes. */
  changeRange(): void;
  refresh(): void;
};

/**
 * The virtualized diff. It owns the code viewer and the workers that highlight it, and versions
 * items so the viewer redraws exactly the files whose content, annotations, or collapsed state
 * changed. Each file header carries the reviewer's seen toggle, which collapses the file.
 */
export class DiffView {
  private codeView?: CodeView<AnnotationMetadata>;
  private readonly workerPool: WorkerPoolManager;
  private itemVersion = 0;

  constructor(
    private readonly root: HTMLElement,
    private readonly deps: DiffViewDeps,
  ) {
    this.workerPool = new WorkerPoolManager(
      {
        workerFactory: () => new Worker(
          new URL("./worker.js", import.meta.url),
          { type: "module" },
        ),
      },
      { theme: diffTheme(deps.settings()) },
    );
  }

  /** The code viewer, while the page has changes to show. */
  get viewer() {
    return this.codeView;
  }

  /** A fresh version for an item whose content or annotations changed before it is shown. */
  nextVersion() {
    return ++this.itemVersion;
  }

  /** Redraws `item` after its annotations or collapsed state changed. */
  updateItem(item: ReviewDiffItem) {
    item.version = ++this.itemVersion;
    this.codeView?.updateItem(item);
  }

  setCollapsed(item: ReviewDiffItem, collapsed: boolean) {
    item.collapsed = collapsed;
    this.updateItem(item);
  }

  /** Sends a changed syntax theme to the workers that highlight the diff. */
  updateTheme() {
    void this.workerPool
      .setRenderOptions({ theme: diffTheme(this.deps.settings()) })
      .catch((error) => console.warn("Could not update the diff worker theme.", error));
  }

  dispose() {
    this.codeView?.cleanUp();
    this.workerPool.terminate();
  }

  render(items: ReviewDiffItem[], resetScroll = false) {
    const container = this.root.querySelector<HTMLElement>("#diff-view");
    if (!container) return;
    if (items.length === 0) {
      this.codeView?.cleanUp();
      this.codeView = undefined;
      container.replaceChildren();
      container.innerHTML = `
        <div class="empty-range">
          <strong>No changes in this range</strong>
          <span>Choose another range or refresh after changing the workspace.</span>
          <div><button class="button" data-empty-range-change>Change range</button><button class="button" data-empty-range-refresh>Refresh</button></div>
        </div>`;
      container.querySelector("[data-empty-range-change]")?.addEventListener("click", () => this.deps.changeRange());
      container.querySelector("[data-empty-range-refresh]")?.addEventListener("click", () => this.deps.refresh());
      return;
    }

    if (!this.codeView) {
      container.replaceChildren();
      this.codeView = new CodeView<AnnotationMetadata>(this.options(), this.workerPool);
      this.codeView.setup(container);
    } else {
      this.codeView.setOptions(this.options());
    }
    this.codeView.setItems(items);
    if (resetScroll) {
      this.codeView.scrollTo({ type: "position", position: 0, behavior: "instant" });
    }
  }

  private options() {
    return {
      diffStyle: this.deps.settings().diffStyle,
      overflow: this.deps.settings().wrapLines ? "wrap" as const : "scroll" as const,
      disableLineNumbers: !this.deps.settings().lineNumbers,
      theme: diffTheme(this.deps.settings()),
      themeType: this.deps.appearance(),
      unsafeCSS: `
        ::highlight(${SEARCH_HIGHLIGHT}) { color: #171717; background-color: #ffd54f; }
        [data-diffs-header="default"] [data-additions-count] { color: var(--add); }
        [data-diffs-header="default"] [data-deletions-count] { color: var(--del); }`,
      hunkSeparators: "line-info" as const,
      expansionLineCount: 20,
      enableLineSelection: true,
      stickyHeaders: true,
      pointerEventsOnScroll: false,
      lineHoverHighlight: "both" as const,
      renderHeaderMetadata: (_file, context) => {
        if (context.item.type !== "diff") return null;
        return this.seenButton(context.item);
      },
      onSelectedLinesChange: (selection) => this.deps.selectedLinesChanged(selection),
      onPostRender: (node, _instance, phase, context) => {
        this.deps.itemRendered(context.item.id, phase === "unmount" ? null : node.shadowRoot);
      },
      ...commentSelectionCallbacks((selection) => this.deps.openComposer(selection)),
      renderAnnotation: (annotation: DiffLineAnnotation<AnnotationMetadata>) => this.deps.renderAnnotation(annotation),
    };
  }

  private seenButton(item: ReviewDiffItem) {
    const seen = this.deps.seenFiles().has(item.fileDiff.name);
    const button = document.createElement("button");
    button.type = "button";
    button.className = `mark-seen-button${seen ? " seen" : ""}`;
    button.innerHTML = `${icon("check")}<span>${seen ? "Seen" : "Mark as Seen"}</span>`;
    button.setAttribute("aria-pressed", String(seen));
    button.title = seen ? "Mark as unseen and expand file" : "Mark as seen and collapse file";
    button.addEventListener("click", (event) => {
      event.stopPropagation();
      this.toggleSeen(item);
    });
    return button;
  }

  private toggleSeen(item: ReviewDiffItem) {
    const files = this.deps.seenFiles();
    const path = item.fileDiff.name;
    const seen = !files.has(path);
    if (seen) files.add(path);
    else files.delete(path);

    this.setCollapsed(item, seen);
    this.deps.seenChanged(path, seen);
  }
}
