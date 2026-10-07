import { icon } from "./icons";
import { escapeHtml } from "./markup";
import {
  expandRange,
  moveRangeBoundary,
  rangePresets,
  rangesEqual,
  targetLabel,
  type RangeBoundary,
  type ReviewRange,
  type ReviewTarget,
} from "./range-selection";

export type RangeDialogDeps = {
  /** The branch timeline that ranges index into. */
  targets(): ReviewTarget[];
  /** The range of the installed page. */
  currentRange(): ReviewRange;
  /** Whether the reviewer may start choosing another range. */
  canChange(): boolean;
  /** Describes the feedback that switching ranges would discard; empty when there is none. */
  pendingFeedback(): string;
  selectRange(range: ReviewRange, discardCurrentFeedback: boolean): Promise<void>;
};

/**
 * The range selector dialog. It owns the range being edited, and a previewed range while the
 * reviewer hovers a boundary action, until the reviewer applies or cancels it; applying hands the
 * range back to the panel to load.
 */
export class RangeDialog {
  private pendingRange?: ReviewRange;
  private previewRange?: ReviewRange;

  constructor(
    private readonly root: HTMLElement,
    private readonly deps: RangeDialogDeps,
  ) {}

  markup() {
    return `<dialog class="range-dialog" id="range-dialog" aria-labelledby="range-title" aria-describedby="range-description">
            <header>
              <div>
                <span class="dialog-eyebrow">Review scope</span>
                <h2 id="range-title">Choose a change range</h2>
                <p id="range-description">Click outside the range to widen it. Drag a handle or use an interior action to shrink it.</p>
              </div>
              <button class="icon-button" data-range-close aria-label="Close range selector">${icon("close")}</button>
            </header>
            <div class="range-builder">
              <div class="segmented range-presets" id="range-presets" role="group" aria-label="Quick ranges">
                ${rangePresets(this.deps.targets()).map((preset) => `<button type="button" class="segment" data-range-preset="${preset.id}" aria-pressed="false">${preset.label}</button>`).join("")}
              </div>
              <div class="range-endpoints" aria-label="Selected range endpoints" aria-live="polite">
                <div class="range-endpoint">
                  <small>Base (excluded)</small><strong id="range-from-label"></strong><span id="range-from-title"></span>
                </div>
                <span class="range-direction">${icon("arrow-right")}</span>
                <div class="range-endpoint">
                  <small>Through (included)</small><strong id="range-to-label"></strong><span id="range-to-title"></span>
                </div>
              </div>
              <div class="commit-timeline" id="commit-timeline" aria-label="Branch timeline">
                ${this.timelineMarkup()}
              </div>
            </div>
            <div class="range-warning" id="range-warning" hidden></div>
            <footer>
              <span><kbd>Esc</kbd> to cancel</span>
              <div>
                <button class="button quiet" data-range-close>Cancel</button>
                <button class="button primary" id="apply-range">Apply range</button>
              </div>
            </footer>
        </dialog>`;
  }

  bind() {
    this.root.querySelector("#range-button")?.addEventListener("click", () => this.open());
    for (const button of this.root.querySelectorAll<HTMLButtonElement>("[data-range-preset]")) {
      button.addEventListener("click", () => this.previewPreset(button.dataset.rangePreset));
    }
    this.bindTimeline();
    for (const button of this.root.querySelectorAll<HTMLButtonElement>("[data-range-close]")) {
      button.addEventListener("click", () => this.close());
    }
    this.root.querySelector("#apply-range")?.addEventListener("click", () => void this.apply());
    const rangeDialog = this.root.querySelector<HTMLDialogElement>("#range-dialog");
    rangeDialog?.addEventListener("cancel", (event) => {
      event.preventDefault();
      this.close();
    });
    rangeDialog?.addEventListener("click", (event) => {
      if (event.target === rangeDialog) this.close();
    });
  }

  /** Redraws the timeline after a snapshot changed the branch's commits. */
  replaceTimeline() {
    const timeline = this.root.querySelector<HTMLElement>("#commit-timeline");
    if (!timeline) return;
    timeline.innerHTML = this.timelineMarkup();
    this.bindTimeline();
  }

  private timelineMarkup() {
    return this.deps.targets().map((target) => `
      <div class="commit-target" data-range-target="${target.index}">
        <button class="commit-target-main" data-range-expand aria-label="Expand range through ${escapeHtml(targetLabel(target))}: ${escapeHtml(target.title)}">
          <span class="commit-rail"><i></i><b></b></span>
          <span class="commit-id">${escapeHtml(targetLabel(target))}</span>
          <span class="commit-copy">
            <strong>${escapeHtml(target.title)}</strong>
            <small>${target.kind === "trunk" ? "Trunk base" : target.kind === "working_tree" ? "Working tree" : "Commit"}</small>
          </span>
          <span class="row-control-space"></span>
        </button>
        <span class="endpoint-handles">
          <button data-boundary-handle="from" aria-label="Drag Base excluded boundary; use arrow keys to move">↕ Base</button>
          <button data-boundary-handle="to" aria-label="Drag Through included boundary; use arrow keys to move">↕ Through</button>
        </span>
        <span class="boundary-actions">
          <button data-move-boundary="from">Move Base</button>
          <button data-move-boundary="to">Move Through</button>
        </span>
      </div>`).join("");
  }

  private bindTimeline() {
    for (const button of this.root.querySelectorAll<HTMLButtonElement>("[data-range-expand]")) {
      button.addEventListener("click", () => {
        if (!this.pendingRange) return;
        const index = this.rangeTargetIndex(button);
        const expanded = expandRange(this.pendingRange, index);
        if (expanded === this.pendingRange) {
          this.closeBoundaryActions();
          button.closest("[data-range-target]")?.classList.add("actions-open");
          return;
        }
        this.closeBoundaryActions();
        this.pendingRange = expanded;
        this.syncRangeSelector();
      });
    }
    for (const button of this.root.querySelectorAll<HTMLButtonElement>("[data-move-boundary]")) {
      const boundary = button.dataset.moveBoundary as RangeBoundary;
      const preview = () => this.previewBoundaryMove(boundary, this.rangeTargetIndex(button));
      button.addEventListener("pointerenter", preview);
      button.addEventListener("focus", preview);
      button.addEventListener("pointerleave", () => this.clearRangePreview());
      button.addEventListener("blur", () => this.clearRangePreview());
      button.addEventListener("click", () => this.commitBoundaryMove(boundary, this.rangeTargetIndex(button)));
    }
    for (const handle of this.root.querySelectorAll<HTMLButtonElement>("[data-boundary-handle]")) {
      const boundary = handle.dataset.boundaryHandle as RangeBoundary;
      handle.addEventListener("keydown", (event) => this.moveBoundaryWithKeyboard(event, boundary));
      handle.addEventListener("pointerdown", (event) => this.startBoundaryDrag(event, boundary));
    }
  }

  open() {
    if (!this.deps.canChange()) return;
    this.closeBoundaryActions();
    this.pendingRange = { ...(this.deps.currentRange()) };
    this.previewRange = undefined;
    this.syncRangeSelector();
    this.root.querySelector<HTMLDialogElement>("#range-dialog")?.showModal();
    this.root.querySelector("#range-button")?.setAttribute("aria-expanded", "true");
    queueMicrotask(() => {
      const from = this.root.querySelector<HTMLButtonElement>(".commit-target.pending-from [data-boundary-handle=from]");
      from?.scrollIntoView({ block: "center" });
      from?.focus();
    });
  }

  private close() {
    const dialog = this.root.querySelector<HTMLDialogElement>("#range-dialog");
    if (dialog?.open) dialog.close();
    this.root.querySelector("#range-button")?.setAttribute("aria-expanded", "false");
    this.closeBoundaryActions();
    this.pendingRange = undefined;
    this.previewRange = undefined;
    this.root.querySelector<HTMLButtonElement>("#range-button")?.focus();
  }

  private syncRangeSelector() {
    const range = this.previewRange ?? this.pendingRange;
    if (!range) return;
    const from = this.deps.targets()[range.from];
    const to = this.deps.targets()[range.to];
    this.setRangeEndpointText("from", from);
    this.setRangeEndpointText("to", to);
    const pending = this.pendingRange ?? range;
    const timeline = this.root.querySelector<HTMLElement>("#commit-timeline");
    timeline?.classList.toggle("previewing", this.previewRange !== undefined);
    for (const target of this.root.querySelectorAll<HTMLElement>("[data-range-target]")) {
      const index = Number(target.dataset.rangeTarget);
      target.classList.toggle("from", index === range.from);
      target.classList.toggle("to", index === range.to);
      target.classList.toggle("included", index > range.from && index < range.to);
      target.classList.toggle("pending-from", index === pending.from);
      target.classList.toggle("pending-to", index === pending.to);
      target.classList.toggle("interior", index > pending.from && index < pending.to);
      target.classList.toggle("outside", index < pending.from || index > pending.to);
    }
    for (const button of this.root.querySelectorAll<HTMLButtonElement>("[data-range-preset]")) {
      const preset = rangePresets(this.deps.targets()).find((candidate) => candidate.id === button.dataset.rangePreset);
      button.setAttribute("aria-pressed", String(preset !== undefined && rangesEqual(preset.range, pending)));
    }
    this.syncRangeWarning();
  }

  private rangeTargetIndex(element: Element) {
    return Number(element.closest<HTMLElement>("[data-range-target]")?.dataset.rangeTarget);
  }

  private previewBoundaryMove(boundary: RangeBoundary, index: number) {
    if (!this.pendingRange) return;
    this.previewRange = moveRangeBoundary(this.pendingRange, boundary, index);
    this.syncRangeSelector();
  }

  private clearRangePreview() {
    if (!this.previewRange) return;
    this.previewRange = undefined;
    this.syncRangeSelector();
  }

  private commitBoundaryMove(boundary: RangeBoundary, index: number) {
    if (!this.pendingRange) return;
    this.closeBoundaryActions();
    this.pendingRange = moveRangeBoundary(this.pendingRange, boundary, index);
    this.previewRange = undefined;
    this.syncRangeSelector();
  }

  private closeBoundaryActions() {
    for (const target of this.root.querySelectorAll(".commit-target.actions-open")) {
      target.classList.remove("actions-open");
    }
  }

  private moveBoundaryWithKeyboard(event: KeyboardEvent, boundary: RangeBoundary) {
    if (!this.pendingRange) return;
    const current = this.pendingRange[boundary];
    let target = current;
    if (event.key === "ArrowUp" || event.key === "ArrowLeft") target--;
    else if (event.key === "ArrowDown" || event.key === "ArrowRight") target++;
    else if (event.key === "Home") target = boundary === "from" ? 0 : this.pendingRange.from + 1;
    else if (event.key === "End") target = boundary === "from" ? this.pendingRange.to - 1 : this.deps.targets().length - 1;
    else return;
    event.preventDefault();
    this.commitBoundaryMove(boundary, target);
    this.root.querySelector<HTMLButtonElement>(`.commit-target.pending-${boundary} [data-boundary-handle=${boundary}]`)?.focus();
  }

  private startBoundaryDrag(event: PointerEvent, boundary: RangeBoundary) {
    if (event.button !== 0) return;
    event.preventDefault();
    document.body.classList.add("range-dragging");
    const move = (pointer: PointerEvent) => {
      pointer.preventDefault();
      const target = document.elementFromPoint(pointer.clientX, pointer.clientY);
      if (target?.closest("[data-range-target]")) {
        this.commitBoundaryMove(boundary, this.rangeTargetIndex(target));
      }
    };
    const finish = () => {
      document.body.classList.remove("range-dragging");
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", finish);
      window.removeEventListener("pointercancel", finish);
      this.root.querySelector<HTMLButtonElement>(`.commit-target.pending-${boundary} [data-boundary-handle=${boundary}]`)?.focus();
    };
    window.addEventListener("pointermove", move, { passive: false });
    window.addEventListener("pointerup", finish);
    window.addEventListener("pointercancel", finish);
  }

  private setRangeEndpointText(endpoint: "from" | "to", target: ReviewTarget) {
    const label = this.root.querySelector<HTMLElement>(`#range-${endpoint}-label`);
    const title = this.root.querySelector<HTMLElement>(`#range-${endpoint}-title`);
    if (label) label.textContent = targetLabel(target);
    if (title) title.textContent = target.title;
  }

  private syncRangeWarning() {
    const warning = this.root.querySelector<HTMLElement>("#range-warning");
    const apply = this.root.querySelector<HTMLButtonElement>("#apply-range");
    const currentRange = this.deps.currentRange();
    const changesRange = this.pendingRange !== undefined && !rangesEqual(this.pendingRange, currentRange);
    const pendingFeedback = this.deps.pendingFeedback();
    const hasPendingFeedback = pendingFeedback.length > 0;
    if (apply) {
      apply.disabled = !changesRange;
      apply.textContent = changesRange && hasPendingFeedback ? "Discard feedback and apply" : "Apply range";
    }
    if (!warning) return;
    if (!changesRange || !hasPendingFeedback) {
      warning.hidden = true;
      return;
    }
    warning.textContent = `Switching ranges will discard ${pendingFeedback}.`;
    warning.hidden = false;
  }

  private previewPreset(id: string | undefined) {
    const preset = rangePresets(this.deps.targets()).find((candidate) => candidate.id === id);
    if (!preset || !this.pendingRange) return;
    this.pendingRange = { ...preset.range };
    this.previewRange = undefined;
    this.closeBoundaryActions();
    this.syncRangeSelector();
  }

  private async apply() {
    const range = this.pendingRange;
    const discardFeedback = this.deps.pendingFeedback().length > 0;
    this.close();
    if (range) await this.deps.selectRange(range, discardFeedback);
  }
}
