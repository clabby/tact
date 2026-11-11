import {
  CodeView,
  type CodeViewLineSelection,
  type DiffLineAnnotation,
  type FileDiffMetadata,
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
  /** The files the viewer shows, in order. */
  items(): readonly ReviewDiffItem[];
  /** The paths the reviewer marked as seen and has not seen change since. */
  seenFiles(): ReadonlySet<string>;
  /** The reviewer marked `file` as seen, or took the mark back. */
  seenChanged(file: FileDiffMetadata, seen: boolean): void;
  /** The reviewer opened the file at `path` themselves, so nothing should close it on their behalf. */
  fileExpanded(path: string): void;
  renderAnnotation(annotation: DiffLineAnnotation<AnnotationMetadata>): HTMLElement;
  openComposer(selection: CodeViewLineSelection | null): void;
  selectedLinesChanged(selection: CodeViewLineSelection | null): void;
  itemRendered(itemId: string, root: ShadowRoot | null): void;
  /** Actions offered when the range has no changes. */
  changeRange(): void;
  refresh(): void;
};

/** How far the pointer may move between press and release for a header press to count as a click. */
const HEADER_CLICK_SLOP_PX = 4;

/**
 * The virtualized diff. It owns the code viewer and the workers that highlight it, and versions
 * items so the viewer redraws exactly the files whose content, annotations, or collapsed state
 * changed. Clicking a file header collapses or expands that file, and Alt+click applies the same
 * to every file. The header also carries the reviewer's seen toggle, which collapses the file too.
 */
export class DiffView {
  private codeView?: CodeView<AnnotationMetadata>;
  private readonly workerPool: WorkerPoolManager;
  private itemVersion = 0;
  /** Where the last mouse or pen press started, to tell a click from a drag that selects the file name. */
  private headerPress?: { x: number; y: number };
  /** The file whose collapse toggle takes the keyboard focus back once its header is redrawn. */
  private refocusToggle?: string;
  private readonly rememberHeaderPress = (event: PointerEvent) => {
    // A touch that moves scrolls instead of selecting, and the browser then sends no click.
    this.headerPress = event.pointerType === "touch" ? undefined : { x: event.clientX, y: event.clientY };
  };
  private readonly handleHeaderClick = (event: MouseEvent) => {
    const press = this.headerPress;
    this.headerPress = undefined;
    // A repeated click is part of a double click that selects a word in the file name.
    if (event.button !== 0 || event.detail > 1) return;
    if (press && Math.hypot(event.clientX - press.x, event.clientY - press.y) > HEADER_CLICK_SLOP_PX) return;
    const path = event.composedPath().filter((target) => target instanceof HTMLElement);
    const header = path.find((element) => element.hasAttribute("data-diffs-header"));
    // The metadata holds the line counts and the seen toggle, which are not part of the header's toggle.
    if (!header || path.some((element) => element.hasAttribute("data-metadata"))) return;
    const host = (header.getRootNode() as ShadowRoot).host;
    const id = this.codeView?.getRenderedItems().find((rendered) => rendered.element === host)?.id;
    const item = this.deps.items().find((candidate) => candidate.id === id);
    if (!item) return;
    // Keyboard activation replaces the focused toggle as the header redraws, so it must refocus.
    const fromKeyboard = event.detail === 0 && path.some((element) => element.classList.contains("file-collapse-toggle"));
    this.refocusToggle = fromKeyboard ? item.id : undefined;
    this.toggleCollapsed(item, header, event.altKey);
  };

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
      container.addEventListener("pointerdown", this.rememberHeaderPress);
      container.addEventListener("click", this.handleHeaderClick);
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
        [data-diffs-header="default"] {
          --tact-header-rule: light-dark(
            color-mix(in lab, var(--diffs-bg) 90%, var(--diffs-mixer)),
            color-mix(in lab, var(--diffs-bg) 86%, var(--diffs-mixer))
          );
          background-color: light-dark(
            color-mix(in lab, var(--diffs-bg) 97%, var(--diffs-mixer)),
            color-mix(in lab, var(--diffs-bg) 94%, var(--diffs-mixer))
          );
          box-shadow: inset 0 1px 0 var(--tact-header-rule), inset 0 -1px 0 var(--tact-header-rule);
          cursor: pointer;
        }
        [data-diffs-header="default"] [data-metadata] { cursor: auto; }
        [data-diffs-header="default"] [data-additions-count] { color: var(--add); }
        [data-diffs-header="default"] [data-deletions-count] { color: var(--del); }`,
      hunkSeparators: "line-info" as const,
      expansionLineCount: 20,
      enableLineSelection: true,
      stickyHeaders: true,
      layout: { paddingTop: 8, paddingBottom: 8, gap: 16 },
      pointerEventsOnScroll: false,
      lineHoverHighlight: "both" as const,
      renderHeaderPrefix: (_file, context) => {
        if (context.item.type !== "diff") return null;
        return this.collapseToggle(context.item);
      },
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

  private collapseToggle(item: ReviewDiffItem) {
    const collapsed = item.collapsed === true;
    const button = document.createElement("button");
    button.type = "button";
    button.className = "file-collapse-toggle";
    button.innerHTML = icon("chevron-down");
    button.setAttribute("aria-expanded", String(!collapsed));
    button.setAttribute("aria-label", `${collapsed ? "Expand" : "Collapse"} ${item.fileDiff.name}`);
    button.title = `${collapsed ? "Expand" : "Collapse"} file (Alt+click for all files)`;
    if (this.refocusToggle === item.id) {
      this.refocusToggle = undefined;
      // The viewer attaches the toggle after this returns.
      queueMicrotask(() => button.focus({ preventScroll: true }));
    }
    return button;
  }

  /**
   * Collapses or expands `item`, or with `all` gives every file the state `item` takes. The clicked
   * header keeps its place on screen. A sticky header sits at the top while its file scrolls under
   * it, and without this, collapsing the file would leave the view somewhere in a later file.
   */
  private toggleCollapsed(item: ReviewDiffItem, header: HTMLElement, all: boolean) {
    const viewport = this.codeView?.getContainerElement();
    if (!viewport) return;
    const offset = Math.max(0, header.getBoundingClientRect().top - viewport.getBoundingClientRect().top);
    const collapsed = item.collapsed !== true;
    for (const target of all ? this.deps.items() : [item]) {
      if (!collapsed) this.deps.fileExpanded(target.id);
      if ((target.collapsed === true) !== collapsed) this.setCollapsed(target, collapsed);
    }
    this.codeView?.scrollTo({ type: "item", id: item.id, align: "start", offset, behavior: "instant" });
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
    const seen = !this.deps.seenFiles().has(item.fileDiff.name);
    this.setCollapsed(item, seen);
    if (!seen) this.deps.fileExpanded(item.fileDiff.name);
    this.deps.seenChanged(item.fileDiff, seen);
  }
}
