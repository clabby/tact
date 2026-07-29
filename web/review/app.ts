import {
  CodeView,
  parsePatchFiles,
  type CodeViewDiffItem,
  type CodeViewLineSelection,
  type DiffLineAnnotation,
  type FileDiffMetadata,
} from "@pierre/diffs";
import { FileTree, type GitStatus, type GitStatusEntry } from "@pierre/trees";
import { pendingCommentCount } from "./comment-state";
import { commentSelectionCallbacks } from "./comment-selection";
import { renderMarkdown } from "./markdown";
import {
  activeSyntaxTheme,
  appearance,
  diffTheme,
  loadReviewSettings,
  saveReviewSettings,
  type ReviewSettings,
  type SyntaxTheme,
} from "./review-settings";
import { overviewDocument } from "./overview";
import {
  expandRange,
  moveRangeBoundary,
  rangeKey,
  rangeLabel,
  rangesEqual,
  targetLabel,
  type RangeBoundary,
  type ReviewRange,
  type ReviewTarget,
} from "./range-selection";
import "./styles.css";

const COMMENT_ICON_SPRITE = `
  <svg aria-hidden="true" style="display:none" xmlns="http://www.w3.org/2000/svg">
    <symbol id="tact-comment" viewBox="0 0 16 16">
      <path fill="#4b8cff" d="M3.25 2.25h9.5c.97 0 1.75.78 1.75 1.75v6c0 .97-.78 1.75-1.75 1.75H7.1l-3.53 2.06a.55.55 0 0 1-.82-.47V11.7A1.75 1.75 0 0 1 1.5 10V4c0-.97.78-1.75 1.75-1.75Z"/>
    </symbol>
  </svg>`;
const TREE_ICONS = { set: "minimal", spriteSheet: COMMENT_ICON_SPRITE } as const;

type ReviewBootstrap = {
  title: string;
  repository: string;
  trunk: string;
  range_targets: ReviewTarget[];
  default_range: ReviewRange;
};

type ReviewPage = {
  title: string;
  selected_range: ReviewRange;
  patch: string;
  repository: string;
  scope: string;
  base: string;
};

type OverviewResponse = { selected_range: ReviewRange; overview_html: string };
type RefreshResponse = { bootstrap: ReviewBootstrap; page: ReviewPage };

type ReviewComment = {
  id: number;
  path: string;
  side: "additions" | "deletions";
  start_line: number;
  end_line: number;
  body: string;
};

type ReviewDecision = {
  decision: "approve" | "request_changes";
  summary: string;
  comments: Omit<ReviewComment, "id">[];
};

type CommentMetadata = ReviewComment & { itemId: string };

type CommentDraft = {
  itemId: string;
  path: string;
  side: ReviewComment["side"];
  startLine: number;
  endLine: number;
  body: string;
  editingId?: number;
  tab: "comment" | "preview";
};

type AnnotationMetadata =
  | { kind: "comment"; comment: CommentMetadata }
  | { kind: "composer"; draft: CommentDraft };

const root = document.querySelector<HTMLElement>("#app");
if (!root) throw new Error("review root is missing");

void start();

async function start() {
  try {
    const response = await fetch("./api/review");
    if (!response.ok) throw new Error(`review request failed: ${response.status}`);
    const bootstrap = (await response.json()) as ReviewBootstrap;
    const app = new ReviewApp(root, bootstrap);
    app.render();
    await app.selectRange(bootstrap.default_range);
    app.startWorkspacePolling();
  } catch (error) {
    root.innerHTML = `<div class="fatal"><p>Could not load this review.</p><small>${escapeHtml(String(error))}</small></div>`;
  }
}

class ReviewApp {
  private page?: ReviewPage;
  private files: FileDiffMetadata[] = [];
  private items: CodeViewDiffItem<AnnotationMetadata>[] = [];
  private readonly pathToItem = new Map<string, string>();
  private readonly comments: CommentMetadata[] = [];
  private readonly overviews = new Map<string, string>();
  private draft?: CommentDraft;
  private pendingRange?: ReviewRange;
  private previewRange?: ReviewRange;
  private nextCommentId = 1;
  private submitted = false;
  private loadingRange?: ReviewRange;
  private loadingOverview?: ReviewRange;
  private rangeRequest = 0;
  private overviewRequest = 0;
  private statusTimer?: number;
  private checkingStatus = false;
  private refreshing = false;
  private viewer?: CodeView<AnnotationMetadata>;
  private tree?: FileTree;
  private settings = loadReviewSettings(window.localStorage, document.cookie);
  private readonly colorScheme = window.matchMedia("(prefers-color-scheme: dark)");

  constructor(
    private readonly root: HTMLElement,
    private bootstrap: ReviewBootstrap,
  ) {}

  render() {
    document.title = `${this.bootstrap.title} · Tact`;
    this.root.innerHTML = `
      <div class="review-shell">
        <header class="topbar">
          <div class="identity">
            <span class="mark" aria-hidden="true">T</span>
            <div>
              <h1>${escapeHtml(this.bootstrap.title)}</h1>
              <p>${escapeHtml(this.bootstrap.repository)} <span>·</span> <span id="scope-description">Loading uncommitted changes…</span></p>
            </div>
          </div>
          <div class="topbar-actions">
            <button class="range-button" id="range-button" aria-haspopup="dialog" aria-controls="range-dialog" aria-expanded="false">
              <span class="range-button-icon">${icon("git-branch")}</span>
              <span><small>Change range</small><strong id="range-label">Uncommitted changes</strong></span>
              <span class="range-chevron">${icon("chevron-down")}</span>
            </button>
            <div class="change-stats" id="change-stats" aria-label="Change statistics"></div>
            <button class="refresh-notice" id="refresh-notice" hidden>
              <i aria-hidden="true"></i><span>New changes available</span><strong>Refresh</strong>
            </button>
            <button class="icon-button settings-button" id="settings-button" aria-label="Review settings" aria-expanded="false">
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
            </div>
          </div>
        </header>
        <nav class="tabs" role="tablist" aria-label="Review sections">
          <button class="tab active" id="changes-tab" role="tab" aria-selected="true" aria-controls="changes-panel" data-tab="changes">Changes <span id="file-count">0</span></button>
          <button class="tab" id="overview-tab" role="tab" aria-selected="false" aria-controls="overview-panel" tabindex="-1" data-tab="overview">Overview</button>
        </nav>
        <section class="panel overview-panel" id="overview-panel" role="tabpanel" aria-labelledby="overview-tab" data-panel="overview" hidden>
          <div class="overview-state" id="overview-state"></div>
          <iframe class="overview" title="Agent overview" sandbox="" hidden></iframe>
        </section>
        <section class="panel changes-panel active" id="changes-panel" role="tabpanel" aria-labelledby="changes-tab" data-panel="changes">
          <aside class="sidebar">
            <div class="sidebar-label">Changed files</div>
            <div id="file-tree" class="file-tree"></div>
            <div class="comment-index">
              <div class="sidebar-label">Comments <span id="comment-count">0</span></div>
              <div id="comment-list" class="comment-list">
                <p class="empty-comments">Select a line in the diff to comment.</p>
              </div>
            </div>
          </aside>
          <main id="diff-view" class="diff-view"></main>
        </section>
        <footer class="review-bar">
          <textarea id="review-summary" rows="1" placeholder="Leave an overall comment (optional)"></textarea>
          <div class="review-actions">
            <button class="button quiet" id="cancel-review">Cancel</button>
            <button class="button secondary" data-decision="request_changes">Request changes</button>
            <button class="button primary" data-decision="approve">Approve</button>
          </div>
        </footer>
        <div class="scope-state" id="scope-state" hidden></div>
        <dialog class="range-dialog" id="range-dialog" aria-labelledby="range-title" aria-describedby="range-description">
            <header>
              <div>
                <span class="dialog-eyebrow">Review scope</span>
                <h2 id="range-title">Choose a change range</h2>
                <p id="range-description">Click outside the range to widen it. Drag a handle or use an interior action to shrink it.</p>
              </div>
              <button class="icon-button" data-range-close aria-label="Close range selector">${icon("close")}</button>
            </header>
            <div class="range-builder">
              <div class="range-endpoints" aria-label="Selected range endpoints" aria-live="polite">
                <div class="range-endpoint">
                  <small>From</small><strong id="range-from-label"></strong><span id="range-from-title"></span>
                </div>
                <span class="range-direction">${icon("arrow-right")}</span>
                <div class="range-endpoint">
                  <small>To</small><strong id="range-to-label"></strong><span id="range-to-title"></span>
                </div>
              </div>
              <div class="commit-timeline" id="commit-timeline" aria-label="Branch timeline">
                ${this.rangeTimelineMarkup()}
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
        </dialog>
        <div id="finished" class="finished" hidden>
          <div><span>✓</span><h2>Review submitted</h2><p>You can return to Tact.</p></div>
        </div>
      </div>`;

    this.bindEvents();
    this.syncSettingsControls();
    this.applySettings(false);
  }

  startWorkspacePolling() {
    this.statusTimer = window.setInterval(() => void this.checkWorkspaceStatus(), 5_000);
    document.addEventListener("visibilitychange", () => {
      if (!document.hidden) void this.checkWorkspaceStatus();
    });
    window.addEventListener("focus", () => void this.checkWorkspaceStatus());
    void this.checkWorkspaceStatus();
  }

  async selectRange(range: ReviewRange) {
    if (this.loadingRange) return;
    if (rangesEqual(this.page?.selected_range, range)) {
      const state = this.root.querySelector<HTMLElement>("#scope-state");
      if (state?.classList.contains("error")) state.hidden = true;
      return;
    }
    const request = ++this.rangeRequest;
    this.setRangeLoading(range);
    try {
      const response = await fetch("./api/range", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ range }),
      });
      const payload = await response.json() as ReviewPage | { error?: string };
      if (request !== this.rangeRequest) return;
      if (!response.ok) {
        const message = "error" in payload && payload.error
          ? payload.error
          : `Could not load this range (${response.status}).`;
        this.showRangeError(message);
        return;
      }
      this.clearPendingComments();
      this.installPage(payload as ReviewPage);
    } catch (error) {
      if (request === this.rangeRequest) this.showRangeError(String(error));
    } finally {
      if (request === this.rangeRequest) this.setRangeReady();
    }
  }

  private installPage(page: ReviewPage) {
    this.page = page;
    this.files = parsePatchFiles(page.patch, `tact-review-${rangeKey(page.selected_range)}`, true).flatMap(
      (patch) => patch.files,
    );
    this.pathToItem.clear();
    this.items = this.files.map((file, index) => {
      const id = `${rangeKey(page.selected_range)}:${index}:${file.name}`;
      this.pathToItem.set(file.name, id);
      return { id, type: "diff", fileDiff: file, annotations: [], version: 1 };
    });

    const description = this.root.querySelector<HTMLElement>("#scope-description");
    if (description) description.textContent = page.scope;
    this.renderStats();
    this.renderOverviewState();
    this.renderDiff();
    this.renderTree();
    this.renderCommentList();
    this.syncSelectedRange(page.selected_range);
    this.selectTab("changes");
  }

  private renderStats() {
    const stats = changeStats(this.files);
    const container = this.root.querySelector<HTMLElement>("#change-stats");
    const count = this.root.querySelector<HTMLElement>("#file-count");
    if (count) count.textContent = String(this.files.length);
    if (!container) return;
    container.innerHTML = `
      <span>${this.files.length} ${this.files.length === 1 ? "file" : "files"}</span>
      <strong class="add">+${stats.additions}</strong>
      <strong class="del">−${stats.deletions}</strong>`;
  }

  private rangeTimelineMarkup() {
    return this.bootstrap.range_targets.map((target) => `
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
          <button data-boundary-handle="from" aria-label="Drag From boundary; use arrow keys to move">↕ From</button>
          <button data-boundary-handle="to" aria-label="Drag To boundary; use arrow keys to move">↕ To</button>
        </span>
        <span class="boundary-actions">
          <button data-move-boundary="from">Move From</button>
          <button data-move-boundary="to">Move To</button>
        </span>
      </div>`).join("");
  }

  private renderOverviewState() {
    const state = this.root.querySelector<HTMLElement>("#overview-state");
    const frame = this.root.querySelector<HTMLIFrameElement>(".overview");
    if (!state || !frame || !this.page) return;
    if (this.overviews.has(rangeKey(this.page.selected_range))) {
      state.hidden = true;
      this.renderOverview();
      return;
    }
    frame.hidden = true;
    frame.removeAttribute("srcdoc");
    state.hidden = false;
    state.innerHTML = `
      <div class="overview-orbit">${icon("sparkles")}</div>
      <strong>Overview available on request</strong>
      <span>Ask Tact’s root agent to explain and visualize the change when you need it.</span>
      <button type="button" class="button primary" data-generate-overview>Generate overview</button>`;
    state.querySelector("[data-generate-overview]")?.addEventListener("click", () => void this.loadOverview());
  }

  private async loadOverview() {
    const page = this.page;
    if (!page || rangesEqual(this.loadingOverview, page.selected_range)) return;
    const key = rangeKey(page.selected_range);
    if (this.overviews.has(key)) {
      this.renderOverviewState();
      return;
    }

    const range = page.selected_range;
    const request = ++this.overviewRequest;
    this.loadingOverview = range;
    this.syncSelectedRange(range);
    const state = this.root.querySelector<HTMLElement>("#overview-state");
    if (state) {
      state.hidden = false;
      state.setAttribute("aria-busy", "true");
      state.innerHTML = `
        <div class="overview-spinner"><span></span><i></i></div>
        <strong>Preparing the overview</strong>
        <span>Tact’s root agent is reading the exact patch shown in Changes.</span>`;
    }

    try {
      const response = await fetch("./api/overview", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ range }),
      });
      const payload = await response.json() as OverviewResponse | { error?: string };
      if (request !== this.overviewRequest) return;
      if (!response.ok || !("overview_html" in payload) || !rangesEqual(payload.selected_range, range)) {
        const message = "error" in payload && payload.error
          ? payload.error
          : `Could not prepare the overview (${response.status}).`;
        this.showOverviewError(message);
        return;
      }
      this.overviews.set(key, payload.overview_html);
      if (rangesEqual(this.page?.selected_range, range)) this.renderOverviewState();
    } catch (error) {
      if (request === this.overviewRequest) this.showOverviewError(String(error));
    } finally {
      if (request === this.overviewRequest) {
        this.loadingOverview = undefined;
        state?.removeAttribute("aria-busy");
        this.syncSelectedRange(this.page?.selected_range ?? this.bootstrap.default_range);
      }
    }
  }

  private showOverviewError(message: string) {
    const state = this.root.querySelector<HTMLElement>("#overview-state");
    if (!state) return;
    state.hidden = false;
    state.innerHTML = `
      <div class="overview-error">!</div>
      <strong>Could not prepare the overview</strong>
      <span>${escapeHtml(message)}</span>
      <button class="button" data-retry-overview>Try again</button>`;
    state.querySelector("[data-retry-overview]")?.addEventListener("click", () => void this.loadOverview());
  }

  private renderOverview() {
    const frame = this.root.querySelector<HTMLIFrameElement>(".overview");
    if (!frame || !this.page) return;
    const html = this.overviews.get(rangeKey(this.page.selected_range));
    if (!html) return;
    frame.srcdoc = overviewDocument(html, appearance(this.settings));
    frame.hidden = false;
  }

  private renderDiff() {
    const container = this.root.querySelector<HTMLElement>("#diff-view");
    if (!container) return;
    this.viewer?.cleanUp();
    container.replaceChildren();
    this.viewer = new CodeView<AnnotationMetadata>(this.viewerOptions());
    this.viewer.setup(container);
    this.viewer.setItems(this.items);
  }

  private viewerOptions() {
    return {
      diffStyle: this.settings.diffStyle,
      overflow: this.settings.wrapLines ? "wrap" as const : "scroll" as const,
      disableLineNumbers: !this.settings.lineNumbers,
      theme: diffTheme(this.settings),
      themeType: appearance(this.settings),
      enableLineSelection: true,
      stickyHeaders: true,
      lineHoverHighlight: "both" as const,
      ...commentSelectionCallbacks((selection) => this.openCommentComposer(selection)),
      renderAnnotation: (annotation: DiffLineAnnotation<AnnotationMetadata>) => this.annotationElement(annotation),
    };
  }

  private renderTree() {
    const container = this.root.querySelector<HTMLElement>("#file-tree");
    if (!container) return;
    this.tree?.cleanUp();
    container.replaceChildren();
    this.tree = new FileTree({
      paths: this.files.map((file) => file.name),
      flattenEmptyDirectories: true,
      initialExpansion: "open",
      density: "compact",
      icons: TREE_ICONS,
      gitStatus: this.treeGitStatus(),
      renderRowDecoration: ({ item }) => {
        if (item.kind !== "file") return null;
        const count = pendingCommentCount(this.comments, item.path);
        if (count === 0) return null;
        return {
          icon: { name: "tact-comment", width: 14, height: 14, viewBox: "0 0 16 16" },
          title: `${count} pending ${count === 1 ? "comment" : "comments"}`,
        };
      },
      onSelectionChange: (paths) => {
        const path = paths.at(-1);
        const id = path ? this.pathToItem.get(path) : undefined;
        if (id) this.viewer?.scrollTo({ type: "item", id, align: "start", behavior: "smooth-auto" });
      },
    });
    this.tree.render({ containerWrapper: container });
  }

  private treeGitStatus(): GitStatusEntry[] {
    return this.files.map((file) => ({
      path: file.name,
      status: treeStatus(file.type),
    }));
  }

  private bindEvents() {
    const tabs = [...this.root.querySelectorAll<HTMLButtonElement>("[data-tab]")];
    for (const [index, tab] of tabs.entries()) {
      tab.addEventListener("click", () => this.selectTab(tab.dataset.tab ?? "changes"));
      tab.addEventListener("keydown", (event) => {
        if (event.key !== "ArrowLeft" && event.key !== "ArrowRight") return;
        event.preventDefault();
        const offset = event.key === "ArrowRight" ? 1 : -1;
        const target = tabs[(index + offset + tabs.length) % tabs.length];
        target.focus();
        this.selectTab(target.dataset.tab ?? "changes");
      });
    }
    this.root.querySelector("#range-button")?.addEventListener("click", () => this.openRangeDialog());
    this.bindRangeEvents();
    this.root.querySelector("#refresh-notice")?.addEventListener("click", () => void this.refreshReview());
    for (const button of this.root.querySelectorAll<HTMLButtonElement>("[data-range-close]")) {
      button.addEventListener("click", () => this.closeRangeDialog());
    }
    this.root.querySelector("#apply-range")?.addEventListener("click", () => void this.applyRange());
    const rangeDialog = this.root.querySelector<HTMLDialogElement>("#range-dialog");
    rangeDialog?.addEventListener("cancel", (event) => {
      event.preventDefault();
      this.closeRangeDialog();
    });
    rangeDialog?.addEventListener("click", (event) => {
      if (event.target === rangeDialog) this.closeRangeDialog();
    });
    this.root.querySelector("#cancel-review")?.addEventListener("click", () => void this.cancel());
    for (const button of this.root.querySelectorAll<HTMLButtonElement>("[data-decision]")) {
      button.addEventListener("click", () => void this.submit(button.dataset.decision as ReviewDecision["decision"]));
    }
    this.bindSettings();
    this.colorScheme.addEventListener("change", () => {
      if (this.settings.syntaxTheme !== "system") return;
      if (this.draft?.tab === "preview") void this.renderDraftPreview();
    });
    window.addEventListener("pagehide", () => {
      if (this.statusTimer !== undefined) window.clearInterval(this.statusTimer);
      if (!this.submitted) navigator.sendBeacon("./api/cancel");
    });
  }

  private bindRangeEvents() {
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

  private async checkWorkspaceStatus() {
    if (document.hidden || this.checkingStatus || this.refreshing || this.submitted) return;
    this.checkingStatus = true;
    try {
      const response = await fetch("./api/status", { cache: "no-store" });
      if (!response.ok) return;
      const status = await response.json() as { changed: boolean };
      this.setRefreshNotice(status.changed);
    } catch {
      // Polling is advisory; transient failures should not interrupt the review.
    } finally {
      this.checkingStatus = false;
    }
  }

  private setRefreshNotice(visible: boolean, error?: string) {
    const notice = this.root.querySelector<HTMLButtonElement>("#refresh-notice");
    if (!notice) return;
    notice.hidden = !visible;
    notice.classList.toggle("error", error !== undefined);
    notice.title = error ?? "";
    const label = notice.querySelector("span");
    const action = notice.querySelector("strong");
    if (label) label.textContent = error ? "Could not refresh" : "New changes available";
    if (action) action.textContent = error ? "Retry" : "Refresh";
  }

  private async refreshReview() {
    if (this.refreshing || this.loadingRange || this.loadingOverview) return;
    const pendingFeedback = [
      this.comments.length > 0
        ? `${this.comments.length} pending ${this.comments.length === 1 ? "comment" : "comments"}`
        : "",
      this.draft ? "the open comment draft" : "",
    ].filter(Boolean).join(" and ");
    if (pendingFeedback && !window.confirm(
      `Refreshing will discard ${pendingFeedback}. Continue?`,
    )) return;

    const notice = this.root.querySelector<HTMLButtonElement>("#refresh-notice");
    this.refreshing = true;
    if (notice) {
      notice.disabled = true;
      notice.classList.add("loading");
      const action = notice.querySelector("strong");
      if (action) action.textContent = "Refreshing…";
    }
    try {
      const response = await fetch("./api/refresh", { method: "POST" });
      const payload = await response.json() as RefreshResponse | { error?: string };
      if (!response.ok || !("bootstrap" in payload)) {
        const message = "error" in payload && payload.error ? payload.error : "Refresh failed";
        this.setRefreshNotice(true, message);
        return;
      }
      this.bootstrap = payload.bootstrap;
      this.clearPendingComments();
      this.overviews.clear();
      const timeline = this.root.querySelector<HTMLElement>("#commit-timeline");
      if (timeline) {
        timeline.innerHTML = this.rangeTimelineMarkup();
        this.bindRangeEvents();
      }
      this.installPage(payload.page);
      this.setRefreshNotice(false);
    } catch (error) {
      this.setRefreshNotice(true, String(error));
    } finally {
      this.refreshing = false;
      if (notice) {
        notice.disabled = false;
        notice.classList.remove("loading");
      }
    }
  }

  private bindSettings() {
    const button = this.root.querySelector<HTMLButtonElement>("#settings-button");
    const popover = this.root.querySelector<HTMLElement>("#settings-popover");
    button?.addEventListener("click", (event) => {
      event.stopPropagation();
      if (!popover) return;
      popover.hidden = !popover.hidden;
      button.setAttribute("aria-expanded", String(!popover.hidden));
    });
    popover?.addEventListener("click", (event) => event.stopPropagation());
    document.addEventListener("click", () => {
      if (!popover || popover.hidden) return;
      popover.hidden = true;
      button?.setAttribute("aria-expanded", "false");
    });
    for (const control of this.root.querySelectorAll<HTMLInputElement | HTMLSelectElement>("[data-setting]")) {
      control.addEventListener("change", () => {
        this.readSettingsControls();
        saveReviewSettings(window.localStorage, this.settings, (cookie) => {
          document.cookie = cookie;
        });
        this.applySettings(true);
      });
    }
  }

  private syncSettingsControls() {
    const theme = this.root.querySelector<HTMLSelectElement>("[data-setting=syntaxTheme]");
    const layout = this.root.querySelector<HTMLSelectElement>("[data-setting=diffStyle]");
    const wrap = this.root.querySelector<HTMLInputElement>("[data-setting=wrapLines]");
    const lineNumbers = this.root.querySelector<HTMLInputElement>("[data-setting=lineNumbers]");
    if (theme) theme.value = this.settings.syntaxTheme;
    if (layout) layout.value = this.settings.diffStyle;
    if (wrap) wrap.checked = this.settings.wrapLines;
    if (lineNumbers) lineNumbers.checked = this.settings.lineNumbers;
  }

  private readSettingsControls() {
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

  private applySettings(rebuildDiff: boolean) {
    document.documentElement.dataset.appearance = appearance(this.settings);
    if (rebuildDiff && this.page) {
      this.renderOverview();
      this.renderDiff();
    }
    if (this.draft?.tab === "preview") void this.renderDraftPreview();
  }

  private selectTab(name: string) {
    for (const tab of this.root.querySelectorAll<HTMLElement>("[data-tab]")) {
      const selected = tab.dataset.tab === name;
      tab.classList.toggle("active", selected);
      tab.setAttribute("aria-selected", String(selected));
      tab.tabIndex = selected ? 0 : -1;
    }
    for (const panel of this.root.querySelectorAll<HTMLElement>("[data-panel]")) {
      const selected = panel.dataset.panel === name;
      panel.classList.toggle("active", selected);
      panel.hidden = !selected;
    }
    if (name === "changes") this.viewer?.render(true);
  }

  private openRangeDialog() {
    if (this.loadingRange || this.loadingOverview) return;
    this.closeBoundaryActions();
    this.pendingRange = { ...(this.page?.selected_range ?? this.bootstrap.default_range) };
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

  private closeRangeDialog() {
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
    const from = this.bootstrap.range_targets[range.from];
    const to = this.bootstrap.range_targets[range.to];
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
    else if (event.key === "End") target = boundary === "from" ? this.pendingRange.to - 1 : this.bootstrap.range_targets.length - 1;
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
    const currentRange = this.page?.selected_range ?? this.bootstrap.default_range;
    const changesRange = this.pendingRange !== undefined && !rangesEqual(this.pendingRange, currentRange);
    const commentCount = this.comments.length;
    const hasDraft = this.draft !== undefined;
    const hasPendingFeedback = commentCount > 0 || hasDraft;
    if (apply) {
      apply.disabled = !changesRange;
      apply.textContent = changesRange && hasPendingFeedback ? "Discard feedback and apply" : "Apply range";
    }
    if (!warning) return;
    if (!changesRange || !hasPendingFeedback) {
      warning.hidden = true;
      return;
    }
    const comments = commentCount === 0
      ? ""
      : `${commentCount} pending ${commentCount === 1 ? "comment" : "comments"}`;
    const draft = hasDraft ? "an open comment draft" : "";
    warning.textContent = `Switching ranges will discard ${[comments, draft].filter(Boolean).join(" and ")}.`;
    warning.hidden = false;
  }

  private async applyRange() {
    const range = this.pendingRange;
    this.closeRangeDialog();
    if (range) await this.selectRange(range);
  }

  private openCommentComposer(selection: CodeViewLineSelection | null) {
    if (!selection || this.loadingRange) return;
    const side = selection.range.side ?? "additions";
    const endSide = selection.range.endSide ?? side;
    if (side !== endSide) return;
    const item = this.items.find((candidate) => candidate.id === selection.id);
    if (!item) return;

    const previousItemId = this.draft?.itemId;
    this.draft = {
      itemId: item.id,
      path: item.fileDiff.name,
      side,
      startLine: Math.min(selection.range.start, selection.range.end),
      endLine: Math.max(selection.range.start, selection.range.end),
      body: "",
      tab: "comment",
    };
    if (previousItemId && previousItemId !== item.id) this.refreshItem(previousItemId);
    this.refreshItem(item.id);
    queueMicrotask(() => this.focusDraft());
  }

  private editComment(comment: CommentMetadata) {
    const previousItemId = this.draft?.itemId;
    this.draft = {
      itemId: comment.itemId,
      path: comment.path,
      side: comment.side,
      startLine: comment.start_line,
      endLine: comment.end_line,
      body: comment.body,
      editingId: comment.id,
      tab: "comment",
    };
    if (previousItemId && previousItemId !== comment.itemId) this.refreshItem(previousItemId);
    this.refreshItem(comment.itemId);
    this.selectTab("changes");
    this.viewer?.scrollTo({
      type: "range",
      id: comment.itemId,
      range: {
        start: comment.start_line,
        end: comment.end_line,
        side: comment.side,
        endSide: comment.side,
      },
      align: "center",
      behavior: "smooth-auto",
    });
    queueMicrotask(() => this.focusDraft());
  }

  private closeCommentComposer() {
    const itemId = this.draft?.itemId;
    this.draft = undefined;
    this.viewer?.clearSelectedLines();
    if (itemId) this.refreshItem(itemId);
  }

  private saveComment() {
    const draft = this.draft;
    if (!draft) return;
    const body = draft.body.trim();
    if (!body) {
      this.focusDraft();
      return;
    }

    if (draft.editingId !== undefined) {
      const comment = this.comments.find((candidate) => candidate.id === draft.editingId);
      if (comment) comment.body = body;
    } else {
      this.comments.push({
        id: this.nextCommentId++,
        itemId: draft.itemId,
        path: draft.path,
        side: draft.side,
        start_line: draft.startLine,
        end_line: draft.endLine,
        body,
      });
    }
    const itemId = draft.itemId;
    this.draft = undefined;
    this.viewer?.clearSelectedLines();
    this.refreshItem(itemId);
    this.refreshTreeDecorations();
    this.renderCommentList();
  }

  private removeComment(id: number) {
    const index = this.comments.findIndex((comment) => comment.id === id);
    if (index < 0) return;
    const [comment] = this.comments.splice(index, 1);
    if (this.draft?.editingId === id) this.draft = undefined;
    this.refreshItem(comment.itemId);
    this.refreshTreeDecorations();
    this.renderCommentList();
  }

  private clearPendingComments() {
    this.comments.splice(0);
    this.draft = undefined;
    this.viewer?.clearSelectedLines();
  }

  private refreshTreeDecorations() {
    this.tree?.setIcons({ ...TREE_ICONS });
  }

  private refreshItem(itemId: string) {
    const item = this.items.find((candidate) => candidate.id === itemId);
    if (!item) return;
    const annotations: DiffLineAnnotation<AnnotationMetadata>[] = this.comments
      .filter((comment) => comment.itemId === itemId && comment.id !== this.draft?.editingId)
      .map((comment) => ({
        side: comment.side,
        lineNumber: comment.end_line,
        metadata: { kind: "comment", comment },
      }));
    if (this.draft?.itemId === itemId) {
      annotations.push({
        side: this.draft.side,
        lineNumber: this.draft.endLine,
        metadata: { kind: "composer", draft: this.draft },
      });
    }
    item.annotations = annotations;
    item.version = (item.version ?? 0) + 1;
    this.viewer?.updateItem(item);
  }

  private annotationElement(annotation: DiffLineAnnotation<AnnotationMetadata>) {
    if (annotation.metadata.kind === "composer") {
      return this.commentComposerElement(annotation.metadata.draft);
    }
    return this.pendingCommentElement(annotation.metadata.comment);
  }

  private commentComposerElement(draft: CommentDraft) {
    const element = document.createElement("section");
    element.className = "inline-comment-editor";
    element.innerHTML = `
      <div class="editor-topbar">
        <div class="formatting-tools" aria-label="Markdown formatting">
          ${formatButton("bold", "Bold")}
          ${formatButton("italic", "Italic")}
          ${formatButton("code", "Inline code")}
          ${formatButton("code-block", "Code block")}
          ${formatButton("link", "Link")}
          ${formatButton("list", "Bulleted list")}
          ${formatButton("quote", "Quote")}
        </div>
        <div class="editor-tabs" role="tablist">
          <button class="${draft.tab === "comment" ? "active" : ""}" data-editor-tab="comment">Comment</button>
          <button class="${draft.tab === "preview" ? "active" : ""}" data-editor-tab="preview">Preview</button>
        </div>
      </div>
      <div class="editor-context">${escapeHtml(draft.path)}:${formatRange(draft.startLine, draft.endLine)}</div>
      <textarea class="comment-input" rows="5" placeholder="Leave a comment" ${draft.tab === "preview" ? "hidden" : ""}></textarea>
      <div class="markdown-preview" ${draft.tab === "comment" ? "hidden" : ""}></div>
      <div class="composer-actions">
        <button class="text-button" data-comment-action="cancel">Cancel</button>
        <button class="button primary" data-comment-action="save">${draft.editingId === undefined ? "Add comment" : "Save changes"}</button>
      </div>`;

    const textarea = element.querySelector<HTMLTextAreaElement>(".comment-input");
    if (textarea) {
      textarea.value = draft.body;
      textarea.addEventListener("input", () => {
        if (this.draft === draft) draft.body = textarea.value;
      });
      textarea.addEventListener("keydown", (event) => {
        if ((event.metaKey || event.ctrlKey) && event.key === "Enter") this.saveComment();
        if (event.key === "Escape") this.closeCommentComposer();
      });
    }
    for (const button of element.querySelectorAll<HTMLButtonElement>("[data-format]")) {
      button.addEventListener("click", () => {
        if (textarea) applyFormatting(textarea, button.dataset.format ?? "");
        draft.body = textarea?.value ?? draft.body;
      });
    }
    for (const button of element.querySelectorAll<HTMLButtonElement>("[data-editor-tab]")) {
      button.addEventListener("click", () => this.selectEditorTab(element, draft, button.dataset.editorTab as CommentDraft["tab"]));
    }
    element.querySelector("[data-comment-action=cancel]")?.addEventListener("click", () => this.closeCommentComposer());
    element.querySelector("[data-comment-action=save]")?.addEventListener("click", () => this.saveComment());
    if (draft.tab === "preview") void this.renderPreviewElement(element, draft.body);
    return element;
  }

  private pendingCommentElement(comment: CommentMetadata) {
    const element = document.createElement("article");
    element.className = "diff-comment";
    element.innerHTML = `
      <header>
        <span>Lines ${formatRange(comment.start_line, comment.end_line)}</span>
        <div>
          <button class="small-icon-button" data-comment-edit aria-label="Edit comment">${icon("edit")}</button>
          <button class="small-icon-button danger" data-comment-delete aria-label="Delete comment">${icon("trash")}</button>
        </div>
      </header>
      <div class="comment-markdown"></div>`;
    element.querySelector("[data-comment-edit]")?.addEventListener("click", () => this.editComment(comment));
    element.querySelector("[data-comment-delete]")?.addEventListener("click", () => this.removeComment(comment.id));
    const markdown = element.querySelector<HTMLElement>(".comment-markdown");
    if (markdown) void this.renderMarkdown(markdown, comment.body);
    return element;
  }

  private selectEditorTab(element: HTMLElement, draft: CommentDraft, tab: CommentDraft["tab"]) {
    if (this.draft !== draft) return;
    draft.tab = tab;
    for (const button of element.querySelectorAll<HTMLButtonElement>("[data-editor-tab]")) {
      button.classList.toggle("active", button.dataset.editorTab === tab);
    }
    const textarea = element.querySelector<HTMLTextAreaElement>(".comment-input");
    const preview = element.querySelector<HTMLElement>(".markdown-preview");
    if (textarea) textarea.hidden = tab !== "comment";
    if (preview) preview.hidden = tab !== "preview";
    if (tab === "comment") {
      textarea?.focus();
      return;
    }
    if (preview) void this.renderPreviewElement(element, draft.body);
  }

  private async renderPreviewElement(element: HTMLElement, body: string) {
    const preview = element.querySelector<HTMLElement>(".markdown-preview");
    if (preview) await this.renderMarkdown(preview, body);
  }

  private async renderDraftPreview() {
    const editor = this.root.querySelector<HTMLElement>(".inline-comment-editor");
    if (editor && this.draft) await this.renderPreviewElement(editor, this.draft.body);
  }

  private async renderMarkdown(container: HTMLElement, body: string) {
    const theme = activeSyntaxTheme(this.settings, this.colorScheme.matches);
    await renderMarkdown(container, body, theme);
  }

  private focusDraft() {
    const textarea = this.root.querySelector<HTMLTextAreaElement>(".inline-comment-editor .comment-input");
    textarea?.focus();
    textarea?.setSelectionRange(textarea.value.length, textarea.value.length);
  }

  private renderCommentList() {
    const count = this.root.querySelector<HTMLElement>("#comment-count");
    const list = this.root.querySelector<HTMLElement>("#comment-list");
    if (!count || !list) return;
    count.textContent = String(this.comments.length);
    list.replaceChildren();
    if (this.comments.length === 0) {
      const empty = document.createElement("p");
      empty.className = "empty-comments";
      empty.textContent = "Select a line in the diff to comment.";
      list.append(empty);
      return;
    }
    for (const comment of this.comments) {
      const item = document.createElement("div");
      item.className = "comment-link";
      item.innerHTML = `
        <button class="comment-jump">
          <strong>${escapeHtml(comment.path)}</strong>
          <span>${formatRange(comment.start_line, comment.end_line)} · ${comment.side === "additions" ? "new" : "old"}</span>
          <p>${escapeHtml(comment.body)}</p>
        </button>
        <div class="comment-link-actions">
          <button aria-label="Edit comment" data-edit>${icon("edit")}</button>
          <button aria-label="Delete comment" data-delete>${icon("trash")}</button>
        </div>`;
      item.querySelector(".comment-jump")?.addEventListener("click", () => {
        this.selectTab("changes");
        this.viewer?.scrollTo({
          type: "range",
          id: comment.itemId,
          range: { start: comment.start_line, end: comment.end_line, side: comment.side, endSide: comment.side },
          align: "center",
          behavior: "smooth-auto",
        });
      });
      item.querySelector("[data-edit]")?.addEventListener("click", () => this.editComment(comment));
      item.querySelector("[data-delete]")?.addEventListener("click", () => this.removeComment(comment.id));
      list.append(item);
    }
  }

  private setRangeLoading(range: ReviewRange) {
    this.loadingRange = range;
    const state = this.root.querySelector<HTMLElement>("#scope-state");
    if (state) {
      state.className = "scope-state loading";
      state.innerHTML = `<div class="scope-spinner"></div><strong>Loading ${escapeHtml(rangeLabel(this.bootstrap.range_targets, range))}</strong><span>Capturing an immutable diff for this review.</span>`;
      state.hidden = false;
    }
    this.setReviewControlsDisabled(true);
  }

  private setRangeReady() {
    this.loadingRange = undefined;
    this.syncSelectedRange(this.page?.selected_range ?? this.bootstrap.default_range);
    this.setReviewControlsDisabled(false);
    const state = this.root.querySelector<HTMLElement>("#scope-state");
    if (state?.classList.contains("loading")) state.hidden = true;
  }

  private showRangeError(message: string) {
    const state = this.root.querySelector<HTMLElement>("#scope-state");
    if (!state) return;
    state.className = "scope-state error";
    state.innerHTML = `
      <div class="scope-error-icon">!</div>
      <strong>Could not load the selected range</strong>
      <span>${escapeHtml(message)}</span>`;
    state.hidden = false;
  }

  private syncSelectedRange(range: ReviewRange) {
    const label = this.root.querySelector<HTMLElement>("#range-label");
    const button = this.root.querySelector<HTMLButtonElement>("#range-button");
    if (label) label.textContent = rangeLabel(this.bootstrap.range_targets, range);
    if (button) button.disabled = this.loadingRange !== undefined || this.loadingOverview !== undefined;
    const refresh = this.root.querySelector<HTMLButtonElement>("#refresh-notice");
    if (refresh) refresh.disabled = this.loadingRange !== undefined || this.loadingOverview !== undefined || this.refreshing;
  }

  private setReviewControlsDisabled(disabled: boolean) {
    for (const button of this.root.querySelectorAll<HTMLButtonElement>("[data-decision]")) {
      button.disabled = disabled || !this.page;
    }
  }

  private async submit(decision: ReviewDecision["decision"]) {
    if (!this.page || this.loadingRange) return;
    const summary = this.root.querySelector<HTMLTextAreaElement>("#review-summary")?.value.trim() ?? "";
    const payload: ReviewDecision = {
      decision,
      summary,
      comments: this.comments.map(({ path, side, start_line, end_line, body }) => ({
        path,
        side,
        start_line,
        end_line,
        body,
      })),
    };
    const response = await fetch("./api/decision", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(payload),
    });
    if (!response.ok) {
      window.alert(`Could not submit review (${response.status}).`);
      return;
    }
    this.submitted = true;
    const finished = this.root.querySelector<HTMLElement>("#finished");
    if (finished) finished.hidden = false;
  }

  private async cancel() {
    const response = await fetch("./api/cancel", { method: "POST", keepalive: true });
    if (!response.ok) {
      window.alert(`Could not cancel review (${response.status}).`);
      return;
    }
    this.submitted = true;
    const finished = this.root.querySelector<HTMLElement>("#finished");
    if (!finished) return;
    finished.innerHTML = "<div><span>✓</span><h2>Review cancelled</h2><p>You can return to Tact.</p></div>";
    finished.hidden = false;
  }
}

function changeStats(files: FileDiffMetadata[]) {
  return files.reduce(
    (total, file) => {
      for (const hunk of file.hunks) {
        total.additions += hunk.additionLines;
        total.deletions += hunk.deletionLines;
      }
      return total;
    },
    { additions: 0, deletions: 0 },
  );
}

function treeStatus(type: FileDiffMetadata["type"]): GitStatus {
  switch (type) {
    case "new": return "added";
    case "deleted": return "deleted";
    case "rename-pure":
    case "rename-changed": return "renamed";
    case "change": return "modified";
  }
}

function applyFormatting(textarea: HTMLTextAreaElement, format: string) {
  const start = textarea.selectionStart;
  const end = textarea.selectionEnd;
  const selection = textarea.value.slice(start, end);
  const replacements: Record<string, [string, string, string]> = {
    bold: ["**", "**", "bold text"],
    italic: ["_", "_", "italic text"],
    code: ["`", "`", "code"],
    "code-block": ["```\n", "\n```", "code"],
    link: ["[", "](https://)", "link text"],
    quote: ["> ", "", "quote"],
  };
  if (format === "list") {
    const value = selection || "list item";
    const replacement = value.split("\n").map((line) => `- ${line}`).join("\n");
    textarea.setRangeText(replacement, start, end, "select");
    textarea.dispatchEvent(new Event("input", { bubbles: true }));
    textarea.focus();
    return;
  }
  const [before, after, placeholder] = replacements[format] ?? ["", "", ""];
  const value = selection || placeholder;
  textarea.setRangeText(`${before}${value}${after}`, start, end, "end");
  if (!selection) textarea.setSelectionRange(start + before.length, start + before.length + value.length);
  textarea.dispatchEvent(new Event("input", { bubbles: true }));
  textarea.focus();
}

function formatButton(format: string, label: string) {
  return `<button class="format-button" data-format="${format}" aria-label="${label}" title="${label}">${icon(format)}</button>`;
}

function icon(name: string) {
  const paths: Record<string, string> = {
    "git-branch": '<circle cx="6" cy="5" r="2"/><circle cx="18" cy="6" r="2"/><circle cx="6" cy="19" r="2"/><path d="M6 7v10M8 11h4a6 6 0 0 0 6-3"/>',
    "chevron-down": '<path d="m7 10 5 5 5-5"/>',
    close: '<path d="m7 7 10 10M17 7 7 17"/>',
    "arrow-right": '<path d="M5 12h14m-5-5 5 5-5 5"/>',
    sparkles: '<path d="m12 3 1.2 3.3L16.5 7.5l-3.3 1.2L12 12l-1.2-3.3-3.3-1.2 3.3-1.2ZM18 14l.8 2.2L21 17l-2.2.8L18 20l-.8-2.2L15 17l2.2-.8ZM6 13l.7 1.8 1.8.7-1.8.7L6 18l-.7-1.8-1.8-.7 1.8-.7Z"/>',
    settings: '<circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.7 1.7 0 0 0 .34 1.88l.06.06-1.42 1.42-.06-.06a1.7 1.7 0 0 0-1.88-.34 1.7 1.7 0 0 0-1.02 1.56V20h-2v-.48A1.7 1.7 0 0 0 12.4 18a1.7 1.7 0 0 0-1.88.34l-.06.06-1.42-1.42.06-.06A1.7 1.7 0 0 0 9.44 15a1.7 1.7 0 0 0-1.56-1.02H7.4v-2h.48A1.7 1.7 0 0 0 9.44 11a1.7 1.7 0 0 0-.34-1.88l-.06-.06 1.42-1.42.06.06A1.7 1.7 0 0 0 12.4 8a1.7 1.7 0 0 0 1.02-1.56V6h2v.44A1.7 1.7 0 0 0 16.44 8a1.7 1.7 0 0 0 1.88-.34l.06-.06 1.42 1.42-.06.06A1.7 1.7 0 0 0 19.4 11a1.7 1.7 0 0 0 1.56 1.02h.48v2h-.48A1.7 1.7 0 0 0 19.4 15Z" transform="translate(-2.4 -1) scale(1.2)"/>',
    bold: '<path d="M7 5h5a3 3 0 0 1 0 6H7Zm0 6h5.5a3.5 3.5 0 0 1 0 7H7Z"/>',
    italic: '<path d="M10 5h7M7 19h7M14 5 10 19"/>',
    code: '<path d="m8 9-4 3 4 3m8-6 4 3-4 3m-3-8-2 10"/>',
    "code-block": '<path d="M4 5h16v14H4zM8 10l-2 2 2 2m4-4 2 2-2 2"/>',
    link: '<path d="M10 13a5 5 0 0 0 7.5.5l2-2a5 5 0 0 0-7-7l-1.15 1.15M14 11a5 5 0 0 0-7.5-.5l-2 2a5 5 0 0 0 7 7l1.15-1.15"/>',
    list: '<path d="M9 6h11M9 12h11M9 18h11M4 6h.01M4 12h.01M4 18h.01"/>',
    quote: '<path d="M7 17H4a2 2 0 0 1-2-2v-3a5 5 0 0 1 5-5v2a3 3 0 0 0-3 3h3Zm10 0h-3a2 2 0 0 1-2-2v-3a5 5 0 0 1 5-5v2a3 3 0 0 0-3 3h3Z"/>',
    edit: '<path d="M12 20h9M16.5 3.5a2.1 2.1 0 0 1 3 3L8 18l-4 1 1-4Z"/>',
    trash: '<path d="M4 7h16M9 11v6m6-6v6M6 7l1 14h10l1-14M9 7V4h6v3"/>',
  };
  return `<svg viewBox="0 0 24 24" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">${paths[name] ?? ""}</svg>`;
}

function formatRange(start: number, end: number) {
  return start === end ? String(start) : `${Math.min(start, end)}–${Math.max(start, end)}`;
}

function escapeHtml(value: string) {
  return value.replace(/[&<>'"]/g, (character) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", "'": "&#39;", '"': "&quot;",
  })[character] ?? character);
}
