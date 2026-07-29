import {
  CodeView,
  parsePatchFiles,
  type CodeViewDiffItem,
  type CodeViewLineSelection,
  type DiffLineAnnotation,
  type FileDiffMetadata,
} from "@pierre/diffs";
import { FileTree, type GitStatus, type GitStatusEntry } from "@pierre/trees";
import "./styles.css";

type ReviewPage = {
  title: string;
  overview_html: string;
  patch: string;
  repository: string;
  scope: string;
  base?: string;
};

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

const root = document.querySelector<HTMLElement>("#app");
if (!root) throw new Error("review root is missing");

void start();

async function start() {
  try {
    const response = await fetch("./api/review");
    if (!response.ok) throw new Error(`review request failed: ${response.status}`);
    const review = (await response.json()) as ReviewPage;
    new ReviewApp(root, review).render();
  } catch (error) {
    root.innerHTML = `<div class="fatal"><p>Could not load this review.</p><small>${escapeHtml(String(error))}</small></div>`;
  }
}

class ReviewApp {
  private readonly files: FileDiffMetadata[];
  private readonly items: CodeViewDiffItem<CommentMetadata>[];
  private readonly pathToItem = new Map<string, string>();
  private readonly comments: CommentMetadata[] = [];
  private selection: CodeViewLineSelection | null = null;
  private nextCommentId = 1;
  private submitted = false;
  private viewer?: CodeView<CommentMetadata>;
  private tree?: FileTree;

  constructor(
    private readonly root: HTMLElement,
    private readonly review: ReviewPage,
  ) {
    this.files = parsePatchFiles(review.patch, "tact-review", true).flatMap(
      (patch) => patch.files,
    );
    this.items = this.files.map((file, index) => {
      const id = `${index}:${file.name}`;
      this.pathToItem.set(file.name, id);
      return { id, type: "diff", fileDiff: file, annotations: [], version: 1 };
    });
  }

  render() {
    document.title = `${this.review.title} · Tact`;
    const stats = changeStats(this.files);
    this.root.innerHTML = `
      <div class="review-shell">
        <header class="topbar">
          <div class="identity">
            <span class="mark" aria-hidden="true">T</span>
            <div>
              <h1>${escapeHtml(this.review.title)}</h1>
              <p>${escapeHtml(this.review.repository)} <span>·</span> ${escapeHtml(this.review.scope)}</p>
            </div>
          </div>
          <div class="change-stats" aria-label="Change statistics">
            <span>${this.files.length} ${this.files.length === 1 ? "file" : "files"}</span>
            <strong class="add">+${stats.additions}</strong>
            <strong class="del">−${stats.deletions}</strong>
          </div>
        </header>
        <nav class="tabs" aria-label="Review sections">
          <button class="tab active" data-tab="overview">Overview</button>
          <button class="tab" data-tab="changes">Changes <span>${this.files.length}</span></button>
        </nav>
        <section class="panel overview-panel active" data-panel="overview">
          <iframe class="overview" title="Agent overview" sandbox=""></iframe>
        </section>
        <section class="panel changes-panel" data-panel="changes">
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
        <div id="comment-composer" class="comment-composer" hidden>
          <div class="comment-anchor" id="comment-anchor"></div>
          <textarea id="comment-body" rows="4" placeholder="Leave a comment"></textarea>
          <div class="composer-actions">
            <button class="text-button" id="cancel-comment">Cancel</button>
            <button class="button primary" id="save-comment">Add comment</button>
          </div>
        </div>
        <div id="finished" class="finished" hidden>
          <div><span>✓</span><h2>Review submitted</h2><p>You can return to Tact.</p></div>
        </div>
      </div>`;

    this.renderOverview();
    this.renderDiff();
    this.renderTree();
    this.bindEvents();
  }

  private renderOverview() {
    const frame = this.root.querySelector<HTMLIFrameElement>(".overview");
    if (!frame) return;
    frame.srcdoc = `<!doctype html><html><head><meta charset="utf-8"><meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; img-src data:"><style>${overviewStyles()}</style></head><body>${this.review.overview_html}</body></html>`;
  }

  private renderDiff() {
    const container = this.root.querySelector<HTMLElement>("#diff-view");
    if (!container) return;
    this.viewer = new CodeView<CommentMetadata>({
      diffStyle: "unified",
      enableLineSelection: true,
      stickyHeaders: true,
      lineHoverHighlight: "both",
      onSelectedLinesChange: (selection) => this.openCommentComposer(selection),
      renderAnnotation: (annotation) => this.annotationElement(annotation),
    });
    this.viewer.setup(container);
    this.viewer.setItems(this.items);
  }

  private renderTree() {
    const container = this.root.querySelector<HTMLElement>("#file-tree");
    if (!container) return;
    this.tree = new FileTree({
      paths: this.files.map((file) => file.name),
      flattenEmptyDirectories: true,
      initialExpansion: "open",
      density: "compact",
      icons: "minimal",
      gitStatus: this.files.map((file) => ({
        path: file.name,
        status: treeStatus(file.type),
      })) satisfies GitStatusEntry[],
      onSelectionChange: (paths) => {
        const path = paths.at(-1);
        const id = path ? this.pathToItem.get(path) : undefined;
        if (id) this.viewer?.scrollTo({ type: "item", id, align: "start", behavior: "smooth-auto" });
      },
    });
    this.tree.render({ containerWrapper: container });
  }

  private bindEvents() {
    for (const tab of this.root.querySelectorAll<HTMLButtonElement>("[data-tab]")) {
      tab.addEventListener("click", () => this.selectTab(tab.dataset.tab ?? "overview"));
    }
    this.root.querySelector("#cancel-comment")?.addEventListener("click", () => this.closeCommentComposer());
    this.root.querySelector("#save-comment")?.addEventListener("click", () => this.saveComment());
    this.root.querySelector("#cancel-review")?.addEventListener("click", () => void this.cancel());
    for (const button of this.root.querySelectorAll<HTMLButtonElement>("[data-decision]")) {
      button.addEventListener("click", () => void this.submit(button.dataset.decision as ReviewDecision["decision"]));
    }
    window.addEventListener("pagehide", () => {
      if (!this.submitted) navigator.sendBeacon("./api/cancel");
    });
  }

  private selectTab(name: string) {
    for (const tab of this.root.querySelectorAll<HTMLElement>("[data-tab]")) {
      tab.classList.toggle("active", tab.dataset.tab === name);
    }
    for (const panel of this.root.querySelectorAll<HTMLElement>("[data-panel]")) {
      panel.classList.toggle("active", panel.dataset.panel === name);
    }
    if (name === "changes") this.viewer?.render(true);
  }

  private openCommentComposer(selection: CodeViewLineSelection | null) {
    if (!selection) return;
    const side = selection.range.side ?? "additions";
    const endSide = selection.range.endSide ?? side;
    if (side !== endSide) return;
    this.selection = selection;
    const item = this.items.find((candidate) => candidate.id === selection.id);
    if (!item) return;
    const composer = this.root.querySelector<HTMLElement>("#comment-composer");
    const anchor = this.root.querySelector<HTMLElement>("#comment-anchor");
    if (!composer || !anchor) return;
    anchor.textContent = `${item.fileDiff.name}:${formatRange(selection.range.start, selection.range.end)}`;
    composer.hidden = false;
    this.root.querySelector<HTMLTextAreaElement>("#comment-body")?.focus();
  }

  private closeCommentComposer() {
    this.selection = null;
    this.viewer?.clearSelectedLines();
    const composer = this.root.querySelector<HTMLElement>("#comment-composer");
    if (composer) composer.hidden = true;
    const body = this.root.querySelector<HTMLTextAreaElement>("#comment-body");
    if (body) body.value = "";
  }

  private saveComment() {
    if (!this.selection) return;
    const body = this.root.querySelector<HTMLTextAreaElement>("#comment-body")?.value.trim();
    if (!body) return;
    const item = this.items.find((candidate) => candidate.id === this.selection?.id);
    if (!item) return;
    const range = this.selection.range;
    const comment: CommentMetadata = {
      id: this.nextCommentId++,
      itemId: item.id,
      path: item.fileDiff.name,
      side: range.side ?? "additions",
      start_line: Math.min(range.start, range.end),
      end_line: Math.max(range.start, range.end),
      body,
    };
    this.comments.push(comment);
    this.refreshItem(item.id);
    this.renderCommentList();
    this.closeCommentComposer();
  }

  private removeComment(id: number) {
    const index = this.comments.findIndex((comment) => comment.id === id);
    if (index < 0) return;
    const [comment] = this.comments.splice(index, 1);
    this.refreshItem(comment.itemId);
    this.renderCommentList();
  }

  private refreshItem(itemId: string) {
    const item = this.items.find((candidate) => candidate.id === itemId);
    if (!item) return;
    item.annotations = this.comments
      .filter((comment) => comment.itemId === itemId)
      .map((comment) => ({
        side: comment.side,
        lineNumber: comment.start_line,
        metadata: comment,
      }));
    item.version = (item.version ?? 0) + 1;
    this.viewer?.updateItem(item);
  }

  private annotationElement(annotation: DiffLineAnnotation<CommentMetadata>) {
    const element = document.createElement("div");
    element.className = "diff-comment";
    const range = document.createElement("span");
    range.textContent = `Lines ${formatRange(annotation.metadata.start_line, annotation.metadata.end_line)}`;
    const body = document.createElement("p");
    body.textContent = annotation.metadata.body;
    element.append(range, body);
    return element;
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
      const button = document.createElement("button");
      button.className = "comment-link";
      button.innerHTML = `<strong>${escapeHtml(comment.path)}</strong><span>${formatRange(comment.start_line, comment.end_line)} · ${comment.side === "additions" ? "new" : "old"}</span><p>${escapeHtml(comment.body)}</p><i aria-label="Remove comment">×</i>`;
      button.addEventListener("click", (event) => {
        if ((event.target as HTMLElement).tagName === "I") {
          this.removeComment(comment.id);
          return;
        }
        this.viewer?.scrollTo({
          type: "range",
          id: comment.itemId,
          range: { start: comment.start_line, end: comment.end_line, side: comment.side, endSide: comment.side },
          align: "center",
          behavior: "smooth-auto",
        });
      });
      list.append(button);
    }
  }

  private async submit(decision: ReviewDecision["decision"]) {
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

function formatRange(start: number, end: number) {
  return start === end ? String(start) : `${Math.min(start, end)}–${Math.max(start, end)}`;
}

function escapeHtml(value: string) {
  return value.replace(/[&<>'"]/g, (character) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", "'": "&#39;", '"': "&quot;",
  })[character] ?? character);
}

function overviewStyles() {
  return `
    :root { color-scheme: light dark; font: 15px/1.65 Inter, ui-sans-serif, system-ui, sans-serif; color: light-dark(#242624, #e8eae8); }
    body { max-width: 780px; margin: 0 auto; padding: 56px 42px 120px; }
    h1, h2, h3 { line-height: 1.2; letter-spacing: -.025em; margin: 2em 0 .6em; }
    h1:first-child, h2:first-child { margin-top: 0; }
    p, ul, ol, pre { margin: 0 0 1.2em; }
    code { font: 13px ui-monospace, SFMono-Regular, monospace; background: light-dark(#f0f1ef, #282b29); padding: 2px 5px; border-radius: 5px; }
    pre { padding: 18px; overflow: auto; border: 1px solid light-dark(#e1e3df, #353936); border-radius: 10px; }
    pre code { padding: 0; background: none; }
    a { color: inherit; }
    strong { font-weight: 650; }
    blockquote { margin: 1.5em 0; padding-left: 18px; border-left: 2px solid #8ca388; color: light-dark(#5d625d, #abb0ab); }
  `;
}
