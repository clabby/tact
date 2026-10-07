import {
  CodeView,
  type CodeViewDiffItem,
  type CodeViewLineSelection,
  type DiffLineAnnotation,
  type FileDiffMetadata,
} from "@pierre/diffs";
import { WorkerPoolManager } from "@pierre/diffs/worker";
import {
  FileTree,
  type GitStatus,
  type GitStatusEntry,
} from "@pierre/trees";
import { ProtocolMismatch, ReviewApi, errorCode, errorMessage, type ReviewTransport } from "./review-api";
import { annotationPath, pendingCommentCount } from "./comment-state";
import { commentSelectionCallbacks } from "./comment-selection";
import { changeStats, fileTreeChangeStats } from "./file-tree-stats";
import {
  commentIconMask,
  icon,
  seenIconMask,
  treeIcons,
  type FormattingIconName,
} from "./icons";
import { renderMarkdown } from "./markdown";
import {
  activatePage,
  allQuestions,
  createReviewState,
  currentFeedback,
  currentQuestions,
  discardCurrentFeedback as clearCurrentFeedback,
  feedbackDescription,
  rebaseState,
  replaceQuestions,
  type CommentDraft,
  type CommentMetadata,
  type ReviewState,
} from "./review-state";
import {
  beginFollowUp,
  beginStopping,
  cancelQuestion,
  createQuestionThread,
  failQuestion,
  finishQuestion,
  questionValidationError,
  retryQuestion,
  stopFailed,
  type QuestionThread,
} from "./question-state";
import type {
  ReviewComment,
  ReviewDecision,
  ReviewPage,
  ReviewSession,
  StoredOverview,
} from "./protocol";
import {
  activeSyntaxTheme,
  appearance,
  diffTheme,
  loadReviewSettings,
  saveReviewSettings,
  type ReviewSettings,
  type SyntaxTheme,
} from "./review-settings";
import { overviewProgram } from "./overview";
import { overviewInstructionError } from "./overview-instructions";
import { parseReviewPatch } from "./review-diff";
import { RefreshScheduler } from "./refresh-scheduler";
import { carryFeedback, carryQuestions, changedFileNames } from "./review-live";
import { moveSearchTarget, searchReview, type ReviewSearchMatch } from "./review-search";
import {
  expandRange,
  moveRangeBoundary,
  rangeKey,
  rangeLabel,
  rangePresets,
  rangesEqual,
  targetLabel,
  type RangeBoundary,
  type ReviewRange,
  type ReviewTarget,
} from "./range-selection";
import "./review-panel.css";

const SEARCH_HIGHLIGHT = "tact-web-search-match";
const TREE_STYLES = `
  [data-type="item"] {
    --tact-tree-row-bg: var(--trees-bg);
    position: relative;
  }
  [data-type="item"]:hover {
    --tact-tree-row-bg: var(--trees-bg-muted);
  }
  [data-type="item"][aria-selected="true"] {
    --tact-tree-row-bg: var(--trees-selected-bg);
  }
  [data-item-section="decoration"] {
    position: absolute;
    z-index: 2;
    --tact-comment-icon: url("${commentIconMask}");
    --tact-seen-icon: url("${seenIconMask}");
    --tact-comment-indicator: #4b8cff;
    inset-block: var(--trees-focus-ring-width);
    inset-inline-end: calc(var(--trees-item-padding-x) + var(--trees-git-lane-width));
    align-items: center;
    padding-inline-start: 8px;
    pointer-events: none;
    background-color: var(--tact-tree-row-bg);
    text-align: right;
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
    font-size: 11px;
    font-variant-numeric: tabular-nums;
  }
  [data-item-section="decoration"] span[style*="--tact-comment-indicator"] {
    display: inline-flex;
    align-items: center;
  }
  [data-item-section="decoration"] span[style*="--tact-comment-indicator"]::before {
    width: 12px;
    height: 12px;
    content: "";
    background-color: currentColor;
    -webkit-mask: var(--tact-comment-icon) center / contain no-repeat;
    mask: var(--tact-comment-icon) center / contain no-repeat;
  }
  [data-item-section="decoration"] [title*="Seen"] {
    display: inline-flex;
    align-items: center;
  }
  [data-item-section="decoration"] [title*="Seen"]::after {
    width: 13px;
    height: 13px;
    flex: none;
    margin-inline-start: 8px;
    content: "";
    background-color: var(--trees-accent);
    -webkit-mask: var(--tact-seen-icon) center / contain no-repeat;
    mask: var(--tact-seen-icon) center / contain no-repeat;
  }
  [data-type="item"]:has([data-item-section="decoration"] [title*="Seen"]) [data-item-section="content"] {
    opacity: .56;
    text-decoration: line-through;
  }
`;

type AnnotationMetadata =
  | { kind: "comment"; comment: CommentMetadata }
  | { kind: "question"; thread: QuestionThread }
  | { kind: "composer"; draft: CommentDraft };

type QuestionOperation = { threadId: string; request: number; operationId: string };

/** What the application shell lends the review panel. */
export type ReviewHost = {
  api: ReviewTransport;
  activeSession(): string | null;
  onActiveSessionChange(listener: () => void): () => void;
  /** Fires when the workspace's files may have changed. */
  onWorkspaceChanged(listener: () => void): () => void;
  /** Whether any live session is working, and so possibly editing the workspace. */
  anyRunning(): boolean;
  onRunningChange(listener: () => void): () => void;
  /** Places the composed review into the active session's draft. */
  sendToChat(markdown: string): void | Promise<void>;
  theme(): "light" | "dark";
  onThemeChange(listener: () => void): () => void;
};

/** How long workspace changes are coalesced before the visible diff is refreshed. */
const LIVE_REFRESH_DELAY_MS = 400;

/**
 * Mounts the review panel into `container`, which must have a definite height. The panel keeps the
 * diff current while visible; `setVisible(false)` defers refreshes until it is shown again.
 */
export function mountReviewPanel(container: HTMLElement, host: ReviewHost) {
  const panel = new ReviewPanel(container, new ReviewApi(host.api), host);
  void panel.start();
  return {
    dispose: () => panel.cleanUp(),
    setVisible: (visible: boolean) => panel.setVisible(visible),
  };
}

class ReviewPanel {
  private page?: ReviewPage;
  private files: FileDiffMetadata[] = [];
  private items: CodeViewDiffItem<AnnotationMetadata>[] = [];
  private readonly pathToItem = new Map<string, string>();
  private readonly overviews = new Map<string, { mdx: string; instructions: string; generation: number }>();
  private readonly overviewInstructions = new Map<string, string>();
  private editingOverview = false;
  private pendingRange?: ReviewRange;
  private previewRange?: ReviewRange;
  private nextCommentId = 1;
  private loadingRange?: ReviewRange;
  private loadingOverview?: ReviewRange;
  private rangeRequest = 0;
  private overviewRequest = 0;
  private aiReviewRequest = 0;
  private questionRequest = 0;
  private questionPollTimer?: number;
  private pollingQuestions = false;
  private aiReviewPending = false;
  private sending = false;
  private readonly questionOperations = new Map<string, QuestionOperation>();
  private readonly questionsToPoll = new Set<string>();
  private refreshing = false;
  private readonly refresher = new RefreshScheduler(() => void this.refreshReview(), LIVE_REFRESH_DELAY_MS);
  private visible = true;
  private disposed = false;
  private loaded = false;
  private session: string | null;
  /** Invalidates every response that was requested for an earlier session. */
  private sessionEpoch = 0;
  private bootstrap!: ReviewSession;
  private state!: ReviewState;
  private viewer?: CodeView<AnnotationMetadata>;
  private readonly workerPool: WorkerPoolManager;
  private itemVersion = 0;
  private tree?: FileTree;
  private treeStats = new Map<string, { additions: number; deletions: number }>();
  private searchCount = 0;
  private searchIndex = 0;
  private searchMatch?: ReviewSearchMatch;
  private searchPaused = true;
  private searchSelection?: CodeViewLineSelection | null;
  private searchExpandedItem?: string;
  private searchReturnFocus?: HTMLElement;
  private settings = loadReviewSettings(window.localStorage, document.cookie);
  private readonly unsubscribe: Array<() => void>;
  private readonly closeSettings = () => {
    const popover = this.root.querySelector<HTMLElement>("#settings-popover");
    if (!popover || popover.hidden) return;
    popover.hidden = true;
    this.root.querySelector("#settings-button")?.setAttribute("aria-expanded", "false");
  };
  private readonly handleSearchShortcut = (event: KeyboardEvent) => {
    if (event.isComposing || this.root.querySelector("dialog[open]")) return;
    const key = event.key.toLowerCase();
    if ((event.metaKey || event.ctrlKey) && key === "f") {
      const changes = this.root.querySelector<HTMLElement>("#changes-panel");
      if (!this.visible || !this.root.contains(event.target as Node) || !changes?.matches(".active:not([hidden])")) return;
      event.preventDefault();
      this.openSearch();
      return;
    }
    if ((event.metaKey || event.ctrlKey) && key === "g" && this.searchIsOpen()) {
      event.preventDefault();
      this.moveSearch(event.shiftKey ? -1 : 1);
      return;
    }
    if (event.key === "Escape" && this.searchIsOpen()) {
      event.preventDefault();
      this.closeSearch();
    }
  };

  constructor(
    private readonly root: HTMLElement,
    private readonly api: ReviewApi,
    private readonly host: ReviewHost,
  ) {
    this.session = host.activeSession();
    this.workerPool = new WorkerPoolManager(
      {
        workerFactory: () => new Worker(
          new URL("./worker.js", import.meta.url),
          { type: "module" },
        ),
      },
      { theme: diffTheme(this.settings) },
    );
    this.unsubscribe = [
      host.onWorkspaceChanged(() => this.workspaceChanged()),
      host.onRunningChange(() => this.runningChanged()),
      host.onActiveSessionChange(() => void this.sessionChanged()),
      host.onThemeChange(() => this.themeChanged()),
    ];
    document.addEventListener("click", this.closeSettings);
  }

  private get feedback() { return currentFeedback(this.state); }
  private get comments() { return this.feedback.comments; }
  private get questions() { return currentQuestions(this.state); }
  private get draft() { return this.feedback.draft; }
  private set draft(value: CommentDraft | undefined) { this.feedback.draft = value; }
  private get running() { return this.host.anyRunning(); }
  private get agentBusy() {
    return this.loadingOverview !== undefined || this.aiReviewPending || this.questionOperations.size > 0;
  }
  /** Comments are local to the browser, so only snapshot changes in flight lock them. */
  private get commentsLocked() {
    return this.loadingRange !== undefined || this.refreshing || this.sending;
  }
  /** Overviews, AI review, and questions need an idle chat session to run in. */
  private get agentUnavailable() {
    return this.commentsLocked || this.running || this.session === null;
  }
  private get agentUnavailableReason() {
    if (this.session === null) return "Open a chat to use the agent here.";
    return this.running ? "The agent is working. These actions are available when it finishes." : "";
  }

  async start() {
    this.root.classList.add("review-panel");
    await this.load();
  }

  private async load() {
    const epoch = this.sessionEpoch;
    this.root.innerHTML = '<div class="panel-notice" role="status"><span class="activity-spinner" aria-hidden="true"></span>Loading changes…</div>';
    try {
      const review = await this.api.review();
      if (this.disposed || epoch !== this.sessionEpoch) return;
      this.bootstrap = review;
      this.state = createReviewState(review);
      this.loaded = true;
      this.render();
      this.installInitialPage();
    } catch (error) {
      if (this.disposed || epoch !== this.sessionEpoch) return;
      this.root.innerHTML = '<div class="panel-notice error" role="alert"><strong>Could not load the changes</strong><span></span><button class="button primary" data-retry>Retry</button></div>';
      const message = this.root.querySelector("span");
      if (message) message.textContent = errorMessage(error);
      this.root.querySelector("[data-retry]")?.addEventListener("click", () => void this.load());
    }
  }

  setVisible(visible: boolean) {
    this.visible = visible;
    this.refresher.setVisible(visible);
    if (visible && this.loaded) this.viewer?.render(true);
  }

  installInitialPage() {
    this.restoreStoredOverview(this.bootstrap.overview);
    this.installPage(this.bootstrap.page);
    this.restoreAgentOperation();
  }

  private restoreStoredOverview(overview: StoredOverview | null) {
    if (!overview) return;
    const key = rangeKey(overview.selected_range);
    const instructions = overview.instructions?.trim() ?? "";
    if (!this.overviewInstructions.has(key)) this.overviewInstructions.set(key, instructions);
    if (overview.status === "ready" && overview.overview_mdx?.trim()) {
      this.overviews.set(key, { mdx: overview.overview_mdx, instructions, generation: this.bootstrap.generation });
    }
  }

  private restoreAgentOperation() {
    const overview = this.bootstrap.overview;
    if (overview?.status === "generating"
      && rangesEqual(overview.selected_range, this.page?.selected_range)) {
      void this.loadOverview(true);
    }
    void this.restoreActiveQuestions();
  }

  private async restoreActiveQuestions() {
    const active = allQuestions(this.state).filter((thread) => thread.turn.kind === "asking");
    for (const thread of active) this.resumeQuestionOperation(thread);
    const current = active.find((thread) => rangesEqual(thread.range, this.page?.selected_range));
    if (!current && active[0]) await this.selectRange(active[0].range, false, true);
  }

  render() {
    this.root.innerHTML = `
      <div class="review-shell">
        <header class="topbar">
          <nav class="tabs" role="tablist" aria-label="Review sections">
            <button class="tab active" id="changes-tab" role="tab" aria-selected="true" aria-controls="changes-panel" data-tab="changes">Changes <span id="file-count">0</span></button>
            <button class="tab" id="overview-tab" role="tab" aria-selected="false" aria-controls="overview-panel" tabindex="-1" data-tab="overview">Overview<span class="activity-spinner overview-tab-activity" aria-hidden="true"></span></button>
          </nav>
          <button class="range-button" id="range-button" aria-haspopup="dialog" aria-controls="range-dialog" aria-expanded="false" aria-label="Change range">
            ${icon("git-branch")}
            <strong id="range-label">Full branch</strong>
            <span class="range-chevron">${icon("chevron-down")}</span>
          </button>
          <div class="change-stats" id="change-stats" aria-label="Change statistics"></div>
          <p class="scope-description" id="scope-description" title="${escapeHtml(this.bootstrap.repository)}">Loading changes…</p>
          <div class="topbar-actions">
            <div class="live-status" role="status" aria-live="polite">
              <span class="live-badge" id="live-badge" title="The agent is working; this diff follows its edits." hidden><i aria-hidden="true"></i>Live</span>
              <span class="updating" id="updating" hidden><span class="activity-spinner" aria-hidden="true"></span>Updating</span>
            </div>
            <button class="refresh-notice" id="refresh-notice" hidden>
              <i aria-hidden="true"></i><span>New changes available</span><strong>Refresh</strong>
            </button>
            <div class="layout-toggle" role="group" aria-label="Diff layout">
              <button type="button" data-diff-style="unified">Unified</button>
              <button type="button" data-diff-style="split">Split</button>
            </div>
            <div class="view-toggles" role="group" aria-label="Changes navigation">
              <button class="icon-button" type="button" aria-pressed="false" aria-label="Files" data-mobile-panel="files">${icon("list")}</button>
              <button class="icon-button" type="button" aria-pressed="false" aria-label="Comments" data-mobile-panel="comments">${icon("comment")}<span class="count-badge" id="mobile-comment-count" hidden>0</span></button>
            </div>
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
              <div class="settings-footer">Snapshot <span id="generation"></span></div>
            </div>
          </div>
        </header>
        <section class="panel overview-panel" id="overview-panel" role="tabpanel" aria-labelledby="overview-tab" data-panel="overview" hidden>
          <div class="overview-state" id="overview-state"></div>
          <iframe class="overview" title="Agent overview" sandbox="allow-scripts" hidden></iframe>
        </section>
        <section class="panel changes-panel active" id="changes-panel" role="tabpanel" aria-labelledby="changes-tab" data-panel="changes">
          <div class="review-search" id="review-search" role="search" hidden>
            <label class="sr-only" for="review-search-input">Find in changes</label>
            <input id="review-search-input" type="search" placeholder="Find in changes" autocomplete="off" spellcheck="false" aria-describedby="review-search-count">
            <span id="review-search-count" aria-live="polite">0 of 0</span>
            <button class="small-icon-button search-previous" data-search-previous aria-label="Previous match" title="Previous match" disabled>${icon("chevron-down")}</button>
            <button class="small-icon-button" data-search-next aria-label="Next match" title="Next match" disabled>${icon("chevron-down")}</button>
            <button class="small-icon-button" data-search-close aria-label="Close search" title="Close search">${icon("close")}</button>
          </div>
          <aside class="sidebar">
            <div class="files-navigation" data-mobile-content="files">
              <div class="sidebar-label">Changed files</div>
              <div id="file-tree" class="file-tree"></div>
            </div>
            <div class="comment-index" data-mobile-content="comments">
              <div class="sidebar-label">Comments <span id="comment-count">0</span></div>
              <div id="comment-list" class="comment-list">
                <p class="empty-comments">Select a line in the diff to comment.</p>
              </div>
            </div>
          </aside>
          <main id="diff-view" class="diff-view" data-mobile-content="diff" tabindex="-1"></main>
        </section>
        <footer class="review-bar">
          <label class="sr-only" for="review-summary">Overall review comment</label>
          <textarea id="review-summary" rows="1" placeholder="Leave an overall comment (optional)" aria-describedby="review-status"></textarea>
          <div class="review-actions">
            <button class="button secondary" id="ai-review" data-agent-action>AI review</button>
            <button class="button secondary" data-decision="approve">Approve</button>
            <button class="button primary" data-decision="request_changes">Send to chat</button>
          </div>
        </footer>
        <div class="review-status" id="review-status" role="status" aria-live="polite"></div>
        <div class="sr-only" id="agent-announcement" aria-live="polite"></div>
        <div class="scope-state" id="scope-state" role="status" aria-live="polite" hidden></div>
        <dialog class="range-dialog" id="range-dialog" aria-labelledby="range-title" aria-describedby="range-description">
            <header>
              <div>
                <span class="dialog-eyebrow">Review scope</span>
                <h2 id="range-title">Choose a change range</h2>
                <p id="range-description">Click outside the range to widen it. Drag a handle or use an interior action to shrink it.</p>
              </div>
              <button class="icon-button" data-range-close aria-label="Close range selector">${icon("close")}</button>
            </header>
            <div class="range-presets" id="range-presets" role="group" aria-label="Quick ranges">
              ${rangePresets(this.bootstrap.range_targets).map((preset) => `<button type="button" class="button quiet" data-range-preset="${preset.id}" aria-pressed="false">${preset.label}</button>`).join("")}
            </div>
            <div class="range-builder">
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
      </div>`;

    this.bindEvents();
    this.syncSettingsControls();
    const summary = this.root.querySelector<HTMLTextAreaElement>("#review-summary");
    if (summary) {
      summary.value = this.feedback.summary;
      summary.addEventListener("input", () => {
        this.feedback.summary = summary.value;
        this.syncAgentControls();
      });
    }
  }

  cleanUp() {
    this.disposed = true;
    for (const stop of this.unsubscribe) stop();
    this.refresher.dispose();
    window.clearTimeout(this.questionPollTimer);
    document.removeEventListener("keydown", this.handleSearchShortcut);
    document.removeEventListener("click", this.closeSettings);
    this.viewer?.cleanUp();
    CSS.highlights?.delete(SEARCH_HIGHLIGHT);
    this.tree?.cleanUp();
    this.workerPool.terminate();
    this.root.replaceChildren();
    this.root.classList.remove("review-panel");
  }

  private runningChanged() {
    if (!this.loaded) return;
    if (!this.loadingOverview) this.renderOverviewState();
    for (const item of this.items) {
      if (item.annotations?.length) this.refreshItem(item.id);
    }
    this.renderCommentList();
    this.syncAgentControls();
  }

  private themeChanged() {
    if (this.loaded) this.applySettings(true);
  }

  async selectRange(
    range: ReviewRange,
    discardCurrentFeedback = false,
    restoringActiveQuestion = false,
  ) {
    if (this.loadingRange || this.refreshing || (this.agentBusy && !restoringActiveQuestion)) return;
    if (rangesEqual(this.page?.selected_range, range)) {
      const state = this.root.querySelector<HTMLElement>("#scope-state");
      if (state?.classList.contains("error")) state.hidden = true;
      return;
    }
    const request = ++this.rangeRequest;
    this.setRangeLoading(range);
    try {
      const payload = await this.api.loadRange(this.bootstrap.generation, range);
      if (request !== this.rangeRequest) return;
      if (payload.generation !== this.bootstrap.generation || !rangesEqual(payload.selected_range, range)) {
        this.showRangeError(
          "Tact returned a page for a different review generation or range.",
          range,
          discardCurrentFeedback,
        );
        return;
      }
      if (discardCurrentFeedback) this.state = clearCurrentFeedback(this.state);
      this.installPage(payload as ReviewPage);
    } catch (error) {
      if (request === this.rangeRequest) {
        this.recordActionError(error);
        this.showRangeError(errorMessage(error), range, discardCurrentFeedback);
      }
    } finally {
      if (request === this.rangeRequest) this.setRangeReady();
    }
  }

  private parsePage(page: ReviewPage) {
    const cacheKey = "tact-web-" + page.generation + "-" + rangeKey(page.selected_range);
    return parseReviewPatch(page.patch, cacheKey, page.full_context === true);
  }

  /**
   * Shows `page`. With `preserveView` the reviewer's place survives: files whose diff did not
   * change keep their rendered items (and so their scroll position and expansion), and search,
   * tab, and range selections stay as they are.
   */
  private installPage(page: ReviewPage, files = this.parsePage(page), preserveView = false) {
    if (!preserveView) {
      this.resetSearch();
      this.editingOverview = false;
    }
    this.page = page;
    this.state = activatePage(this.state, page);
    const seenFiles = this.seenFiles();
    // Git's patch owns changed-line identity. Pierre may expand context already present in
    // that immutable patch, but browser-side re-diffing must never replace its hunks.
    const changed = preserveView ? changedFileNames(this.files, files) : undefined;
    const previousItems = new Map(this.items.map((item) => [item.id, item]));
    const previousKeys = new Map(this.items.map((item) => [item.id, annotationKey(item)]));
    this.pathToItem.clear();
    this.items = files.map((file) => {
      this.pathToItem.set(file.name, file.name);
      const previous = previousItems.get(file.name);
      if (previous && changed && !changed.has(file.name)) return previous;
      return {
        id: file.name,
        type: "diff",
        fileDiff: file,
        annotations: [],
        collapsed: seenFiles.has(file.name),
        version: ++this.itemVersion,
      };
    });
    this.files = this.items.map((item) => item.fileDiff);
    this.attachQuestionItems();
    for (const item of this.items) {
      item.annotations = this.annotationsForItem(item.id);
      if (previousItems.get(item.id) === item && previousKeys.get(item.id) !== annotationKey(item)) {
        item.version = ++this.itemVersion;
      }
    }

    const description = this.root.querySelector<HTMLElement>("#scope-description");
    if (description) {
      description.textContent = page.scope;
      // The range button already names a preset scope; the description only adds detail.
      description.hidden = page.scope === rangeLabel(this.bootstrap.range_targets, page.selected_range);
    }
    this.renderStats();
    this.renderOverviewState();
    this.renderDiff(!preserveView);
    this.renderTree();
    this.renderCommentList();
    this.syncSelectedRange(page.selected_range);
    this.syncAgentControls();
    if (!preserveView) {
      const summary = this.root.querySelector<HTMLTextAreaElement>("#review-summary");
      if (summary) summary.value = this.feedback.summary;
      this.selectTab("changes");
    } else if (this.searchIsOpen()) {
      this.refreshSearchCount();
    }
  }

  /** Recounts matches after the files changed underneath an open search, without moving the view. */
  private refreshSearchCount() {
    const query = this.root.querySelector<HTMLInputElement>("#review-search-input")?.value ?? "";
    this.searchCount = searchReview(this.files, query).count;
    this.searchIndex = Math.min(this.searchIndex, Math.max(0, this.searchCount - 1));
    this.searchMatch = undefined;
    this.searchPaused = true;
    this.updateSearchHighlight();
    this.renderSearchStatus();
  }

  private attachQuestionItems() {
    for (const thread of this.questions) {
      const item = this.items.find(
        (candidate) => annotationPath(candidate.fileDiff, thread.side) === thread.path,
      );
      thread.itemId = item?.id ?? "";
    }
  }

  private resumeQuestionOperation(thread: QuestionThread) {
    if (thread.turn.kind !== "asking" || this.questionOperations.has(thread.id)) return;
    this.questionOperations.set(thread.id, {
      threadId: thread.id,
      request: thread.turn.request,
      operationId: thread.turn.operationId,
    });
    this.questionsToPoll.add(thread.id);
    if (thread.itemId) this.refreshItem(thread.itemId);
    this.syncAgentControls();
    this.scheduleQuestionPoll();
  }

  private scheduleQuestionPoll() {
    if (this.questionPollTimer !== undefined) window.clearTimeout(this.questionPollTimer);
    this.questionPollTimer = window.setTimeout(() => {
      this.questionPollTimer = undefined;
      void this.pollQuestions();
    }, 400);
  }

  private async pollQuestions() {
    if (this.pollingQuestions || this.questionsToPoll.size === 0) return;
    const session = this.session;
    if (!session) return;
    const epoch = this.sessionEpoch;
    this.pollingQuestions = true;
    try {
      const payload = await this.api.questions(session, this.bootstrap.generation);
      if (epoch !== this.sessionEpoch) return;
      if (payload.generation !== this.bootstrap.generation) {
        this.questionOperations.clear();
        this.questionsToPoll.clear();
        this.refresher.markStale();
        this.syncAgentControls();
        return;
      }
      for (const threadId of [...this.questionsToPoll]) {
        const operation = this.questionOperations.get(threadId);
        if (!operation) {
          this.questionsToPoll.delete(threadId);
          continue;
        }
        const thread = allQuestions(this.state).find((candidate) => candidate.id === threadId);
        const stored = payload.questions.find((candidate) => candidate.thread_id === threadId);
        if (!thread) continue;
        if (!stored) {
          failQuestion(thread, operation.request, "Tact did not retain this question. Ask again to retry.");
        } else if (stored.operation_id !== operation.operationId) {
          continue;
        } else if (stored.status === "asking") {
          continue;
        } else if (stored.status === "idle") {
          finishQuestion(thread, operation.request, stored.messages.at(-1)?.body ?? "");
          this.announceAgent("Tact answered the question.");
        } else if (stored.status === "cancelled") {
          cancelQuestion(thread, operation.request);
          this.announceAgent("Question cancelled.");
        } else {
          failQuestion(thread, operation.request, stored.error ?? "Unknown error");
          this.announceAgent(`Tact could not answer the question: ${stored.error ?? "Unknown error"}`);
        }
        this.questionOperations.delete(threadId);
        this.questionsToPoll.delete(threadId);
        if (thread.itemId) this.refreshItem(thread.itemId);
      }
      this.syncAgentControls();
    } catch (error) {
      if (errorCode(error) === "stale_snapshot") {
        this.questionOperations.clear();
        this.questionsToPoll.clear();
        this.refresher.markStale();
        this.syncAgentControls();
      }
      // Other polling failures are transient; the stored operation remains authoritative.
    } finally {
      this.pollingQuestions = false;
      if (this.questionsToPoll.size > 0) this.scheduleQuestionPoll();
    }
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
          <button data-boundary-handle="from" aria-label="Drag Base excluded boundary; use arrow keys to move">↕ Base</button>
          <button data-boundary-handle="to" aria-label="Drag Through included boundary; use arrow keys to move">↕ Through</button>
        </span>
        <span class="boundary-actions">
          <button data-move-boundary="from">Move Base</button>
          <button data-move-boundary="to">Move Through</button>
        </span>
      </div>`).join("");
  }

  private renderOverviewState() {
    const state = this.root.querySelector<HTMLElement>("#overview-state");
    const frame = this.root.querySelector<HTMLIFrameElement>(".overview");
    if (!state || !frame || !this.page) return;
    state.removeAttribute("role");
    state.removeAttribute("aria-live");
    const key = rangeKey(this.page.selected_range);
    const ready = this.overviews.has(key);
    if (ready && !this.editingOverview) {
      state.hidden = false;
      state.classList.add("overview-state-ready");
      const outdated = this.currentOverview(key) === undefined;
      state.innerHTML = `${outdated ? '<span class="overview-outdated">Written for an earlier version of these changes.</span>' : ""}<button type="button" class="button" data-agent-action data-edit-overview ${this.agentUnavailable || this.loadingOverview ? "disabled" : ""}>${outdated ? "Regenerate" : "Edit instructions"}</button>`;
      state.querySelector("[data-edit-overview]")?.addEventListener("click", () => {
        if (this.agentUnavailable) return;
        this.editingOverview = true;
        this.renderOverviewState();
        state.querySelector<HTMLTextAreaElement>("[data-overview-instructions]")?.focus();
      });
      this.renderOverview();
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
          <button type="button" class="button primary" data-agent-action data-generate-overview ${this.agentUnavailable || this.loadingOverview || validationError || (ready && this.currentOverview(key)?.instructions === draft.trim()) ? "disabled" : ""}>${ready ? "Regenerate overview" : "Generate overview"}</button>
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
        if (generate) generate.disabled = !!error || this.agentUnavailable || this.loadingOverview !== undefined
          || (ready && this.currentOverview(key)?.instructions === input.value.trim());
      });
    }
    state.querySelector("[data-cancel-overview-edit]")?.addEventListener("click", () => {
      this.editingOverview = false;
      this.renderOverviewState();
    });
    state.querySelector("[data-generate-overview]")?.addEventListener("click", () => void this.loadOverview());
    if (ready) this.renderOverview();
  }

  /** The overview for `key` if it describes the installed snapshot rather than an earlier one. */
  private currentOverview(key: string) {
    const overview = this.overviews.get(key);
    return overview?.generation === this.page?.generation ? overview : undefined;
  }

  private setOverviewLoading(loading: boolean) {
    const tab = this.root.querySelector<HTMLElement>("#overview-tab");
    if (!tab) return;
    tab.classList.toggle("loading", loading);
    if (loading) {
      tab.setAttribute("aria-busy", "true");
      return;
    }
    tab.removeAttribute("aria-busy");
  }

  private async loadOverview(restoring = false) {
    const page = this.page;
    const session = this.session;
    if (!page || !session
      || (this.agentUnavailable && !restoring)
      || rangesEqual(this.loadingOverview, page.selected_range)) return;
    const key = rangeKey(page.selected_range);
    const instructions = restoring
      ? this.bootstrap.overview?.instructions?.trim() ?? ""
      : this.overviewInstructions.get(key)?.trim() ?? "";
    if (overviewInstructionError(instructions)) return;
    if (restoring) this.overviewInstructions.set(key, instructions);
    if (this.currentOverview(key)?.instructions === instructions) {
      this.renderOverviewState();
      return;
    }

    const range = page.selected_range;
    const request = ++this.overviewRequest;
    this.loadingOverview = range;
    this.editingOverview = false;
    this.setOverviewLoading(true);
    this.syncAgentControls();
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
      const payload = await this.api.overview(session, page, instructions);
      if (request !== this.overviewRequest) return;
      if (payload.generation !== page.generation
        || !rangesEqual(payload.selected_range, range)) {
        this.showOverviewError("Tact returned an overview for a different review range.");
        return;
      }
      this.overviews.set(key, { mdx: payload.overview_mdx, instructions, generation: page.generation });
      if (rangesEqual(this.page?.selected_range, range)) this.renderOverviewState();
    } catch (error) {
      this.recordActionError(error);
      if (request === this.overviewRequest) this.showOverviewError(errorMessage(error));
    } finally {
      if (request === this.overviewRequest) {
        this.loadingOverview = undefined;
        this.setOverviewLoading(false);
        state?.removeAttribute("aria-busy");
        this.syncAgentControls();
      }
    }
  }

  private showOverviewError(message: string) {
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
        <button class="button" data-agent-action data-edit-overview-error ${this.agentUnavailable ? "disabled" : ""}>Edit instructions</button>
        <button class="button primary" data-agent-action data-retry-overview ${this.agentUnavailable || this.loadingOverview ? "disabled" : ""}>Try again</button>
      </div>`;
    state.querySelector("[data-edit-overview-error]")?.addEventListener("click", () => {
      if (this.agentUnavailable) return;
      this.editingOverview = true;
      this.renderOverviewState();
      state.querySelector<HTMLTextAreaElement>("[data-overview-instructions]")?.focus();
    });
    state.querySelector("[data-retry-overview]")?.addEventListener("click", () => void this.loadOverview());
  }

  private renderOverview() {
    const frame = this.root.querySelector<HTMLIFrameElement>(".overview");
    if (!frame || !this.page) return;
    const mdx = this.overviews.get(rangeKey(this.page.selected_range))?.mdx;
    if (!mdx) return;
    frame.onload = () => frame.contentWindow?.postMessage(
      { type: "tact-overview", code: overviewProgram(mdx), appearance: appearance(this.settings, this.host.theme()) }, "*",
    );
    frame.src = "./overview-frame.html";
    frame.hidden = false;
  }

  private async runAiReview() {
    const page = this.page;
    const session = this.session;
    if (!page || !session || this.aiReviewPending || this.agentUnavailable || this.refresher.stale) return;
    const request = ++this.aiReviewRequest;
    this.aiReviewPending = true;
    const button = this.root.querySelector<HTMLButtonElement>("#ai-review");
    if (button) {
      button.innerHTML = '<span class="activity-spinner" aria-hidden="true"></span>Reviewing…';
      button.setAttribute("aria-busy", "true");
    }
    this.announceAgent("Tact is reviewing the selected diff.");
    this.clearInlineError();
    this.syncAgentControls();
    try {
      const result = await this.api.aiReview(session, page);
      if (request !== this.aiReviewRequest) return;
      if (result.generation !== page.generation
        || !rangesEqual(result.selected_range, page.selected_range)
        || this.page !== page) throw new Error("Tact returned findings for a different review range.");
      let added = 0;
      for (const finding of result.comments) {
        const item = this.items.find((candidate) =>
          annotationPath(candidate.fileDiff, finding.side) === finding.path);
        if (!item || !finding.body.trim()
          || this.comments.some((comment) => comment.path === finding.path
            && comment.side === finding.side && comment.start_line === finding.start_line
            && comment.end_line === finding.end_line && comment.body === finding.body)) continue;
        this.comments.push({ ...finding, id: this.nextCommentId++, itemId: item.id });
        this.refreshItem(item.id);
        added++;
      }
      this.refreshTreeDecorations();
      this.renderCommentList();
      this.selectTab("changes");
      this.announceAgent(added ? `Tact added ${added} inline review comments.` : "Tact found no actionable issues.");
      this.showNotice(added ? `Added ${added} AI review comments. Review and edit them before sending.` : "AI review found no actionable issues.");
    } catch (error) {
      this.recordActionError(error);
      this.showInlineError(errorMessage(error), this.refresher.stale ? undefined : () => void this.runAiReview());
    } finally {
      if (request === this.aiReviewRequest) {
        this.aiReviewPending = false;
        if (button) {
          button.textContent = "AI review";
          button.removeAttribute("aria-busy");
        }
        this.syncAgentControls();
      }
    }
  }

  private renderDiff(resetScroll = false) {
    const container = this.root.querySelector<HTMLElement>("#diff-view");
    if (!container) return;
    if (this.files.length === 0) {
      this.viewer?.cleanUp();
      this.viewer = undefined;
      container.replaceChildren();
      container.innerHTML = `
        <div class="empty-range">
          <strong>No changes in this range</strong>
          <span>Choose another range or refresh after changing the workspace.</span>
          <div><button class="button" data-empty-range-change>Change range</button><button class="button" data-empty-range-refresh>Refresh</button></div>
        </div>`;
      container.querySelector("[data-empty-range-change]")?.addEventListener("click", () => this.openRangeDialog());
      container.querySelector("[data-empty-range-refresh]")?.addEventListener("click", () => void this.refreshReview());
      return;
    }

    if (!this.viewer) {
      container.replaceChildren();
      this.viewer = new CodeView<AnnotationMetadata>(this.viewerOptions(), this.workerPool);
      this.viewer.setup(container);
    } else {
      this.viewer.setOptions(this.viewerOptions());
    }
    this.viewer.setItems(this.items);
    if (resetScroll) {
      this.viewer.scrollTo({ type: "position", position: 0, behavior: "instant" });
    }
  }

  private viewerOptions() {
    return {
      diffStyle: this.settings.diffStyle,
      overflow: this.settings.wrapLines ? "wrap" as const : "scroll" as const,
      disableLineNumbers: !this.settings.lineNumbers,
      theme: diffTheme(this.settings),
      themeType: appearance(this.settings, this.host.theme()),
      unsafeCSS: `::highlight(${SEARCH_HIGHLIGHT}) { color: #171717; background-color: #ffd54f; }`,
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
      onSelectedLinesChange: (selection) => {
        if (this.searchIsOpen()) this.searchSelection = selection;
      },
      onPostRender: (node, _instance, phase, context) => {
        if (context.item.id === this.searchMatch?.itemId) {
          this.updateSearchHighlight(phase === "unmount" ? null : node.shadowRoot);
        }
      },
      ...commentSelectionCallbacks((selection) => this.openCommentComposer(selection)),
      renderAnnotation: (annotation: DiffLineAnnotation<AnnotationMetadata>) => this.annotationElement(annotation),
    };
  }

  private renderTree() {
    const container = this.root.querySelector<HTMLElement>("#file-tree");
    if (!container) return;
    this.treeStats = fileTreeChangeStats(this.files);
    if (this.tree) {
      this.tree.resetPaths(this.files.map((file) => file.name));
      this.tree.setGitStatus(this.treeGitStatus());
      this.refreshTreeDecorations();
      return;
    }
    this.tree = new FileTree({
      paths: this.files.map((file) => file.name),
      flattenEmptyDirectories: true,
      initialExpansion: "open",
      density: "compact",
      icons: treeIcons,
      unsafeCSS: TREE_STYLES,
      gitStatus: this.treeGitStatus(),
      renderRowDecoration: ({ item }) => {
        const stats = this.treeStats.get(item.path);
        if (!stats) return null;

        const count = item.kind === "file"
          ? pendingCommentCount(this.comments, item.path)
          : 0;
        const seen = item.kind === "file" && this.seenFiles().has(item.path);
        const title = [`+${stats.additions} / -${stats.deletions}`];
        const parts: Array<{ text: string; color?: string }> = [
          { text: `+${stats.additions}`, color: "var(--trees-status-added)" },
          { text: "\u00a0/\u00a0", color: "var(--trees-fg-muted)" },
          { text: `-${stats.deletions}`, color: "var(--trees-status-deleted)" },
        ];
        if (count > 0) {
          title.push(`${count} pending ${count === 1 ? "comment" : "comments"}`);
          parts.push(
            { text: "\u00a0\u00a0" },
            { text: `\u00a0${count}`, color: "var(--tact-comment-indicator)" },
          );
        }
        if (seen) {
          title.push("Seen");
        }
        return {
          text: `+${stats.additions} / -${stats.deletions}`,
          parts,
          title: title.join(" · "),
        };
      },
      onSelectionChange: (paths) => {
        const path = paths.at(-1);
        const id = path ? this.pathToItem.get(path) : undefined;
        if (id) this.viewer?.scrollTo({ type: "item", id, align: "start", behavior: "smooth-auto" });
      },
    });
    this.tree.render({ containerWrapper: container });
    this.syncTreeAppearance();
  }

  private syncTreeAppearance() {
    const container = this.tree?.getFileTreeContainer();
    if (!container) return;
    container.style.colorScheme = appearance(this.settings, this.host.theme());
  }

  private treeGitStatus(): GitStatusEntry[] {
    return this.files.map((file) => ({
      path: file.name,
      status: treeStatus(file.type),
    }));
  }

  private seenFiles() {
    return this.feedback.seenPaths;
  }

  private seenButton(item: CodeViewDiffItem<AnnotationMetadata>) {
    const seen = this.seenFiles().has(item.fileDiff.name);
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

  private toggleSeen(item: CodeViewDiffItem<AnnotationMetadata>) {
    const files = this.seenFiles();
    const path = item.fileDiff.name;
    const seen = !files.has(path);
    if (seen) files.add(path);
    else files.delete(path);

    item.collapsed = seen;
    item.version = ++this.itemVersion;
    this.viewer?.updateItem(item);
    if (!seen && this.searchExpandedItem === path) this.searchExpandedItem = undefined;
    this.refreshTreeDecorations();
  }

  private bindSearch() {
    document.addEventListener("keydown", this.handleSearchShortcut);
    const input = this.root.querySelector<HTMLInputElement>("#review-search-input");
    input?.addEventListener("input", () => this.updateSearch());
    input?.addEventListener("keydown", (event) => {
      if (event.key !== "Enter" || event.isComposing) return;
      event.preventDefault();
      this.moveSearch(event.shiftKey ? -1 : 1);
    });
    this.root.querySelector("[data-search-previous]")?.addEventListener("click", () => this.moveSearch(-1));
    this.root.querySelector("[data-search-next]")?.addEventListener("click", () => this.moveSearch(1));
    this.root.querySelector("[data-search-close]")?.addEventListener("click", () => this.closeSearch());
  }

  private searchIsOpen() {
    return this.root.querySelector<HTMLElement>("#review-search")?.hidden === false;
  }

  private openSearch() {
    const panel = this.root.querySelector<HTMLElement>("#review-search");
    const input = this.root.querySelector<HTMLInputElement>("#review-search-input");
    if (!panel || !input) return;
    if (panel.hidden) {
      this.searchReturnFocus = deepActiveElement(document);
      this.searchSelection = this.viewer?.getSelectedLines() ?? null;
      panel.hidden = false;
    }
    input.focus();
    input.select();
  }

  private closeSearch() {
    const panel = this.root.querySelector<HTMLElement>("#review-search");
    if (!panel || panel.hidden) return;
    const restoreFocus = panel.contains(document.activeElement);
    panel.hidden = true;
    this.searchPaused = true;
    this.updateSearchHighlight();
    this.restoreSearchExpandedItem();
    this.restoreSearchSelection();
    this.searchSelection = undefined;
    if (restoreFocus) {
      const target = this.searchReturnFocus?.isConnected
        ? this.searchReturnFocus
        : this.root.querySelector<HTMLElement>("#diff-view");
      queueMicrotask(() => target?.focus());
    }
    this.searchReturnFocus = undefined;
  }

  private resetSearch() {
    this.closeSearch();
    const input = this.root.querySelector<HTMLInputElement>("#review-search-input");
    if (input) input.value = "";
    this.searchCount = 0;
    this.searchIndex = 0;
    this.searchMatch = undefined;
    this.searchPaused = true;
    this.renderSearchStatus();
  }

  private updateSearch() {
    const query = this.root.querySelector<HTMLInputElement>("#review-search-input")?.value ?? "";
    this.restoreSearchExpandedItem();
    this.restoreSearchSelection();
    const result = searchReview(this.files, query);
    this.searchCount = result.count;
    this.searchIndex = 0;
    this.searchMatch = result.match;
    this.searchPaused = false;
    this.renderSearchStatus();
    if (result.match) this.revealSearchMatch();
    else this.updateSearchHighlight();
  }

  private moveSearch(direction: -1 | 1) {
    if (this.searchCount === 0) return;
    const [index, occurrence] = moveSearchTarget(
      this.searchMatch,
      this.searchIndex,
      this.searchCount,
      direction,
    );
    this.searchIndex = index;
    const query = this.root.querySelector<HTMLInputElement>("#review-search-input")?.value ?? "";
    this.searchMatch = searchReview(this.files, query, index, occurrence).match;
    this.searchPaused = false;
    this.renderSearchStatus();
    this.revealSearchMatch();
  }

  private revealSearchMatch() {
    const match = this.searchMatch;
    if (!match) return;
    this.updateSearchHighlight();
    this.selectMobilePanel("diff");
    if (match.kind === "path") {
      this.restoreSearchExpandedItem();
      this.restoreSearchSelection();
      this.viewer?.scrollTo({
        type: "item",
        id: match.itemId,
        align: "start",
        behavior: "smooth-auto",
      });
      return;
    }

    this.restoreSearchExpandedItem(match.itemId);
    const item = this.items.find((candidate) => candidate.id === match.itemId);
    if (item?.collapsed) {
      item.collapsed = false;
      item.version = ++this.itemVersion;
      this.viewer?.updateItem(item);
      if (this.seenFiles().has(item.id)) this.searchExpandedItem = item.id;
    }
    this.viewer?.setSelectedLines({
      id: match.itemId,
      range: {
        start: match.lineNumber,
        end: match.lineNumber,
        side: match.side,
        endSide: match.side,
      },
    }, { notify: false });
    this.viewer?.scrollTo({
      type: "line",
      id: match.itemId,
      lineNumber: match.lineNumber,
      side: match.side,
      align: "center",
      behavior: "smooth-auto",
    });
  }

  private updateSearchHighlight(
    root: ShadowRoot | null | undefined = this.viewer?.getRenderedItems()
      .find((candidate) => candidate.id === this.searchMatch?.itemId)?.element.shadowRoot,
  ) {
    CSS.highlights?.delete(SEARCH_HIGHLIGHT);
    const match = this.searchMatch;
    if (!CSS.highlights || this.searchPaused || !this.searchIsOpen() || match?.kind !== "content") return;
    const split = root?.querySelector(`[data-${match.side}]`);
    const lineType = match.side === "additions" ? "change-addition" : "change-deletion";
    const line = split?.querySelector<HTMLElement>(`[data-line="${match.lineNumber}"]`)
      ?? root?.querySelector<HTMLElement>(
        `[data-unified] [data-line="${match.lineNumber}"]:is([data-line-type="${lineType}"], [data-line-type="context"])`,
      );
    const range = line ? textRange(line, match.start, match.length) : undefined;
    if (range) CSS.highlights.set(SEARCH_HIGHLIGHT, new Highlight(range));
  }

  private restoreSearchSelection() {
    if (this.searchSelection === undefined) return;
    this.viewer?.setSelectedLines(this.searchSelection, { notify: false });
  }

  private clearSelectedLines() {
    if (this.searchIsOpen()) this.searchSelection = null;
    this.viewer?.clearSelectedLines();
  }

  private restoreSearchExpandedItem(keepItem?: string) {
    const itemId = this.searchExpandedItem;
    if (!itemId || itemId === keepItem) return;
    this.searchExpandedItem = undefined;
    const item = this.items.find((candidate) => candidate.id === itemId);
    if (!item || !this.seenFiles().has(itemId) || item.collapsed) return;
    item.collapsed = true;
    item.version = ++this.itemVersion;
    this.viewer?.updateItem(item);
  }

  private renderSearchStatus() {
    const count = this.root.querySelector<HTMLElement>("#review-search-count");
    const previous = this.root.querySelector<HTMLButtonElement>("[data-search-previous]");
    const next = this.root.querySelector<HTMLButtonElement>("[data-search-next]");
    const hasMatches = this.searchCount > 0;
    if (previous) previous.disabled = !hasMatches;
    if (next) next.disabled = !hasMatches;
    if (!count) return;
    const occurrence = this.searchMatch?.kind === "content" && this.searchMatch.occurrenceCount > 1
      ? ` · ${this.searchMatch.occurrenceIndex + 1} of ${this.searchMatch.occurrenceCount} on line`
      : "";
    count.textContent = hasMatches
      ? `${this.searchIndex + 1} of ${this.searchCount}${occurrence}`
      : "0 of 0";
  }

  private bindEvents() {
    this.bindSearch();
    const tabs = [...this.root.querySelectorAll<HTMLButtonElement>("[data-tab]")];
    for (const [index, tab] of tabs.entries()) {
      tab.addEventListener("click", () => this.selectTab(tab.dataset.tab ?? "changes"));
      tab.addEventListener("keydown", (event) => {
        if (!["ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key)) return;
        event.preventDefault();
        const targetIndex = event.key === "Home" ? 0
          : event.key === "End" ? tabs.length - 1
          : (index + (event.key === "ArrowRight" ? 1 : -1) + tabs.length) % tabs.length;
        const target = tabs[targetIndex];
        target.focus();
        this.selectTab(target.dataset.tab ?? "changes");
      });
    }
    this.bindMobileNavigation();
    this.root.querySelector("#range-button")?.addEventListener("click", () => this.openRangeDialog());
    for (const button of this.root.querySelectorAll<HTMLButtonElement>("[data-range-preset]")) {
      button.addEventListener("click", () => this.previewPreset(button.dataset.rangePreset));
    }
    this.bindRangeEvents();
    this.root.querySelector("#refresh-notice")?.addEventListener("click", () => this.refresher.retry());
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
    this.root.querySelector("#ai-review")?.addEventListener("click", () => void this.runAiReview());
    for (const button of this.root.querySelectorAll<HTMLButtonElement>("[data-decision]")) {
      button.addEventListener("click", () => void this.sendToChat(button.dataset.decision as ReviewDecision["decision"]));
    }
    this.bindSettings();
  }

  /** On narrow screens the file list and the comment list replace the diff; choosing one again returns to it. */
  private selectMobilePanel(name: "diff" | "files" | "comments") {
    for (const button of this.root.querySelectorAll<HTMLButtonElement>("[data-mobile-panel]")) {
      button.setAttribute("aria-pressed", String(button.dataset.mobilePanel === name));
    }
    const panel = this.root.querySelector("#changes-panel");
    if (panel?.getAttribute("data-mobile-active") === name) return;
    panel?.setAttribute("data-mobile-active", name);
    if (name === "diff") this.viewer?.render(true);
  }

  private bindMobileNavigation() {
    const panel = this.root.querySelector("#changes-panel");
    for (const button of this.root.querySelectorAll<HTMLButtonElement>("[data-mobile-panel]")) {
      button.addEventListener("click", () => {
        const name = button.dataset.mobilePanel as "files" | "comments";
        this.selectMobilePanel(panel?.getAttribute("data-mobile-active") === name ? "diff" : name);
      });
    }
    this.selectMobilePanel("diff");
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

  private workspaceChanged() {
    this.refresher.markStale();
  }

  private async sessionChanged() {
    const session = this.host.activeSession();
    if (session === this.session) return;
    this.session = session;
    const epoch = ++this.sessionEpoch;
    // Responses requested for the previous session are ignored when they arrive.
    this.overviewRequest++;
    this.aiReviewRequest++;
    this.questionRequest++;
    this.loadingOverview = undefined;
    this.aiReviewPending = false;
    this.questionOperations.clear();
    this.questionsToPoll.clear();
    window.clearTimeout(this.questionPollTimer);
    this.questionPollTimer = undefined;
    this.overviews.clear();
    this.overviewInstructions.clear();
    this.editingOverview = false;
    if (!this.loaded) {
      void this.load();
      return;
    }
    this.setOverviewLoading(false);
    this.syncAgentControls();
    try {
      const review = await this.api.review();
      if (epoch === this.sessionEpoch && !this.disposed) await this.installSnapshot(review, epoch);
    } catch (error) {
      if (epoch === this.sessionEpoch) this.showInlineError(errorMessage(error));
    }
  }

  private async refreshReview() {
    if (this.refreshing || !this.loaded || this.loadingRange || this.agentBusy || this.sending) return;
    const epoch = this.sessionEpoch;
    this.refresher.start();
    this.refreshing = true;
    this.syncAgentControls();
    try {
      let review: ReviewSession;
      try {
        review = await this.api.refresh(this.bootstrap.generation);
      } catch (error) {
        if (errorCode(error) !== "stale_snapshot") throw error;
        // Another window already moved the workspace to a newer generation.
        review = await this.api.review();
      }
      if (epoch === this.sessionEpoch && !this.disposed) await this.installSnapshot(review, epoch);
    } catch (error) {
      this.refresher.fail(errorMessage(error));
    } finally {
      this.refreshing = false;
      this.syncAgentControls();
    }
  }

  /**
   * Installs a newer snapshot, or another session's overview and questions. The reviewer's
   * comments, drafts, seen marks, range, and scroll position follow the diff where it allows.
   */
  private async installSnapshot(review: ReviewSession, epoch: number) {
    const previous = this.state;
    const previousRange = this.page?.selected_range ?? review.default_range;
    const sameTimeline = JSON.stringify(review.range_targets) === JSON.stringify(this.bootstrap.range_targets);
    let page = review.page;
    if (sameTimeline && !rangesEqual(previousRange, page.selected_range)) {
      page = await this.api.loadRange(review.generation, previousRange).catch(() => review.page);
      if (epoch !== this.sessionEpoch || this.disposed) return;
    }
    const sameRange = rangesEqual(page.selected_range, previousRange);
    const files = this.parsePage(page);
    if (sameRange && review.generation === previous.session.generation) {
      this.state = replaceQuestions({ ...previous, page }, review);
    } else if (sameRange) {
      this.state = rebaseState(
        review,
        page,
        carryFeedback(this.files, files, currentFeedback(previous), () => this.nextCommentId++),
        carryQuestions(this.files, files, currentQuestions(previous)),
      );
    } else {
      this.state = createReviewState(review);
    }
    this.bootstrap = review;
    this.restoreStoredOverview(review.overview);
    if (!sameTimeline) {
      const timeline = this.root.querySelector<HTMLElement>("#commit-timeline");
      if (timeline) {
        timeline.innerHTML = this.rangeTimelineMarkup();
        this.bindRangeEvents();
      }
    }
    this.installPage(this.state.page, files, sameRange);
    this.restoreAgentOperation();
  }

  private syncRefreshState() {
    const notice = this.root.querySelector<HTMLButtonElement>("#refresh-notice");
    if (notice) {
      notice.hidden = !this.refresher.stale || this.refreshing || this.refresher.waiting;
      notice.classList.toggle("error", this.refresher.failed !== undefined);
      notice.title = this.refresher.failed ?? "";
      const label = notice.querySelector("span");
      const action = notice.querySelector("strong");
      if (label) label.textContent = this.refresher.failed ? "Could not refresh" : "New changes available";
      if (action) action.textContent = this.refresher.failed ? "Retry" : "Refresh";
    }
    const updating = this.root.querySelector<HTMLElement>("#updating");
    if (updating) updating.hidden = !this.refreshing;
    const live = this.root.querySelector<HTMLElement>("#live-badge");
    if (live) live.hidden = !this.running;
    const generation = this.root.querySelector<HTMLElement>("#generation");
    if (generation) generation.textContent = "#" + this.bootstrap.generation;
  }

  private recordActionError(error: unknown) {
    if (["stale_snapshot", "workspace_changed"].includes(errorCode(error) ?? "")) this.refresher.markStale();
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
        this.readSettingsControls();
        this.syncSettingsControls();
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
    for (const button of this.root.querySelectorAll<HTMLElement>("[data-diff-style]")) {
      button.setAttribute("aria-pressed", String(button.dataset.diffStyle === this.settings.diffStyle));
    }
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
    this.syncTreeAppearance();
    if (rebuildDiff && this.page) {
      void this.workerPool
        .setRenderOptions({ theme: diffTheme(this.settings) })
        .catch((error) => console.warn("Could not update the diff worker theme.", error));
      this.renderOverview();
      this.renderDiff();
    }
    if (this.draft?.tab === "preview") void this.renderDraftPreview();
  }

  private selectTab(name: string) {
    if (name !== "changes") this.closeSearch();
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
    if (this.loadingRange || this.agentBusy) return;
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
    for (const button of this.root.querySelectorAll<HTMLButtonElement>("[data-range-preset]")) {
      const preset = rangePresets(this.bootstrap.range_targets).find((candidate) => candidate.id === button.dataset.rangePreset);
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
    const pendingFeedback = feedbackDescription(this.feedback);
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
    const preset = rangePresets(this.bootstrap.range_targets).find((candidate) => candidate.id === id);
    if (!preset || !this.pendingRange) return;
    this.pendingRange = { ...preset.range };
    this.previewRange = undefined;
    this.closeBoundaryActions();
    this.syncRangeSelector();
  }

  private async applyRange() {
    const range = this.pendingRange;
    const discardFeedback = feedbackDescription(this.feedback).length > 0;
    this.closeRangeDialog();
    if (range) await this.selectRange(range, discardFeedback);
  }

  private openCommentComposer(selection: CodeViewLineSelection | null) {
    if (!selection || this.commentsLocked) return;
    const side = selection.range.side ?? "additions";
    const endSide = selection.range.endSide ?? side;
    if (side !== endSide) return;
    const item = this.items.find((candidate) => candidate.id === selection.id);
    if (!item) return;
    if (this.draft) {
      this.showInlineError("Save or discard the open comment draft before starting another comment.");
      this.focusDraft();
      return;
    }

    const previousItemId = this.draft?.itemId;
    this.draft = {
      itemId: item.id,
      path: annotationPath(item.fileDiff, side),
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
    if (this.commentsLocked || comment.outdated) return;
    if (this.draft) {
      if (this.draft.editingId === comment.id) {
        this.focusDraft();
        return;
      }
      this.showInlineError("Save or discard the open comment draft before editing another comment.");
      this.focusDraft();
      return;
    }
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
    this.clearSelectedLines();
    if (itemId) this.refreshItem(itemId);
    this.clearInlineError();
  }

  private saveComment() {
    const draft = this.draft;
    if (!draft || this.commentsLocked) return;
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
    this.clearSelectedLines();
    this.refreshItem(itemId);
    this.refreshTreeDecorations();
    this.renderCommentList();
    this.clearInlineError();
  }

  private removeComment(id: number) {
    if (this.commentsLocked) return;
    const index = this.comments.findIndex((comment) => comment.id === id);
    if (index < 0) return;
    const [comment] = this.comments.splice(index, 1);
    if (this.draft?.editingId === id) this.draft = undefined;
    this.refreshItem(comment.itemId);
    this.refreshTreeDecorations();
    this.renderCommentList();
  }

  private refreshTreeDecorations() {
    this.tree?.setIcons(treeIcons);
  }

  private refreshItem(itemId: string) {
    const item = this.items.find((candidate) => candidate.id === itemId);
    if (!item) return;
    item.annotations = this.annotationsForItem(itemId);
    item.version = ++this.itemVersion;
    this.viewer?.updateItem(item);
  }

  private annotationsForItem(itemId: string): DiffLineAnnotation<AnnotationMetadata>[] {
    const annotations: DiffLineAnnotation<AnnotationMetadata>[] = this.comments
      .filter((comment) => comment.itemId === itemId && comment.id !== this.draft?.editingId)
      .map((comment) => ({
        side: comment.side,
        lineNumber: comment.end_line,
        metadata: { kind: "comment", comment },
      }));
    annotations.push(...this.questions
      .filter((thread) => thread.itemId === itemId)
      .map((thread) => ({
        side: thread.side,
        lineNumber: thread.endLine,
        metadata: { kind: "question" as const, thread },
      })));
    if (this.draft?.itemId === itemId) {
      annotations.push({
        side: this.draft.side,
        lineNumber: this.draft.endLine,
        metadata: { kind: "composer", draft: this.draft },
      });
    }
    return annotations;
  }

  private annotationElement(annotation: DiffLineAnnotation<AnnotationMetadata>) {
    if (annotation.metadata.kind === "composer") {
      return this.commentComposerElement(annotation.metadata.draft);
    }
    if (annotation.metadata.kind === "question") {
      return this.questionThreadElement(annotation.metadata.thread);
    }
    return this.pendingCommentElement(annotation.metadata.comment);
  }

  private commentComposerElement(draft: CommentDraft) {
    const element = document.createElement("section");
    element.className = "inline-comment-editor";
    const range = formatRange(draft.startLine, draft.endLine);
    const editorId = `comment-editor-${draft.itemId.replace(/[^a-zA-Z0-9_-]/g, "-")}`;
    element.innerHTML = `
      <header class="editor-heading">
        <div>
          <strong>${draft.editingId === undefined ? "Add a comment" : "Edit comment"} on ${draft.startLine === draft.endLine ? "line" : "lines"} ${escapeHtml(range)}</strong>
          <span title="${escapeHtml(draft.path)}">${escapeHtml(draft.path)}</span>
        </div>
      </header>
      <div class="editor-topbar" role="tablist" aria-label="Comment editor">
        <div class="editor-tabs">
          <button id="${editorId}-comment" role="tab" aria-controls="${editorId}-input" aria-selected="${draft.tab === "comment"}" tabindex="${draft.tab === "comment" ? "0" : "-1"}" class="${draft.tab === "comment" ? "active" : ""}" data-editor-tab="comment">Comment</button>
          <button id="${editorId}-preview-tab" role="tab" aria-controls="${editorId}-preview" aria-selected="${draft.tab === "preview"}" tabindex="${draft.tab === "preview" ? "0" : "-1"}" class="${draft.tab === "preview" ? "active" : ""}" data-editor-tab="preview">Preview</button>
        </div>
        <div class="formatting-tools" aria-label="Markdown formatting">
          ${formatButton("bold", "Bold")}
          ${formatButton("italic", "Italic")}
          ${formatButton("code", "Inline code")}
          ${formatButton("code-block", "Code block")}
          ${formatButton("link", "Link")}
          ${formatButton("list", "Bulleted list")}
          ${formatButton("quote", "Quote")}
        </div>
      </div>
      <textarea id="${editorId}-input" role="tabpanel" aria-labelledby="${editorId}-comment" class="comment-input" rows="6" placeholder="Leave a comment" ${draft.tab === "preview" ? "hidden" : ""}></textarea>
      <div id="${editorId}-preview" role="tabpanel" aria-labelledby="${editorId}-preview-tab" class="markdown-preview" ${draft.tab === "comment" ? "hidden" : ""}></div>
      <div class="editor-footer">
        <span class="editor-shortcut"><kbd>⌘</kbd><kbd>Enter</kbd> to save</span>
        <div class="composer-actions">
          <button class="button quiet" data-comment-action="cancel">Cancel</button>
          ${draft.editingId === undefined ? `<button class="button" data-agent-action data-comment-action="ask" ${draft.body.trim() && !this.agentUnavailable ? "" : "disabled"}>Ask <span aria-hidden="true">✨</span></button>` : ""}
          <button class="button primary" data-comment-action="save" ${draft.body.trim() && !this.commentsLocked ? "" : "disabled"}>${draft.editingId === undefined ? "Add comment" : "Save changes"}</button>
        </div>
      </div>`;

    const textarea = element.querySelector<HTMLTextAreaElement>(".comment-input");
    const saveButton = element.querySelector<HTMLButtonElement>("[data-comment-action=save]");
    const askButton = element.querySelector<HTMLButtonElement>("[data-comment-action=ask]");
    if (textarea) {
      textarea.value = draft.body;
      textarea.addEventListener("input", () => {
        if (this.draft === draft) draft.body = textarea.value;
        if (saveButton) saveButton.disabled = textarea.value.trim().length === 0 || this.commentsLocked;
        if (askButton) askButton.disabled = textarea.value.trim().length === 0 || this.agentUnavailable;
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
      button.addEventListener("keydown", (event) => {
        if (!["ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key)) return;
        event.preventDefault();
        const tab = event.key === "ArrowLeft" || event.key === "Home" ? "comment" : "preview";
        this.selectEditorTab(element, draft, tab);
        element.querySelector<HTMLButtonElement>(`[data-editor-tab=${tab}]`)?.focus();
      });
    }
    element.querySelector("[data-comment-action=cancel]")?.addEventListener("click", () => this.closeCommentComposer());
    element.querySelector("[data-comment-action=ask]")?.addEventListener("click", () => this.askDraftQuestion());
    element.querySelector("[data-comment-action=save]")?.addEventListener("click", () => this.saveComment());
    if (draft.tab === "preview") void this.renderPreviewElement(element, draft.body);
    return element;
  }

  private askDraftQuestion() {
    const draft = this.draft;
    const page = this.page;
    if (!draft || draft.editingId !== undefined || !page || this.agentUnavailable) return;
    if (!draft.body.trim()) {
      this.focusDraft();
      return;
    }
    const validationError = questionValidationError([], draft.body);
    if (validationError) {
      this.showInlineError(validationError);
      this.focusDraft();
      return;
    }

    const request = ++this.questionRequest;
    const threadId = crypto.randomUUID();
    const operationId = crypto.randomUUID();
    const thread = createQuestionThread(threadId, {
      itemId: draft.itemId,
      range: page.selected_range,
      path: draft.path,
      side: draft.side,
      startLine: draft.startLine,
      endLine: draft.endLine,
    }, draft.body, request, operationId);
    this.questions.push(thread);
    this.draft = undefined;
    this.clearSelectedLines();
    this.startQuestion(thread, request, operationId, page);
  }

  private askFollowUp(thread: QuestionThread) {
    const page = this.page;
    if (!page || this.agentUnavailable || thread.turn.kind !== "idle") return;
    const validationError = questionValidationError(thread.messages, thread.draft);
    if (validationError) {
      thread.validationError = validationError;
      this.refreshItem(thread.itemId);
      queueMicrotask(() => this.focusThreadDraft(thread));
      return;
    }
    const request = ++this.questionRequest;
    const operationId = crypto.randomUUID();
    if (!beginFollowUp(thread, request, operationId)) return;
    this.startQuestion(thread, request, operationId, page);
  }

  private retryThreadQuestion(thread: QuestionThread) {
    const page = this.page;
    if (!page || this.agentUnavailable || this.questionOperations.has(thread.id)) return;
    const request = ++this.questionRequest;
    const operationId = crypto.randomUUID();
    if (!retryQuestion(thread, request, operationId)) return;
    this.startQuestion(thread, request, operationId, page);
  }

  private startQuestion(
    thread: QuestionThread,
    request: number,
    operationId: string,
    page: ReviewPage,
  ) {
    const session = this.session;
    if (!session) return;
    this.questionOperations.set(thread.id, {
      threadId: thread.id,
      request,
      operationId,
    });
    this.announceAgent(`Tact is answering a question about ${thread.path}, ${formatRange(thread.startLine, thread.endLine)}.`);
    this.refreshItem(thread.itemId);
    this.syncAgentControls();
    void this.sendQuestion(session, thread, request, operationId, page);
  }

  private async sendQuestion(
    session: string,
    thread: QuestionThread,
    request: number,
    operationId: string,
    page: ReviewPage,
  ) {
    let reconcileWithServer = false;
    const epoch = this.sessionEpoch;
    try {
      const payload = await this.api.question(session, {
        thread_id: thread.id,
        operation_id: operationId,
        generation: page.generation,
        range: page.selected_range,
        path: thread.path,
        side: thread.side,
        start_line: thread.startLine,
        end_line: thread.endLine,
        messages: thread.messages.map((message) => ({ ...message })),
      });
      if (epoch !== this.sessionEpoch) return;
      if (payload.generation !== page.generation
        || !rangesEqual(payload.selected_range, page.selected_range)
        || !payload.answer.trim()) {
        throw new ProtocolMismatch("Tact returned an invalid answer for this review range.");
      }
      finishQuestion(thread, request, payload.answer);
      this.announceAgent(`Tact answered the question about ${thread.path}, ${formatRange(thread.startLine, thread.endLine)}.`);
    } catch (error) {
      this.recordActionError(error);
      if (errorCode(error) === "network_error") {
        reconcileWithServer = true;
        this.announceAgent("Reconnecting to the question…");
        this.questionsToPoll.add(thread.id);
        this.scheduleQuestionPoll();
        return;
      }
      const cancelled = errorCode(error) === "operation_cancelled";
      if (cancelled) cancelQuestion(thread, request);
      else failQuestion(thread, request, errorMessage(error));
      this.announceAgent(
        cancelled
          ? "Question cancelled."
          : `Tact could not answer the question: ${errorMessage(error)}`,
      );
    } finally {
      if (reconcileWithServer) return;
      const operation = this.questionOperations.get(thread.id);
      if (operation
        && operation.request === request) {
        this.questionOperations.delete(thread.id);
        this.questionsToPoll.delete(thread.id);
      }
      this.refreshItem(thread.itemId);
      this.syncAgentControls();
    }
  }

  private async stopQuestion(thread: QuestionThread) {
    const operation = this.questionOperations.get(thread.id);
    const page = this.page;
    const session = this.session;
    if (!page || !session || !operation || operation.threadId !== thread.id) return;
    if (!beginStopping(thread, operation.request)) return;
    this.announceAgent("Stopping the question…");
    this.refreshItem(thread.itemId);
    try {
      await this.api.cancelQuestion(session, {
        operation_id: operation.operationId,
        generation: page.generation,
        range: page.selected_range,
      });
    } catch (error) {
      stopFailed(thread, operation.request);
      this.refreshItem(thread.itemId);
      this.showInlineError(`Could not stop the question: ${errorMessage(error)}`);
      this.announceAgent(`Could not stop the question: ${errorMessage(error)}`);
    }
  }

  private questionThreadElement(thread: QuestionThread) {
    const element = document.createElement("article");
    element.className = "agent-thread";
    element.dataset.threadId = String(thread.id);
    const inputId = `thread-${thread.id}-input`;
    const headingId = `thread-${thread.id}-heading`;
    const contextId = `thread-${thread.id}-context`;
    const linesId = `thread-${thread.id}-lines`;
    const lineLabel = thread.startLine === thread.endLine ? "Line" : "Lines";
    element.setAttribute("aria-labelledby", `${headingId} ${contextId} ${linesId}`);
    if (thread.turn.kind === "asking") element.setAttribute("aria-busy", "true");
    element.innerHTML = `
      <header class="agent-thread-heading">
        <div><strong id="${headingId}">Ask Tact <span aria-hidden="true">✨</span></strong><span id="${linesId}">${lineLabel} ${escapeHtml(formatRange(thread.startLine, thread.endLine))}</span></div>
        <small id="${contextId}" title="${escapeHtml(thread.path)}">${escapeHtml(thread.path)}</small>
      </header>
      <div class="agent-thread-messages"></div>
      <div class="agent-thread-turn"></div>`;

    const messages = element.querySelector<HTMLElement>(".agent-thread-messages");
    for (const message of thread.messages) {
      const entry = document.createElement("section");
      entry.className = `agent-thread-message ${message.role}`;
      entry.innerHTML = `<strong>${message.role === "reviewer" ? "You" : "Tact"}</strong><div class="thread-markdown"></div>`;
      const body = entry.querySelector<HTMLElement>(".thread-markdown");
      if (body) void this.renderMarkdown(body, message.body);
      messages?.append(entry);
    }

    const turn = element.querySelector<HTMLElement>(".agent-thread-turn");
    if (!turn) return element;
    if (thread.turn.kind === "asking") {
      turn.className = "agent-thread-turn asking";
      turn.setAttribute("role", "status");
      turn.setAttribute("aria-live", "polite");
      turn.innerHTML = `<span class="thread-spinner" aria-hidden="true">${icon("sparkles")}</span><span>${thread.turn.stopping ? "Stopping…" : "Tact is answering…"}</span><button class="button quiet" data-thread-stop ${thread.turn.stopping ? "disabled" : ""}>${thread.turn.stopping ? "Stopping…" : "Stop"}</button>`;
      turn.querySelector("[data-thread-stop]")?.addEventListener("click", () => void this.stopQuestion(thread));
      return element;
    }
    if (thread.turn.kind === "error") {
      turn.className = "agent-thread-turn error";
      turn.setAttribute("role", "alert");
      turn.innerHTML = `<span>${escapeHtml(thread.turn.message)}</span><button class="button" data-agent-action data-thread-retry ${this.agentUnavailable ? "disabled" : ""}>Try again</button>`;
      turn.querySelector("[data-thread-retry]")?.addEventListener("click", () => this.retryThreadQuestion(thread));
      return element;
    }
    if (thread.turn.kind === "cancelled") {
      turn.className = "agent-thread-turn cancelled";
      turn.setAttribute("role", "status");
      turn.innerHTML = `<span>Question cancelled.</span><button class="button" data-agent-action data-thread-retry ${this.agentUnavailable ? "disabled" : ""}>Ask again</button>`;
      turn.querySelector("[data-thread-retry]")?.addEventListener("click", () => this.retryThreadQuestion(thread));
      return element;
    }

    const capacityError = questionValidationError(thread.messages, "x");
    if (capacityError) {
      turn.innerHTML = `<span class="agent-thread-limit" role="status">${escapeHtml(capacityError)}</span>`;
      return element;
    }
    turn.innerHTML = `
      <label for="${inputId}">Ask a follow-up</label>
      <textarea id="${inputId}" data-thread-input aria-describedby="${contextId} ${linesId} thread-${thread.id}-validation" rows="3" placeholder="Ask about this code" ${this.agentUnavailable ? "disabled" : ""}></textarea>
      <span id="thread-${thread.id}-validation" class="agent-thread-validation" role="alert" ${thread.validationError ? "" : "hidden"}>${escapeHtml(thread.validationError ?? "")}</span>
      <div><button class="button" data-agent-action data-thread-ask disabled>Ask <span aria-hidden="true">✨</span></button></div>`;
    const textarea = turn.querySelector<HTMLTextAreaElement>("textarea");
    const ask = turn.querySelector<HTMLButtonElement>("[data-thread-ask]");
    if (textarea) {
      textarea.value = thread.draft;
      textarea.addEventListener("input", () => {
        thread.draft = textarea.value;
        thread.validationError = undefined;
        const validation = turn.querySelector<HTMLElement>(".agent-thread-validation");
        if (validation) {
          validation.hidden = true;
          validation.textContent = "";
        }
        if (ask) ask.disabled = !textarea.value.trim() || this.agentUnavailable;
      });
      textarea.addEventListener("keydown", (event) => {
        if ((event.metaKey || event.ctrlKey) && event.key === "Enter") {
          this.askFollowUp(thread);
        }
      });
    }
    ask?.addEventListener("click", () => this.askFollowUp(thread));
    return element;
  }

  private focusThreadDraft(thread: QuestionThread) {
    const textarea = this.root.querySelector<HTMLTextAreaElement>(
      `[data-thread-id="${thread.id}"] [data-thread-input]`,
    );
    textarea?.focus();
    textarea?.setSelectionRange(textarea.value.length, textarea.value.length);
  }

  private announceAgent(message: string) {
    const announcement = this.root.querySelector<HTMLElement>("#agent-announcement");
    if (!announcement) return;
    announcement.textContent = "";
    queueMicrotask(() => { announcement.textContent = message; });
  }

  private pendingCommentElement(comment: CommentMetadata) {
    const element = document.createElement("article");
    element.className = "diff-comment";
    element.innerHTML = `
      <header>
        <span>${severityBadge(comment.body)} Lines ${formatRange(comment.start_line, comment.end_line)}</span>
        <div>
          <button class="small-icon-button" data-comment-edit aria-label="Edit comment" ${this.commentsLocked ? "disabled" : ""}>${icon("edit")}</button>
          <button class="small-icon-button danger" data-comment-delete aria-label="Delete comment" ${this.commentsLocked ? "disabled" : ""}>${icon("trash")}</button>
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
      const active = button.dataset.editorTab === tab;
      button.classList.toggle("active", active);
      button.setAttribute("aria-selected", String(active));
      button.tabIndex = active ? 0 : -1;
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
    const theme = activeSyntaxTheme(this.settings, this.host.theme() === "dark");
    await renderMarkdown(container, body, theme);
  }

  private focusDraft() {
    const textarea = this.root.querySelector<HTMLTextAreaElement>(".inline-comment-editor .comment-input");
    textarea?.focus();
    textarea?.setSelectionRange(textarea.value.length, textarea.value.length);
  }

  private renderCommentList() {
    const count = this.root.querySelector<HTMLElement>("#comment-count");
    const mobileCount = this.root.querySelector<HTMLElement>("#mobile-comment-count");
    const list = this.root.querySelector<HTMLElement>("#comment-list");
    if (!count || !list) return;
    count.textContent = String(this.comments.length);
    if (mobileCount) {
      mobileCount.textContent = String(this.comments.length);
      mobileCount.hidden = this.comments.length === 0;
    }
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
          <span>${severityBadge(comment.body)} ${formatRange(comment.start_line, comment.end_line)} · ${comment.side === "additions" ? "new" : "old"}</span>
          <p>${escapeHtml(comment.body)}</p>
        </button>
        <div class="comment-link-actions">
          <button aria-label="Edit comment" data-edit ${this.commentsLocked || comment.outdated ? "disabled" : ""}>${icon("edit")}</button>
          <button aria-label="Delete comment" data-delete ${this.commentsLocked ? "disabled" : ""}>${icon("trash")}</button>
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
    this.syncAgentControls();
  }

  private setRangeReady() {
    this.loadingRange = undefined;
    this.syncAgentControls();
    const state = this.root.querySelector<HTMLElement>("#scope-state");
    if (state?.classList.contains("loading")) state.hidden = true;
  }

  private showRangeError(
    message: string,
    range: ReviewRange,
    discardCurrentFeedback = false,
  ) {
    const state = this.root.querySelector<HTMLElement>("#scope-state");
    if (!state) return;
    state.className = "scope-state error";
    state.setAttribute("role", "alert");
    state.innerHTML = `
      <div class="scope-error-icon">!</div>
      <strong>Could not load the selected range</strong>
      <span>${escapeHtml(message)}</span>
      <div class="scope-error-actions"><button class="button primary" data-range-retry>Retry</button>${this.page ? '<button class="button" data-range-keep>Keep current range</button>' : ""}</div>`;
    state.querySelector("[data-range-retry]")?.addEventListener(
      "click",
      () => void this.selectRange(range, discardCurrentFeedback),
    );
    state.querySelector("[data-range-keep]")?.addEventListener("click", () => { state.hidden = true; });
    state.hidden = false;
  }

  private syncSelectedRange(range: ReviewRange) {
    const label = this.root.querySelector<HTMLElement>("#range-label");
    const button = this.root.querySelector<HTMLButtonElement>("#range-button");
    if (label) label.textContent = rangeLabel(this.bootstrap.range_targets, range);
    if (button) button.disabled = this.loadingRange !== undefined || this.agentBusy;
    const refresh = this.root.querySelector<HTMLButtonElement>("#refresh-notice");
    if (refresh) refresh.disabled = this.loadingRange !== undefined || this.agentBusy || this.refreshing;
  }

  private syncAgentControls() {
    const commentsLocked = this.commentsLocked;
    const agentUnavailable = this.agentUnavailable;
    for (const textarea of this.root.querySelectorAll<HTMLTextAreaElement>(".comment-input")) {
      textarea.disabled = commentsLocked;
    }
    for (const textarea of this.root.querySelectorAll<HTMLTextAreaElement>("[data-overview-instructions], [data-thread-input]")) {
      textarea.disabled = agentUnavailable;
    }
    for (const button of this.root.querySelectorAll<HTMLButtonElement>(".inline-comment-editor [data-format], [data-edit], [data-delete]")) {
      button.disabled = commentsLocked;
    }
    for (const button of this.root.querySelectorAll<HTMLButtonElement>("[data-agent-action]")) {
      const needsQuestion = button.matches("[data-comment-action=ask], [data-thread-ask]");
      const editor = button.closest<HTMLElement>(".inline-comment-editor, .agent-thread-turn");
      const input = editor?.querySelector<HTMLTextAreaElement>("textarea");
      const overviewDraft = this.root.querySelector<HTMLTextAreaElement>("[data-overview-instructions]")?.value;
      const overviewUnchanged = overviewDraft !== undefined
        && this.currentOverview(rangeKey(this.page?.selected_range ?? this.bootstrap.default_range))?.instructions
          === overviewDraft.trim();
      button.disabled = agentUnavailable || (button.id === "ai-review" && (this.aiReviewPending || this.refresher.stale))
        || (button.matches("[data-edit-overview], [data-generate-overview], [data-retry-overview]")
          && this.loadingOverview !== undefined)
        || (button.matches("[data-generate-overview]")
          && (overviewUnchanged || overviewInstructionError(overviewDraft ?? "") !== undefined))
        || (needsQuestion && !input?.value.trim());
      button.title = this.agentUnavailableReason;
    }
    for (const button of this.root.querySelectorAll<HTMLButtonElement>("[data-comment-action=save], [data-comment-edit], [data-comment-delete]")) {
      button.disabled = commentsLocked || (button.matches("[data-comment-action=save]")
        && !this.draft?.body.trim());
    }
    const summary = this.root.querySelector<HTMLTextAreaElement>("#review-summary");
    if (summary) summary.disabled = commentsLocked;
    const hasFeedback = this.comments.length > 0 || this.feedback.summary.trim().length > 0;
    for (const button of this.root.querySelectorAll<HTMLButtonElement>("[data-decision]")) {
      button.disabled = commentsLocked || !this.page
        || (button.dataset.decision === "request_changes" && !hasFeedback);
    }
    this.syncSelectedRange(this.page?.selected_range ?? this.bootstrap.default_range);
    this.refresher.setBlocked(!this.loaded || this.refreshing || this.agentBusy
      || this.loadingRange !== undefined || this.sending);
    this.syncRefreshState();
  }

  /**
   * Composes the review into canonical markdown on the server and hands it to the chat. Comments
   * whose lines no longer exist cannot be anchored, so they travel as plain text.
   */
  private async sendToChat(decision: ReviewDecision["decision"]) {
    if (!this.page || this.commentsLocked) return;
    if (this.draft) {
      this.showInlineError("Save or discard the open comment draft before sending the review.");
      this.focusDraft();
      return;
    }
    if (this.refresher.stale) {
      await this.refreshReview();
      if (this.refresher.stale || this.disposed) {
        this.showInlineError("The workspace changed. Wait for the diff to update, then send again.", () => void this.sendToChat(decision));
        return;
      }
    }
    const page = this.page;
    const feedback = this.feedback;
    this.sending = true;
    this.syncAgentControls();
    try {
      const markdown = await this.api.compose({
        generation: page.generation,
        range: page.selected_range,
        decision,
        summary: [
          feedback.summary.trim(),
          ...feedback.comments.filter((comment) => comment.outdated).map(outdatedNote),
        ].filter(Boolean).join("\n\n"),
        comments: feedback.comments.filter((comment) => !comment.outdated)
          .map(({ path, side, start_line, end_line, body }) => ({ path, side, start_line, end_line, body })),
      });
      await this.host.sendToChat(markdown);
    } catch (error) {
      this.recordActionError(error);
      this.showInlineError(errorMessage(error), () => void this.sendToChat(decision));
      return;
    } finally {
      this.sending = false;
      this.syncAgentControls();
    }
    const sent = new Set(feedback.comments.map((comment) => comment.itemId));
    feedback.comments.length = 0;
    feedback.summary = "";
    const summary = this.root.querySelector<HTMLTextAreaElement>("#review-summary");
    if (summary) summary.value = "";
    for (const itemId of sent) this.refreshItem(itemId);
    this.refreshTreeDecorations();
    this.renderCommentList();
    this.syncAgentControls();
    this.showNotice("Sent to chat.");
  }

  private showNotice(message: string) {
    const status = this.root.querySelector<HTMLElement>("#review-status");
    if (!status) return;
    status.className = "review-status notice";
    status.setAttribute("role", "status");
    status.textContent = message;
    window.setTimeout(() => {
      if (status.textContent === message) this.clearStatus();
    }, 4000);
  }

  private showInlineError(message: string, retry?: () => void) {
    const status = this.root.querySelector<HTMLElement>("#review-status");
    if (!status) return;
    status.className = "review-status error";
    status.setAttribute("role", "alert");
    status.innerHTML = `${escapeHtml(message)}${retry ? ` <button class="text-button" data-status-retry>Retry</button>` : ""}`;
    if (retry) status.querySelector("[data-status-retry]")?.addEventListener("click", retry);
  }

  private clearInlineError() {
    if (this.root.querySelector("#review-status.error")) this.clearStatus();
  }

  private clearStatus() {
    const status = this.root.querySelector<HTMLElement>("#review-status");
    if (!status) return;
    status.className = "review-status";
    status.textContent = "";
  }
}

function annotationKey(item: CodeViewDiffItem<AnnotationMetadata>) {
  return (item.annotations ?? []).map(({ side, lineNumber, metadata }) => {
    const id = metadata.kind === "comment" ? metadata.comment.id
      : metadata.kind === "question" ? metadata.thread.id
      : "draft";
    return metadata.kind + ":" + id + ":" + side + ":" + lineNumber;
  }).join("|");
}

function outdatedNote(comment: CommentMetadata) {
  const lines = formatRange(comment.start_line, comment.end_line);
  return "Comment on " + comment.path + " (lines " + lines + " of an earlier version, since changed): " + comment.body;
}

function deepActiveElement(root: Document | ShadowRoot): HTMLElement | undefined {
  const active = root.activeElement;
  if (!(active instanceof HTMLElement)) return;
  return active.shadowRoot ? deepActiveElement(active.shadowRoot) ?? active : active;
}

function severityBadge(body: string) {
  const severity = /^\[(P[0-3])\]/.exec(body)?.[1];
  return severity ? `<b class="severity-badge severity-${severity.toLowerCase()}">${severity}</b>` : "";
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

function formatButton(format: FormattingIconName, label: string) {
  return `<button class="format-button" data-format="${format}" aria-label="${label}" title="${label}">${icon(format)}</button>`;
}

function formatRange(start: number, end: number) {
  return start === end ? String(start) : `${Math.min(start, end)}–${Math.max(start, end)}`;
}

function escapeHtml(value: string) {
  return value.replace(/[&<>'"]/g, (character) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", "'": "&#39;", '"': "&quot;",
  })[character] ?? character);
}
