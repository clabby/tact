import type { CodeView, CodeViewLineSelection } from "@pierre/diffs";
import type { AnnotationMetadata, ReviewDiffItem } from "./annotations";
import { annotationPath } from "./comment-state";
import { icon, type FormattingIconName } from "./icons";
import { escapeHtml, formatRange, severityBadge } from "./markup";
import type { CommentDraft, CommentMetadata, FeedbackState } from "./review-state";
import type { ReviewStatus } from "./review-status";

export type CommentEditorDeps = {
  status: ReviewStatus;
  /** The feedback for the installed page, which holds its comments and the open draft. */
  feedback(): FeedbackState;
  items(): readonly ReviewDiffItem[];
  viewer(): CodeView<AnnotationMetadata> | undefined;
  commentsLocked(): boolean;
  agentUnavailable(): boolean;
  nextCommentId(): number;
  refreshItem(itemId: string): void;
  refreshTreeDecorations(): void;
  clearSelectedLines(): void;
  selectTab(name: "changes"): void;
  /** Turns the open draft into a question for the agent. */
  askQuestion(): void;
  renderMarkdown(container: HTMLElement, body: string): Promise<void>;
};

/**
 * The reviewer's own comments: the inline editor for the single open draft, the rendered comments
 * in the diff, and the comment list in the sidebar. Only one draft may be open at a time, so the
 * reviewer saves or discards it before starting another.
 */
export class CommentEditor {
  constructor(
    private readonly root: HTMLElement,
    private readonly deps: CommentEditorDeps,
  ) {}

  private get comments() { return this.deps.feedback().comments; }
  private get draft() { return this.deps.feedback().draft; }
  private set draft(value: CommentDraft | undefined) { this.deps.feedback().draft = value; }

  open(selection: CodeViewLineSelection | null) {
    if (!selection || this.deps.commentsLocked()) return;
    const side = selection.range.side ?? "additions";
    const endSide = selection.range.endSide ?? side;
    if (side !== endSide) return;
    const item = this.deps.items().find((candidate) => candidate.id === selection.id);
    if (!item) return;
    if (this.draft) {
      this.deps.status.showInlineError("Save or discard the open comment draft before starting another comment.");
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
    if (previousItemId && previousItemId !== item.id) this.deps.refreshItem(previousItemId);
    this.deps.refreshItem(item.id);
    queueMicrotask(() => this.focusDraft());
  }

  private editComment(comment: CommentMetadata) {
    if (this.deps.commentsLocked() || comment.outdated) return;
    if (this.draft) {
      if (this.draft.editingId === comment.id) {
        this.focusDraft();
        return;
      }
      this.deps.status.showInlineError("Save or discard the open comment draft before editing another comment.");
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
    if (previousItemId && previousItemId !== comment.itemId) this.deps.refreshItem(previousItemId);
    this.deps.refreshItem(comment.itemId);
    this.deps.selectTab("changes");
    this.deps.viewer()?.scrollTo({
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
    this.deps.clearSelectedLines();
    if (itemId) this.deps.refreshItem(itemId);
    this.deps.status.clearInlineError();
  }

  private saveComment() {
    const draft = this.draft;
    if (!draft || this.deps.commentsLocked()) return;
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
        id: this.deps.nextCommentId(),
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
    this.deps.clearSelectedLines();
    this.deps.refreshItem(itemId);
    this.deps.refreshTreeDecorations();
    this.renderList();
    this.deps.status.clearInlineError();
  }

  private removeComment(id: number) {
    if (this.deps.commentsLocked()) return;
    const index = this.comments.findIndex((comment) => comment.id === id);
    if (index < 0) return;
    const [comment] = this.comments.splice(index, 1);
    if (this.draft?.editingId === id) this.draft = undefined;
    this.deps.refreshItem(comment.itemId);
    this.deps.refreshTreeDecorations();
    this.renderList();
  }

  composerElement(draft: CommentDraft) {
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
        <div class="segmented compact editor-tabs">
          <button id="${editorId}-comment" role="tab" aria-controls="${editorId}-input" aria-selected="${draft.tab === "comment"}" tabindex="${draft.tab === "comment" ? "0" : "-1"}" class="segment ${draft.tab === "comment" ? "active" : ""}" data-editor-tab="comment">Comment</button>
          <button id="${editorId}-preview-tab" role="tab" aria-controls="${editorId}-preview" aria-selected="${draft.tab === "preview"}" tabindex="${draft.tab === "preview" ? "0" : "-1"}" class="segment ${draft.tab === "preview" ? "active" : ""}" data-editor-tab="preview">Preview</button>
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
          ${draft.editingId === undefined ? `<button class="button" data-agent-action data-comment-action="ask" ${draft.body.trim() && !this.deps.agentUnavailable() ? "" : "disabled"}>Ask <span aria-hidden="true">✨</span></button>` : ""}
          <button class="button primary" data-comment-action="save" ${draft.body.trim() && !this.deps.commentsLocked() ? "" : "disabled"}>${draft.editingId === undefined ? "Add comment" : "Save changes"}</button>
        </div>
      </div>`;

    const textarea = element.querySelector<HTMLTextAreaElement>(".comment-input");
    const saveButton = element.querySelector<HTMLButtonElement>("[data-comment-action=save]");
    const askButton = element.querySelector<HTMLButtonElement>("[data-comment-action=ask]");
    if (textarea) {
      textarea.value = draft.body;
      textarea.addEventListener("input", () => {
        if (this.draft === draft) draft.body = textarea.value;
        if (saveButton) saveButton.disabled = textarea.value.trim().length === 0 || this.deps.commentsLocked();
        if (askButton) askButton.disabled = textarea.value.trim().length === 0 || this.deps.agentUnavailable();
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
    element.querySelector("[data-comment-action=ask]")?.addEventListener("click", () => this.deps.askQuestion());
    element.querySelector("[data-comment-action=save]")?.addEventListener("click", () => this.saveComment());
    if (draft.tab === "preview") void this.renderPreviewElement(element, draft.body);
    return element;
  }

  commentElement(comment: CommentMetadata) {
    const element = document.createElement("article");
    element.className = "diff-comment";
    element.innerHTML = `
      <header>
        <span>${severityBadge(comment.body)} Lines ${escapeHtml(formatRange(comment.start_line, comment.end_line))}</span>
        <div>
          <button class="small-icon-button" data-comment-edit aria-label="Edit comment" ${this.deps.commentsLocked() ? "disabled" : ""}>${icon("edit")}</button>
          <button class="small-icon-button danger" data-comment-delete aria-label="Delete comment" ${this.deps.commentsLocked() ? "disabled" : ""}>${icon("trash")}</button>
        </div>
      </header>
      <div class="comment-markdown"></div>`;
    element.querySelector("[data-comment-edit]")?.addEventListener("click", () => this.editComment(comment));
    element.querySelector("[data-comment-delete]")?.addEventListener("click", () => this.removeComment(comment.id));
    const markdown = element.querySelector<HTMLElement>(".comment-markdown");
    if (markdown) void this.deps.renderMarkdown(markdown, comment.body);
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
    if (!preview) return;
    preview.classList.toggle("empty", body.trim() === "");
    if (preview.classList.contains("empty")) {
      preview.textContent = "Nothing to preview yet.";
      return;
    }
    await this.deps.renderMarkdown(preview, body);
  }

  async renderDraftPreview() {
    const editor = this.root.querySelector<HTMLElement>(".inline-comment-editor");
    if (editor && this.draft) await this.renderPreviewElement(editor, this.draft.body);
  }

  focusDraft() {
    const textarea = this.root.querySelector<HTMLTextAreaElement>(".inline-comment-editor .comment-input");
    textarea?.focus();
    textarea?.setSelectionRange(textarea.value.length, textarea.value.length);
  }

  renderList() {
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
          <span>${severityBadge(comment.body)} ${escapeHtml(formatRange(comment.start_line, comment.end_line))} · ${comment.side === "additions" ? "new" : "old"}</span>
          <p>${escapeHtml(comment.body)}</p>
        </button>
        <div class="comment-link-actions">
          <button aria-label="Edit comment" data-edit ${this.deps.commentsLocked() || comment.outdated ? "disabled" : ""}>${icon("edit")}</button>
          <button aria-label="Delete comment" data-delete ${this.deps.commentsLocked() ? "disabled" : ""}>${icon("trash")}</button>
        </div>`;
      item.querySelector(".comment-jump")?.addEventListener("click", () => {
        this.deps.selectTab("changes");
        this.deps.viewer()?.scrollTo({
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
