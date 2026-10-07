import type { ReviewDiffItem } from "./annotations";
import { annotationPath } from "./comment-state";
import { errorMessage, type ReviewApi } from "./review-api";
import type { ReviewPage } from "./protocol";
import { rangesEqual } from "./range-selection";
import type { CommentMetadata } from "./review-state";
import type { ReviewStatus } from "./review-status";

export type AiReviewDeps = {
  api: ReviewApi;
  status: ReviewStatus;
  session(): string | null;
  page(): ReviewPage | undefined;
  items(): readonly ReviewDiffItem[];
  /** The installed page's pending comments, which receive the findings. */
  comments(): CommentMetadata[];
  nextCommentId(): number;
  agentUnavailable(): boolean;
  /** Whether the workspace changed since the installed snapshot was taken. */
  stale(): boolean;
  refreshItem(itemId: string): void;
  refreshTreeDecorations(): void;
  renderCommentList(): void;
  selectTab(name: "changes"): void;
  syncAgentControls(): void;
  recordActionError(error: unknown): void;
};

/**
 * The AI review action. The agent reviews the installed page and its findings become ordinary
 * pending comments, which the reviewer can edit or delete before sending; a finding that repeats
 * an existing comment or names a file outside the page is skipped.
 */
export class AiReview {
  private aiReviewRequest = 0;
  private aiReviewPending = false;

  constructor(
    private readonly root: HTMLElement,
    private readonly deps: AiReviewDeps,
  ) {}

  get pending() {
    return this.aiReviewPending;
  }

  /** Forgets the previous session's review; its response is ignored when it arrives. */
  reset() {
    this.aiReviewRequest++;
    this.aiReviewPending = false;
  }

  async run() {
    const page = this.deps.page();
    const session = this.deps.session();
    if (!page || !session || this.aiReviewPending || this.deps.agentUnavailable() || this.deps.stale()) return;
    const request = ++this.aiReviewRequest;
    this.aiReviewPending = true;
    const button = this.root.querySelector<HTMLButtonElement>("#ai-review");
    if (button) {
      button.innerHTML = '<span class="activity-spinner" aria-hidden="true"></span>Reviewing…';
      button.setAttribute("aria-busy", "true");
    }
    this.deps.status.announceAgent("Tact is reviewing the selected diff.");
    this.deps.status.clearInlineError();
    this.deps.syncAgentControls();
    try {
      const result = await this.deps.api.aiReview(session, page);
      if (request !== this.aiReviewRequest) return;
      if (result.generation !== page.generation
        || !rangesEqual(result.selected_range, page.selected_range)
        || this.deps.page() !== page) throw new Error("Tact returned findings for a different review range.");
      let added = 0;
      for (const finding of result.comments) {
        const item = this.deps.items().find((candidate) =>
          annotationPath(candidate.fileDiff, finding.side) === finding.path);
        if (!item || !finding.body.trim()
          || this.deps.comments().some((comment) => comment.path === finding.path
            && comment.side === finding.side && comment.start_line === finding.start_line
            && comment.end_line === finding.end_line && comment.body === finding.body)) continue;
        this.deps.comments().push({ ...finding, id: this.deps.nextCommentId(), itemId: item.id });
        this.deps.refreshItem(item.id);
        added++;
      }
      this.deps.refreshTreeDecorations();
      this.deps.renderCommentList();
      this.deps.selectTab("changes");
      this.deps.status.announceAgent(added ? `Tact added ${added} inline review comments.` : "Tact found no actionable issues.");
      this.deps.status.showNotice(added ? `Added ${added} AI review comments. Review and edit them before sending.` : "AI review found no actionable issues.");
    } catch (error) {
      this.deps.recordActionError(error);
      this.deps.status.showInlineError(errorMessage(error), this.deps.stale() ? undefined : () => void this.run());
    } finally {
      if (request === this.aiReviewRequest) {
        this.aiReviewPending = false;
        if (button) {
          button.textContent = "AI review";
          button.removeAttribute("aria-busy");
        }
        this.deps.syncAgentControls();
      }
    }
  }
}
