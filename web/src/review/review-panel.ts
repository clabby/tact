import type { DiffLineAnnotation, FileDiffMetadata } from "@pierre/diffs";
import { renderMarkdown } from "../core/markdown";
import type { Workspaces } from "../core/wire";
import { checkoutMenu, workspaceLabel } from "../core/workspaces";
import { openMenu } from "../ui/menu";
import { AiReview } from "./ai-review";
import { annotationKey, type AnnotationMetadata, type ReviewDiffItem } from "./annotations";
import { ChangedFilesTree } from "./changed-files-tree";
import {
  TouchedCheckouts,
  changeAffectsTarget,
  loadCheckoutChoice,
  saveCheckoutChoice,
  touchedNotice,
} from "./checkout-target";
import { CommentEditor } from "./comment-editor";
import { annotationPath } from "./comment-state";
import { DiffView } from "./diff-view";
import { changeStats } from "./file-tree-stats";
import { escapeHtml, formatRange } from "./markup";
import { overviewInstructionError } from "./overview-instructions";
import { OverviewPanel } from "./overview-panel";
import { panelMarkup } from "./panel-markup";
import type { ReviewDecision, ReviewPage, ReviewSession } from "./protocol";
import { QuestionThreads } from "./question-threads";
import { RangeDialog } from "./range-dialog";
import { rangeKey, rangeLabel, rangesEqual, type ReviewRange } from "./range-selection";
import { RefreshScheduler } from "./refresh-scheduler";
import { ReviewApi, errorCode, errorMessage, type ReviewTransport } from "./review-api";
import { parseReviewPatch } from "./review-diff";
import { carryFeedback, carryQuestions, changedFileNames } from "./review-live";
import { activeSyntaxTheme, appearance } from "./review-settings";
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
import { ReviewStatus } from "./review-status";
import { SEARCH_HIGHLIGHT, SearchBar } from "./search-bar";
import { SettingsPopover } from "./settings-popover";
import "./review-panel.css";

/** What the application shell lends the review panel. */
export type ReviewHost = {
  api: ReviewTransport;
  /** Where the reviewed checkout of each session is remembered; private to the machine the sessions run on. */
  storage: Pick<Storage, "getItem" | "setItem">;
  activeSession(): string | null;
  onActiveSessionChange(listener: () => void): () => void;
  /** Fires when files in `checkout` (null: the session's workspace) may have changed. */
  onWorkspaceChanged(listener: (checkout: string | null) => void): () => void;
  /** The checkouts of the session's repository, or of the default workspace's without a session. */
  workspaces(session: string | null): Promise<Workspaces>;
  /** Whether any live session is working, and so possibly editing the workspace. */
  anyRunning(): boolean;
  /** Whether the active session is working. */
  sessionRunning(): boolean;
  onRunningChange(listener: () => void): () => void;
  /** Places the composed review into the active session's draft. */
  sendToChat(markdown: string): void | Promise<void>;
  theme(): "light" | "dark";
  onThemeChange(listener: () => void): () => void;
};

/** How long workspace changes are coalesced before the visible diff is refreshed. */
const LIVE_REFRESH_DELAY_MS = 400;
/** How often the checkouts the agent touches are polled while the session runs and the panel is shown. */
const WORKSPACES_POLL_MS = 4000;

/**
 * Mounts the review panel into `container`, which must have a definite height. The panel loads and
 * keeps the diff current whether or not it is shown; `setVisible` tells it when to redraw the diff,
 * which cannot be laid out while hidden.
 */
export function mountReviewPanel(container: HTMLElement, host: ReviewHost) {
  const panel = new ReviewPanel(container, host);
  void panel.start();
  return {
    dispose: () => panel.cleanUp(),
    setVisible: (visible: boolean) => panel.setVisible(visible),
    revealFile: (path: string, line: number | null) => panel.revealFile(path, line),
  };
}

class ReviewPanel {
  private page?: ReviewPage;
  private files: FileDiffMetadata[] = [];
  private items: ReviewDiffItem[] = [];
  private readonly pathToItem = new Map<string, string>();
  private nextCommentId = 1;
  private loadingRange?: ReviewRange;
  private rangeRequest = 0;
  private sending = false;
  private refreshing = false;
  private readonly refresher = new RefreshScheduler(() => void this.refreshReview(), LIVE_REFRESH_DELAY_MS);
  private visible = true;
  private disposed = false;
  private loaded = false;
  private session: string | null;
  /** Invalidates every response that was requested for an earlier session or checkout. */
  private sessionEpoch = 0;
  /** The reviewed checkout's path, or null for the session's workspace. */
  private target: string | null = null;
  private switchingCheckout = false;
  private workspaces: Workspaces | null = null;
  private workspacesRequest = 0;
  private workspacesTimer = 0;
  private readonly touched = new TouchedCheckouts();
  private readonly api: ReviewApi;
  private bootstrap!: ReviewSession;
  private state!: ReviewState;
  private readonly unsubscribe: Array<() => void>;
  private readonly settings: SettingsPopover;
  private readonly status: ReviewStatus;
  private readonly diffView: DiffView;
  private readonly search: SearchBar;
  private readonly fileTree: ChangedFilesTree;
  private readonly rangeDialog: RangeDialog;
  private readonly commentEditor: CommentEditor;
  private readonly questionThreads: QuestionThreads;
  private readonly overview: OverviewPanel;
  private readonly aiReview: AiReview;

  constructor(
    private readonly root: HTMLElement,
    private readonly host: ReviewHost,
  ) {
    this.session = host.activeSession();
    this.api = new ReviewApi(host.api, () => this.target, () => this.session);
    const api = this.api;
    this.settings = new SettingsPopover(root, () => this.applySettings(true));
    this.status = new ReviewStatus(root);
    this.diffView = new DiffView(root, {
      settings: () => this.settings.current,
      appearance: () => appearance(this.settings.current, this.host.theme()),
      seenFiles: () => this.seenFiles(),
      seenChanged: (path, seen) => {
        if (!seen) this.search.fileExpanded(path);
        this.fileTree.refreshDecorations();
      },
      renderAnnotation: (annotation) => this.annotationElement(annotation),
      openComposer: (selection) => this.commentEditor.open(selection),
      selectedLinesChanged: (selection) => this.search.selectedLinesChanged(selection),
      itemRendered: (itemId, root) => this.search.itemRendered(itemId, root),
      changeRange: () => this.rangeDialog.open(),
      refresh: () => void this.refreshReview(),
    });
    this.search = new SearchBar(root, {
      visible: () => this.visible,
      files: () => this.files,
      items: () => this.items,
      viewer: () => this.diffView.viewer,
      seenFiles: () => this.seenFiles(),
      setCollapsed: (item, collapsed) => this.diffView.setCollapsed(item, collapsed),
      selectMobilePanel: (name) => this.selectMobilePanel(name),
    });
    this.fileTree = new ChangedFilesTree(root, {
      comments: () => this.comments,
      seenFiles: () => this.seenFiles(),
      appearance: () => appearance(this.settings.current, this.host.theme()),
      selectFile: (path) => this.selectFile(path),
    });
    this.rangeDialog = new RangeDialog(root, {
      targets: () => this.bootstrap.range_targets,
      currentRange: () => this.page?.selected_range ?? this.bootstrap.default_range,
      canChange: () => !this.loadingRange && !this.agentBusy,
      pendingFeedback: () => feedbackDescription(this.feedback),
      selectRange: (range, discardCurrentFeedback) => this.selectRange(range, discardCurrentFeedback),
    });
    this.commentEditor = new CommentEditor(root, {
      status: this.status,
      feedback: () => this.feedback,
      items: () => this.items,
      viewer: () => this.diffView.viewer,
      commentsLocked: () => this.commentsLocked,
      agentUnavailable: () => this.agentUnavailable,
      nextCommentId: () => this.nextCommentId++,
      refreshItem: (itemId) => this.refreshItem(itemId),
      refreshTreeDecorations: () => this.fileTree.refreshDecorations(),
      clearSelectedLines: () => this.search.clearSelectedLines(),
      selectTab: (name) => this.selectTab(name),
      askQuestion: () => this.questionThreads.askDraft(),
      renderMarkdown: (container, body) => this.renderMarkdown(container, body),
    });
    this.questionThreads = new QuestionThreads(root, {
      api,
      status: this.status,
      session: () => this.session,
      sessionEpoch: () => this.sessionEpoch,
      generation: () => this.bootstrap.generation,
      page: () => this.page,
      feedback: () => this.feedback,
      questions: () => this.questions,
      allQuestions: () => allQuestions(this.state),
      agentUnavailable: () => this.agentUnavailable,
      refreshItem: (itemId) => this.refreshItem(itemId),
      syncAgentControls: () => this.syncAgentControls(),
      markStale: () => this.refresher.markStale(),
      recordActionError: (error) => this.recordActionError(error),
      clearSelectedLines: () => this.search.clearSelectedLines(),
      focusDraft: () => this.commentEditor.focusDraft(),
      renderMarkdown: (container, body) => this.renderMarkdown(container, body),
    });
    this.overview = new OverviewPanel(root, {
      api,
      session: () => this.session,
      page: () => this.page,
      generation: () => this.bootstrap.generation,
      storedOverview: () => this.bootstrap.overview,
      agentUnavailable: () => this.agentUnavailable,
      appearance: () => appearance(this.settings.current, this.host.theme()),
      syncAgentControls: () => this.syncAgentControls(),
      recordActionError: (error) => this.recordActionError(error),
    });
    this.aiReview = new AiReview(root, {
      api,
      status: this.status,
      session: () => this.session,
      page: () => this.page,
      items: () => this.items,
      comments: () => this.comments,
      nextCommentId: () => this.nextCommentId++,
      agentUnavailable: () => this.agentUnavailable,
      stale: () => this.refresher.stale,
      refreshItem: (itemId) => this.refreshItem(itemId),
      refreshTreeDecorations: () => this.fileTree.refreshDecorations(),
      renderCommentList: () => this.commentEditor.renderList(),
      selectTab: (name) => this.selectTab(name),
      syncAgentControls: () => this.syncAgentControls(),
      recordActionError: (error) => this.recordActionError(error),
    });
    this.unsubscribe = [
      host.onWorkspaceChanged((checkout) => this.workspaceChanged(checkout)),
      host.onRunningChange(() => this.runningChanged()),
      host.onActiveSessionChange(() => void this.sessionChanged()),
      host.onThemeChange(() => this.themeChanged()),
    ];
    document.addEventListener("click", this.settings.close);
  }

  private get feedback() { return currentFeedback(this.state); }
  private get comments() { return this.feedback.comments; }
  private get questions() { return currentQuestions(this.state); }
  private get draft() { return this.feedback.draft; }
  private set draft(value: CommentDraft | undefined) { this.feedback.draft = value; }
  private get running() { return this.host.anyRunning(); }
  private get agentBusy() {
    return this.overview.loading || this.aiReview.pending || this.questionThreads.busy;
  }
  /** Comments are local to the browser, so only snapshot changes in flight lock them. */
  private get commentsLocked() {
    return this.loadingRange !== undefined || this.refreshing || this.sending || this.switchingCheckout;
  }
  /** Overviews, AI review, and questions need an idle chat session to run in. */
  private get agentUnavailable() {
    return this.commentsLocked || this.running || this.session === null;
  }
  private get agentUnavailableReason() {
    if (this.session === null) return "Open a chat to use the agent here.";
    return this.running ? "The agent is working. These actions are available when it finishes." : "";
  }

  /** The session's workspace path, once known. */
  private get sessionWorkspace() {
    return this.workspaces?.checkouts.find((checkout) => checkout.current)?.path
      ?? (this.target === null && this.loaded ? this.bootstrap.checkout?.path ?? null : null);
  }

  private get targetPath() {
    return this.target ?? this.sessionWorkspace;
  }

  async start() {
    this.root.classList.add("review-panel");
    await this.load();
  }

  private async load() {
    const epoch = this.sessionEpoch;
    if (this.session !== null) this.target = loadCheckoutChoice(this.host.storage, this.session);
    this.root.innerHTML = '<div class="panel-notice" role="status"><span class="activity-spinner" aria-hidden="true"></span>Loading changes…</div>';
    try {
      const review = await this.api.review(this.session);
      if (this.disposed || epoch !== this.sessionEpoch) return;
      this.bootstrap = review;
      this.state = createReviewState(review);
      this.loaded = true;
      this.render();
      this.installInitialPage();
      void this.refreshWorkspaces();
    } catch (error) {
      if (this.disposed || epoch !== this.sessionEpoch) return;
      if (this.forgetUnknownCheckout(error)) return void this.load();
      this.root.innerHTML = '<div class="panel-notice error" role="alert"><strong>Could not load the changes</strong><span></span><button class="button primary" data-retry>Retry</button></div>';
      const message = this.root.querySelector("span");
      if (message) message.textContent = errorMessage(error);
      this.root.querySelector("[data-retry]")?.addEventListener("click", () => void this.load());
    }
  }

  setVisible(visible: boolean) {
    this.visible = visible;
    if (visible && this.loaded) this.diffView.viewer?.render(true);
    clearInterval(this.workspacesTimer);
    this.workspacesTimer = 0;
    if (!visible || this.disposed) return;
    void this.refreshWorkspaces();
    this.workspacesTimer = window.setInterval(() => {
      if (this.host.sessionRunning()) void this.refreshWorkspaces();
    }, WORKSPACES_POLL_MS);
  }

  installInitialPage() {
    this.overview.restore();
    this.installPage(this.bootstrap.page);
    this.restoreAgentOperation();
  }

  private restoreAgentOperation() {
    const overview = this.bootstrap.overview;
    if (overview?.status === "generating"
      && rangesEqual(overview.selected_range, this.page?.selected_range)) {
      void this.overview.load(true);
    }
    void this.restoreActiveQuestions();
  }

  private async restoreActiveQuestions() {
    const active = allQuestions(this.state).filter((thread) => thread.turn.kind === "asking");
    for (const thread of active) this.questionThreads.resume(thread);
    const current = active.find((thread) => rangesEqual(thread.range, this.page?.selected_range));
    if (!current && active[0]) await this.selectRange(active[0].range, false, true);
  }

  render() {
    this.root.innerHTML = panelMarkup(this.bootstrap.repository, this.settings.markup(), this.rangeDialog.markup());
    this.bindEvents();
    this.settings.syncControls();
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
    clearInterval(this.workspacesTimer);
    for (const stop of this.unsubscribe) stop();
    this.refresher.dispose();
    this.questionThreads.dispose();
    this.search.dispose();
    document.removeEventListener("click", this.settings.close);
    this.diffView.dispose();
    CSS.highlights?.delete(SEARCH_HIGHLIGHT);
    this.fileTree.dispose();
    this.root.replaceChildren();
    this.root.classList.remove("review-panel");
  }

  private runningChanged() {
    if (!this.loaded) return;
    if (!this.overview.loading) this.overview.render();
    for (const item of this.items) {
      if (item.annotations?.length) this.refreshItem(item.id);
    }
    this.commentEditor.renderList();
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
      this.search.reset();
      this.overview.cancelEditing();
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
        version: this.diffView.nextVersion(),
      };
    });
    this.files = this.items.map((item) => item.fileDiff);
    this.attachQuestionItems();
    for (const item of this.items) {
      item.annotations = this.annotationsForItem(item.id);
      if (previousItems.get(item.id) === item && previousKeys.get(item.id) !== annotationKey(item)) {
        item.version = this.diffView.nextVersion();
      }
    }

    const description = this.root.querySelector<HTMLElement>("#scope-description");
    if (description) {
      description.textContent = page.scope;
      // The range button already names a preset scope; the description only adds detail.
      description.hidden = page.scope === rangeLabel(this.bootstrap.range_targets, page.selected_range);
    }
    this.renderStats();
    this.overview.render();
    this.diffView.render(this.items, !preserveView);
    this.fileTree.render(this.files);
    this.commentEditor.renderList();
    this.syncSelectedRange(page.selected_range);
    this.syncAgentControls();
    if (!preserveView) {
      const summary = this.root.querySelector<HTMLTextAreaElement>("#review-summary");
      if (summary) summary.value = this.feedback.summary;
      this.selectTab("changes");
    } else if (this.search.isOpen()) {
      this.search.refreshCount();
    }
  }

  private attachQuestionItems() {
    for (const thread of this.questions) {
      const item = this.items.find(
        (candidate) => annotationPath(candidate.fileDiff, thread.side) === thread.path,
      );
      thread.itemId = item?.id ?? "";
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
      <strong class="add">+${escapeHtml(String(stats.additions))}</strong>
      <strong class="del">−${escapeHtml(String(stats.deletions))}</strong>`;
  }

  private selectFile(path: string) {
    const id = this.pathToItem.get(path);
    if (!id) return;
    // On narrow screens the file list replaces the diff, so return to the diff before scrolling.
    this.selectMobilePanel("diff");
    this.diffView.viewer?.scrollTo({ type: "item", id, align: "start", behavior: "smooth-auto" });
  }

  /**
   * Shows the changed file a reference names, at a line of its new version when given. The
   * reference may be relative to the workspace, absolute, or a trailing part of the path. Returns
   * false when no changed file matches; the scroll happens in the next frame, once the caller has
   * made the review visible.
   */
  revealFile(reference: string, line: number | null) {
    const path = reference.replace(/^\.\//, "");
    const id = this.pathToItem.get(path)
      ?? [...this.pathToItem.keys()].find((name) => path.endsWith(`/${name}`) || name.endsWith(`/${path}`));
    if (!id) return false;
    requestAnimationFrame(() => {
      this.selectTab("changes");
      this.selectMobilePanel("diff");
      const item = this.items.find((candidate) => candidate.id === id);
      if (item?.collapsed) this.diffView.setCollapsed(item, false);
      if (line === null) {
        this.diffView.viewer?.scrollTo({ type: "item", id, align: "start", behavior: "smooth-auto" });
        return;
      }
      this.diffView.viewer?.setSelectedLines({
        id,
        range: { start: line, end: line, side: "additions", endSide: "additions" },
      }, { notify: false });
      this.diffView.viewer?.scrollTo({ type: "line", id, lineNumber: line, side: "additions", align: "center", behavior: "smooth-auto" });
    });
    return true;
  }

  private seenFiles() {
    return this.feedback.seenPaths;
  }

  private bindEvents() {
    this.search.bind();
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
    this.rangeDialog.bind();
    this.root.querySelector("#refresh-notice")?.addEventListener("click", () => this.refresher.retry());
    this.root.querySelector("#checkout-button")?.addEventListener("click", () => this.openCheckoutMenu());
    this.root.querySelector("#touched-notice")?.addEventListener("click", () => this.reviewTouched());
    this.root.querySelector("#ai-review")?.addEventListener("click", () => void this.aiReview.run());
    for (const button of this.root.querySelectorAll<HTMLButtonElement>("[data-decision]")) {
      button.addEventListener("click", () => void this.sendToChat(button.dataset.decision as ReviewDecision["decision"]));
    }
    this.settings.bind();
  }

  /** On narrow screens the file list and the comment list replace the diff; the navigation control switches between all three. */
  private selectMobilePanel(name: "diff" | "files" | "comments") {
    for (const button of this.root.querySelectorAll<HTMLButtonElement>("[data-mobile-panel]")) {
      button.setAttribute("aria-pressed", String(button.dataset.mobilePanel === name));
    }
    const panel = this.root.querySelector("#changes-panel");
    if (panel?.getAttribute("data-mobile-active") === name) return;
    panel?.setAttribute("data-mobile-active", name);
    if (name === "diff") this.diffView.viewer?.render(true);
  }

  private bindMobileNavigation() {
    for (const button of this.root.querySelectorAll<HTMLButtonElement>("[data-mobile-panel]")) {
      button.addEventListener("click", () => {
        this.selectMobilePanel(button.dataset.mobilePanel as "diff" | "files" | "comments");
      });
    }
    this.selectMobilePanel("diff");
  }

  private workspaceChanged(checkout: string | null) {
    if (changeAffectsTarget(checkout, this.target, this.sessionWorkspace)) this.refresher.markStale();
  }

  private async sessionChanged() {
    const session = this.host.activeSession();
    if (session === this.session) return;
    this.session = session;
    this.target = session === null ? null : loadCheckoutChoice(this.host.storage, session);
    this.workspaces = null;
    this.touched.reset();
    const epoch = ++this.sessionEpoch;
    // Responses requested for the previous session are ignored when they arrive.
    this.overview.reset();
    this.aiReview.reset();
    this.questionThreads.reset();
    if (!this.loaded) {
      void this.load();
      return;
    }
    this.syncAgentControls();
    try {
      let review: ReviewSession;
      try {
        review = await this.api.review(session);
      } catch (error) {
        if (!this.forgetUnknownCheckout(error)) throw error;
        review = await this.api.review(session);
      }
      if (epoch === this.sessionEpoch && !this.disposed) await this.installSnapshot(review, epoch);
    } catch (error) {
      if (epoch === this.sessionEpoch) this.status.showInlineError(errorMessage(error));
    }
    if (epoch === this.sessionEpoch) void this.refreshWorkspaces();
  }

  /**
   * Drops a remembered checkout the server no longer reviews for this session, so the panel
   * returns to the session's workspace. Reports whether it did.
   */
  private forgetUnknownCheckout(error: unknown) {
    if (this.target === null || (error as { status?: unknown }).status !== 404) return false;
    if (this.session !== null) saveCheckoutChoice(this.host.storage, this.session, null);
    this.target = null;
    return true;
  }

  /** Fetches the session's checkouts for the selector and the touched notice. */
  private async refreshWorkspaces() {
    const epoch = this.sessionEpoch;
    const request = ++this.workspacesRequest;
    let workspaces: Workspaces;
    try {
      workspaces = await this.host.workspaces(this.session);
    } catch {
      // The selector keeps its last state; the next poll asks again.
      return;
    }
    if (this.disposed || !this.loaded || epoch !== this.sessionEpoch || request !== this.workspacesRequest) return;
    this.workspaces = workspaces;
    const target = workspaces.checkouts.find((checkout) => checkout.path === this.target);
    if (this.target !== null && (!target || target.missing)) {
      const lost = this.target;
      await this.switchCheckout(null, true);
      this.status.showInlineError(`${lost} is no longer available, so the review shows the session's workspace.`);
      return;
    }
    this.touched.update(workspaces.checkouts, this.targetPath ?? "");
    this.renderCheckout();
  }

  private openCheckoutMenu() {
    const button = this.root.querySelector<HTMLButtonElement>("#checkout-button");
    const workspaces = this.workspaces;
    if (!button || !workspaces) return;
    this.touched.acknowledge();
    this.renderCheckout();
    const session = this.sessionWorkspace;
    openMenu(button, checkoutMenu(workspaces, {
      selected: this.targetPath ?? "",
      recent: false,
      pick: (path) => void this.switchCheckout(path === session ? null : path),
    }), "Review checkout");
  }

  /** Follows the touched notice: one checkout is opened directly, several are offered in the selector. */
  private reviewTouched() {
    const unseen = this.touched.unseen;
    if (unseen.length !== 1) return this.openCheckoutMenu();
    this.touched.acknowledge();
    this.renderCheckout();
    const path = unseen[0]!.path;
    void this.switchCheckout(path === this.sessionWorkspace ? null : path);
  }

  /**
   * Reviews another checkout from its default range. Comments belong to the diff they were written
   * on, so pending feedback is discarded, after confirmation unless `force` is set.
   */
  private async switchCheckout(target: string | null, force = false) {
    if (target === this.target || this.switchingCheckout || !this.loaded) return;
    if (!force && (this.loadingRange || this.refreshing || this.agentBusy || this.sending)) return;
    const pending = feedbackDescription(this.feedback);
    if (pending && !force && !confirm(`Switching checkouts will discard ${pending}.`)) return;
    const previous = this.target;
    this.target = target;
    const epoch = ++this.sessionEpoch;
    this.overview.reset();
    this.aiReview.reset();
    this.questionThreads.reset();
    this.switchingCheckout = true;
    const label = workspaceLabel(this.targetPath ?? "", this.workspaces?.checkouts);
    this.showLoading(`Loading ${label}`, "Capturing the checkout's changes.");
    this.syncAgentControls();
    try {
      const review = await this.api.review(this.session);
      if (epoch !== this.sessionEpoch || this.disposed) return;
      await this.installSnapshot(review, epoch);
      if (this.session !== null) saveCheckoutChoice(this.host.storage, this.session, target);
    } catch (error) {
      if (epoch !== this.sessionEpoch) return;
      this.target = previous;
      this.status.showInlineError(errorMessage(error));
    } finally {
      if (epoch === this.sessionEpoch) {
        this.switchingCheckout = false;
        this.hideLoading();
        if (this.workspaces) this.touched.update(this.workspaces.checkouts, this.targetPath ?? "");
        this.syncAgentControls();
      }
    }
  }

  /** The checkout selector, shown when the repository has several checkouts, and the touched notice. */
  private renderCheckout() {
    const button = this.root.querySelector<HTMLButtonElement>("#checkout-button");
    const notice = this.root.querySelector<HTMLButtonElement>("#touched-notice");
    if (!button || !notice) return;
    const workspaces = this.workspaces;
    const path = this.targetPath ?? "";
    const label = workspaces ? workspaceLabel(path, workspaces.checkouts) : this.bootstrap.checkout?.label ?? "";
    button.hidden = !workspaces || workspaces.checkouts.length < 2;
    button.querySelector("#checkout-label")!.textContent = label;
    button.title = path;
    button.setAttribute("aria-label", `Reviewed checkout: ${label}`);
    button.disabled = this.loadingRange !== undefined || this.agentBusy || this.switchingCheckout;
    const unseen = this.touched.unseen;
    const text = touchedNotice(unseen);
    notice.hidden = button.hidden || unseen.length === 0;
    notice.disabled = button.disabled;
    notice.title = text;
    notice.setAttribute("aria-label", text);
    notice.querySelector("span")!.textContent = text;
    notice.querySelector("strong")!.textContent = unseen.length === 1 ? "Review" : "Show";
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
        review = await this.api.review(this.session);
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
   * Another checkout starts afresh from its default range.
   */
  private async installSnapshot(review: ReviewSession, epoch: number) {
    const previous = this.state;
    const previousRange = this.page?.selected_range ?? review.default_range;
    const sameCheckout = review.checkout?.path === this.bootstrap.checkout?.path;
    const sameTimeline = sameCheckout
      && JSON.stringify(review.range_targets) === JSON.stringify(this.bootstrap.range_targets);
    let page = review.page;
    if (sameTimeline && !rangesEqual(previousRange, page.selected_range)) {
      page = await this.api.loadRange(review.generation, previousRange).catch(() => review.page);
      if (epoch !== this.sessionEpoch || this.disposed) return;
    } else if (!sameCheckout && !rangesEqual(review.default_range, page.selected_range)) {
      page = await this.api.loadRange(review.generation, review.default_range).catch(() => review.page);
      if (epoch !== this.sessionEpoch || this.disposed) return;
    }
    const sameRange = sameCheckout && rangesEqual(page.selected_range, previousRange);
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
      this.state = createReviewState({ ...review, page });
    }
    this.bootstrap = review;
    this.overview.restore();
    if (!sameTimeline) this.rangeDialog.replaceTimeline();
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

  private applySettings(rebuildDiff: boolean) {
    this.fileTree.syncAppearance();
    if (rebuildDiff && this.page) {
      this.diffView.updateTheme();
      this.overview.renderFrame();
      this.diffView.render(this.items);
    }
    if (this.draft?.tab === "preview") void this.commentEditor.renderDraftPreview();
  }

  private selectTab(name: string) {
    if (name !== "changes") this.search.close();
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
    if (name === "changes") this.diffView.viewer?.render(true);
  }

  private refreshItem(itemId: string) {
    const item = this.items.find((candidate) => candidate.id === itemId);
    if (!item) return;
    item.annotations = this.annotationsForItem(itemId);
    this.diffView.updateItem(item);
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
      return this.commentEditor.composerElement(annotation.metadata.draft);
    }
    if (annotation.metadata.kind === "question") {
      return this.questionThreads.threadElement(annotation.metadata.thread);
    }
    return this.commentEditor.commentElement(annotation.metadata.comment);
  }

  private async renderMarkdown(container: HTMLElement, body: string) {
    const theme = activeSyntaxTheme(this.settings.current, this.host.theme() === "dark");
    await renderMarkdown(container, body, theme);
  }

  private setRangeLoading(range: ReviewRange) {
    this.loadingRange = range;
    this.showLoading(`Loading ${rangeLabel(this.bootstrap.range_targets, range)}`, "Capturing an immutable diff for this review.");
    this.syncAgentControls();
  }

  private setRangeReady() {
    this.loadingRange = undefined;
    this.syncAgentControls();
    this.hideLoading();
  }

  private showLoading(title: string, detail: string) {
    const state = this.root.querySelector<HTMLElement>("#scope-state");
    if (!state) return;
    state.className = "scope-state loading";
    state.innerHTML = `<div class="scope-spinner"></div><strong>${escapeHtml(title)}</strong><span>${escapeHtml(detail)}</span>`;
    state.hidden = false;
  }

  private hideLoading() {
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
    this.renderCheckout();
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
        && this.overview.current(rangeKey(this.page?.selected_range ?? this.bootstrap.default_range))?.instructions
          === overviewDraft.trim();
      button.disabled = agentUnavailable || (button.id === "ai-review" && (this.aiReview.pending || this.refresher.stale))
        || (button.matches("[data-edit-overview], [data-generate-overview], [data-retry-overview]")
          && this.overview.loading)
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
      this.status.showInlineError("Save or discard the open comment draft before sending the review.");
      this.commentEditor.focusDraft();
      return;
    }
    if (this.refresher.stale) {
      await this.refreshReview();
      if (this.refresher.stale || this.disposed) {
        this.status.showInlineError("The workspace changed. Wait for the diff to update, then send again.", () => void this.sendToChat(decision));
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
      this.status.showInlineError(errorMessage(error), () => void this.sendToChat(decision));
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
    this.fileTree.refreshDecorations();
    this.commentEditor.renderList();
    this.syncAgentControls();
    this.status.showNotice("Sent to chat.");
  }
}

function outdatedNote(comment: CommentMetadata) {
  const lines = formatRange(comment.start_line, comment.end_line);
  return "Comment on " + comment.path + " (lines " + lines + " of an earlier version, since changed): " + comment.body;
}
