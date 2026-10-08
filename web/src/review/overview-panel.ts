import { errorMessage, type ReviewApi } from "./review-api";
import { icon } from "./icons";
import { escapeHtml } from "./markup";
import { overviewProgram } from "./overview";
import { overviewInstructionError } from "./overview-instructions";
import type { ReviewPage, StoredOverview } from "./protocol";
import { rangeKey, rangesEqual, type ReviewRange } from "./range-selection";

export type OverviewPanelDeps = {
  api: ReviewApi;
  session(): string | null;
  page(): ReviewPage | undefined;
  /** The installed snapshot's generation. */
  generation(): number;
  /** The overview stored with the installed snapshot, if any. */
  storedOverview(): StoredOverview | null;
  agentUnavailable(): boolean;
  appearance(): "light" | "dark";
  syncAgentControls(): void;
  recordActionError(error: unknown): void;
};

/**
 * The Overview tab. It owns the agent-written overview of each range, the reviewer's instructions
 * for it, and the sandboxed frame that renders it. An overview written for an earlier snapshot
 * stays visible but is offered for regeneration.
 */
export class OverviewPanel {
  private readonly overviews = new Map<string, { mdx: string; instructions: string; generation: number }>();
  private readonly overviewInstructions = new Map<string, string>();
  private editingOverview = false;
  private loadingOverview?: ReviewRange;
  private overviewRequest = 0;

  constructor(
    private readonly root: HTMLElement,
    private readonly deps: OverviewPanelDeps,
  ) {}

  /** Whether an overview is being prepared. */
  get loading() {
    return this.loadingOverview !== undefined;
  }

  /** Forgets the previous session's overviews; a pending response is ignored when it arrives. */
  reset() {
    this.overviewRequest++;
    this.loadingOverview = undefined;
    this.overviews.clear();
    this.overviewInstructions.clear();
    this.editingOverview = false;
    this.setLoading(false);
  }

  cancelEditing() {
    this.editingOverview = false;
  }

  /** Adopts the overview stored with the installed snapshot. */
  restore() {
    const overview = this.deps.storedOverview();
    if (!overview) return;
    const key = rangeKey(overview.selected_range);
    const instructions = overview.instructions?.trim() ?? "";
    if (!this.overviewInstructions.has(key)) this.overviewInstructions.set(key, instructions);
    if (overview.status === "ready" && overview.overview_mdx?.trim()) {
      this.overviews.set(key, { mdx: overview.overview_mdx, instructions, generation: this.deps.generation() });
    }
  }

  render() {
    const state = this.root.querySelector<HTMLElement>("#overview-state");
    const frame = this.root.querySelector<HTMLIFrameElement>(".overview");
    const page = this.deps.page();
    if (!state || !frame || !page) return;
    state.removeAttribute("role");
    state.removeAttribute("aria-live");
    const key = rangeKey(page.selected_range);
    const ready = this.overviews.has(key);
    if (ready && !this.editingOverview) {
      state.hidden = false;
      state.classList.add("overview-state-ready");
      const outdated = this.current(key) === undefined;
      state.innerHTML = `${outdated ? '<span class="overview-outdated">Written for an earlier version of these changes.</span>' : ""}<button type="button" class="button" data-agent-action data-edit-overview ${this.deps.agentUnavailable() || this.loadingOverview ? "disabled" : ""}>${outdated ? "Regenerate" : "Edit instructions"}</button>`;
      state.querySelector("[data-edit-overview]")?.addEventListener("click", () => {
        if (this.deps.agentUnavailable()) return;
        this.editingOverview = true;
        this.render();
        state.querySelector<HTMLTextAreaElement>("[data-overview-instructions]")?.focus();
      });
      this.renderFrame();
      return;
    }
    if (!ready) {
      frame.hidden = true;
      frame.onload = null;
      frame.removeAttribute("src");
    }
    state.hidden = false;
    state.classList.remove("overview-state-ready");
    const draft = this.overviewInstructions.get(key) ?? "";
    const validationError = overviewInstructionError(draft);
    state.innerHTML = `
      ${ready ? "" : `<div class="overview-orbit">${icon("sparkles")}</div>
      <strong>Overview available on request</strong>
      <span>Get an explainer and a guide to the areas worth checking yourself.</span>`}
      <div class="overview-instructions">
        <label for="overview-instructions">Instructions for the overview <small>(optional)</small></label>
        <textarea id="overview-instructions" data-overview-instructions rows="4" aria-describedby="overview-instructions-help" placeholder="e.g. Focus on the migration and its rollback path"></textarea>
        <small id="overview-instructions-help" class="overview-instructions-help" aria-live="polite">${validationError ?? "Up to 8 KiB of text."}</small>
        <div class="overview-instructions-actions">
          ${ready ? `<button type="button" class="button" data-cancel-overview-edit>Cancel</button>` : ""}
          <button type="button" class="button primary" data-agent-action data-generate-overview ${this.deps.agentUnavailable() || this.loadingOverview || validationError || (ready && this.current(key)?.instructions === draft.trim()) ? "disabled" : ""}>${ready ? "Regenerate overview" : "Generate overview"}</button>
        </div>
      </div>`;
    const input = state.querySelector<HTMLTextAreaElement>("[data-overview-instructions]");
    if (input) {
      input.value = draft;
      input.addEventListener("input", () => {
        this.overviewInstructions.set(key, input.value);
        const error = overviewInstructionError(input.value);
        const help = state.querySelector<HTMLElement>("#overview-instructions-help");
        if (help) help.textContent = error ?? "Up to 8 KiB of text.";
        const generate = state.querySelector<HTMLButtonElement>("[data-generate-overview]");
        if (generate) generate.disabled = !!error || this.deps.agentUnavailable() || this.loadingOverview !== undefined
          || (ready && this.current(key)?.instructions === input.value.trim());
      });
    }
    state.querySelector("[data-cancel-overview-edit]")?.addEventListener("click", () => {
      this.editingOverview = false;
      this.render();
    });
    state.querySelector("[data-generate-overview]")?.addEventListener("click", () => void this.load());
    if (ready) this.renderFrame();
  }

  /** The overview for `key` if it describes the installed snapshot rather than an earlier one. */
  current(key: string) {
    const overview = this.overviews.get(key);
    return overview?.generation === this.deps.page()?.generation ? overview : undefined;
  }

  private setLoading(loading: boolean) {
    const tab = this.root.querySelector<HTMLElement>("#overview-tab");
    if (!tab) return;
    tab.classList.toggle("loading", loading);
    if (loading) {
      tab.setAttribute("aria-busy", "true");
      return;
    }
    tab.removeAttribute("aria-busy");
  }

  async load(restoring = false) {
    const page = this.deps.page();
    const session = this.deps.session();
    if (!page || !session
      || (this.deps.agentUnavailable() && !restoring)
      || rangesEqual(this.loadingOverview, page.selected_range)) return;
    const key = rangeKey(page.selected_range);
    const instructions = restoring
      ? this.deps.storedOverview()?.instructions?.trim() ?? ""
      : this.overviewInstructions.get(key)?.trim() ?? "";
    if (overviewInstructionError(instructions)) return;
    if (restoring) this.overviewInstructions.set(key, instructions);
    if (this.current(key)?.instructions === instructions) {
      this.render();
      return;
    }

    const range = page.selected_range;
    const request = ++this.overviewRequest;
    this.loadingOverview = range;
    this.editingOverview = false;
    this.setLoading(true);
    this.deps.syncAgentControls();
    const state = this.root.querySelector<HTMLElement>("#overview-state");
    if (state) {
      state.hidden = false;
      state.setAttribute("aria-busy", "true");
      state.innerHTML = `
        <div class="overview-spinner" aria-hidden="true"><span>${icon("sparkles")}</span></div>
        <strong>Preparing the overview</strong>
        <span>Tact is mapping the change and preparing human review guidance.</span>`;
    }

    try {
      const payload = await this.deps.api.overview(session, page, instructions);
      if (request !== this.overviewRequest) return;
      if (payload.generation !== page.generation
        || !rangesEqual(payload.selected_range, range)) {
        this.showError("Tact returned an overview for a different review range.");
        return;
      }
      this.overviews.set(key, { mdx: payload.overview_mdx, instructions, generation: page.generation });
      if (rangesEqual(this.deps.page()?.selected_range, range)) this.render();
    } catch (error) {
      this.deps.recordActionError(error);
      if (request === this.overviewRequest) this.showError(errorMessage(error));
    } finally {
      if (request === this.overviewRequest) {
        this.loadingOverview = undefined;
        this.setLoading(false);
        state?.removeAttribute("aria-busy");
        this.deps.syncAgentControls();
      }
    }
  }

  private showError(message: string) {
    const state = this.root.querySelector<HTMLElement>("#overview-state");
    if (!state) return;
    state.hidden = false;
    state.setAttribute("role", "alert");
    state.setAttribute("aria-live", "assertive");
    state.innerHTML = `
      <div class="overview-error">!</div>
      <strong>Could not prepare the overview</strong>
      <span>${escapeHtml(message)}</span>
      <div class="overview-instructions-actions">
        <button class="button" data-agent-action data-edit-overview-error ${this.deps.agentUnavailable() ? "disabled" : ""}>Edit instructions</button>
        <button class="button primary" data-agent-action data-retry-overview ${this.deps.agentUnavailable() || this.loadingOverview ? "disabled" : ""}>Try again</button>
      </div>`;
    state.querySelector("[data-edit-overview-error]")?.addEventListener("click", () => {
      if (this.deps.agentUnavailable()) return;
      this.editingOverview = true;
      this.render();
      state.querySelector<HTMLTextAreaElement>("[data-overview-instructions]")?.focus();
    });
    state.querySelector("[data-retry-overview]")?.addEventListener("click", () => void this.load());
  }

  renderFrame() {
    const frame = this.root.querySelector<HTMLIFrameElement>(".overview");
    const page = this.deps.page();
    if (!frame || !page) return;
    const mdx = this.overviews.get(rangeKey(page.selected_range))?.mdx;
    if (!mdx) return;
    frame.onload = () => frame.contentWindow?.postMessage(
      { type: "tact-overview", code: overviewProgram(mdx), appearance: this.deps.appearance() }, "*",
    );
    frame.src = "./overview-frame.html";
    frame.hidden = false;
  }
}
