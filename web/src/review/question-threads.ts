import { ProtocolMismatch, errorCode, errorMessage, type ReviewApi } from "./review-api";
import { icon } from "./icons";
import { escapeHtml, formatRange } from "./markup";
import type { ReviewPage } from "./protocol";
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
import { rangesEqual } from "./range-selection";
import type { FeedbackState } from "./review-state";
import type { ReviewStatus } from "./review-status";

type QuestionOperation = { threadId: string; request: number; operationId: string };

export type QuestionThreadsDeps = {
  api: ReviewApi;
  status: ReviewStatus;
  session(): string | null;
  /** Advances whenever the active session changes. */
  sessionEpoch(): number;
  generation(): number;
  page(): ReviewPage | undefined;
  /** The feedback for the installed page, whose open draft can become a question. */
  feedback(): FeedbackState;
  /** The question threads of the installed page. */
  questions(): QuestionThread[];
  /** The question threads of every page in the snapshot. */
  allQuestions(): QuestionThread[];
  agentUnavailable(): boolean;
  refreshItem(itemId: string): void;
  syncAgentControls(): void;
  markStale(): void;
  recordActionError(error: unknown): void;
  clearSelectedLines(): void;
  focusDraft(): void;
  renderMarkdown(container: HTMLElement, body: string): Promise<void>;
};

/**
 * Questions the reviewer asks the agent about lines of the diff. Each thread has at most one
 * operation in flight. Tact stores the operation, so a lost connection or a reload reconciles by
 * polling for its outcome instead of asking again.
 */
export class QuestionThreads {
  private questionRequest = 0;
  private questionPollTimer?: number;
  private pollingQuestions = false;
  private readonly questionOperations = new Map<string, QuestionOperation>();
  private readonly questionsToPoll = new Set<string>();

  constructor(
    private readonly root: HTMLElement,
    private readonly deps: QuestionThreadsDeps,
  ) {}

  /** Whether any question is waiting for the agent. */
  get busy() {
    return this.questionOperations.size > 0;
  }

  /** Forgets the previous session's questions; their responses are ignored when they arrive. */
  reset() {
    this.questionRequest++;
    this.questionOperations.clear();
    this.questionsToPoll.clear();
    window.clearTimeout(this.questionPollTimer);
    this.questionPollTimer = undefined;
  }

  dispose() {
    window.clearTimeout(this.questionPollTimer);
  }

  resume(thread: QuestionThread) {
    if (thread.turn.kind !== "asking" || this.questionOperations.has(thread.id)) return;
    this.questionOperations.set(thread.id, {
      threadId: thread.id,
      request: thread.turn.request,
      operationId: thread.turn.operationId,
    });
    this.questionsToPoll.add(thread.id);
    if (thread.itemId) this.deps.refreshItem(thread.itemId);
    this.deps.syncAgentControls();
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
    const session = this.deps.session();
    if (!session) return;
    const epoch = this.deps.sessionEpoch();
    this.pollingQuestions = true;
    try {
      const payload = await this.deps.api.questions(session, this.deps.generation());
      if (epoch !== this.deps.sessionEpoch()) return;
      if (payload.generation !== this.deps.generation()) {
        this.questionOperations.clear();
        this.questionsToPoll.clear();
        this.deps.markStale();
        this.deps.syncAgentControls();
        return;
      }
      for (const threadId of [...this.questionsToPoll]) {
        const operation = this.questionOperations.get(threadId);
        if (!operation) {
          this.questionsToPoll.delete(threadId);
          continue;
        }
        const thread = this.deps.allQuestions().find((candidate) => candidate.id === threadId);
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
          this.deps.status.announceAgent("Tact answered the question.");
        } else if (stored.status === "cancelled") {
          cancelQuestion(thread, operation.request);
          this.deps.status.announceAgent("Question cancelled.");
        } else {
          failQuestion(thread, operation.request, stored.error ?? "Unknown error");
          this.deps.status.announceAgent(`Tact could not answer the question: ${stored.error ?? "Unknown error"}`);
        }
        this.questionOperations.delete(threadId);
        this.questionsToPoll.delete(threadId);
        if (thread.itemId) this.deps.refreshItem(thread.itemId);
      }
      this.deps.syncAgentControls();
    } catch (error) {
      if (errorCode(error) === "stale_snapshot") {
        this.questionOperations.clear();
        this.questionsToPoll.clear();
        this.deps.markStale();
        this.deps.syncAgentControls();
      }
      // Other polling failures are transient; the stored operation remains authoritative.
    } finally {
      this.pollingQuestions = false;
      if (this.questionsToPoll.size > 0) this.scheduleQuestionPoll();
    }
  }

  askDraft() {
    const draft = this.deps.feedback().draft;
    const page = this.deps.page();
    if (!draft || draft.editingId !== undefined || !page || this.deps.agentUnavailable()) return;
    if (!draft.body.trim()) {
      this.deps.focusDraft();
      return;
    }
    const validationError = questionValidationError([], draft.body);
    if (validationError) {
      this.deps.status.showInlineError(validationError);
      this.deps.focusDraft();
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
    this.deps.questions().push(thread);
    this.deps.feedback().draft = undefined;
    this.deps.clearSelectedLines();
    this.startQuestion(thread, request, operationId, page);
  }

  private askFollowUp(thread: QuestionThread) {
    const page = this.deps.page();
    if (!page || this.deps.agentUnavailable() || thread.turn.kind !== "idle") return;
    const validationError = questionValidationError(thread.messages, thread.draft);
    if (validationError) {
      thread.validationError = validationError;
      this.deps.refreshItem(thread.itemId);
      queueMicrotask(() => this.focusThreadDraft(thread));
      return;
    }
    const request = ++this.questionRequest;
    const operationId = crypto.randomUUID();
    if (!beginFollowUp(thread, request, operationId)) return;
    this.startQuestion(thread, request, operationId, page);
  }

  private retryThreadQuestion(thread: QuestionThread) {
    const page = this.deps.page();
    if (!page || this.deps.agentUnavailable() || this.questionOperations.has(thread.id)) return;
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
    const session = this.deps.session();
    if (!session) return;
    this.questionOperations.set(thread.id, {
      threadId: thread.id,
      request,
      operationId,
    });
    this.deps.status.announceAgent(`Tact is answering a question about ${thread.path}, ${formatRange(thread.startLine, thread.endLine)}.`);
    this.deps.refreshItem(thread.itemId);
    this.deps.syncAgentControls();
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
    const epoch = this.deps.sessionEpoch();
    try {
      const payload = await this.deps.api.question(session, {
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
      if (epoch !== this.deps.sessionEpoch()) return;
      if (payload.generation !== page.generation
        || !rangesEqual(payload.selected_range, page.selected_range)
        || !payload.answer.trim()) {
        throw new ProtocolMismatch("Tact returned an invalid answer for this review range.");
      }
      finishQuestion(thread, request, payload.answer);
      this.deps.status.announceAgent(`Tact answered the question about ${thread.path}, ${formatRange(thread.startLine, thread.endLine)}.`);
    } catch (error) {
      this.deps.recordActionError(error);
      if (errorCode(error) === "network_error") {
        reconcileWithServer = true;
        this.deps.status.announceAgent("Reconnecting to the question…");
        this.questionsToPoll.add(thread.id);
        this.scheduleQuestionPoll();
        return;
      }
      const cancelled = errorCode(error) === "operation_cancelled";
      if (cancelled) cancelQuestion(thread, request);
      else failQuestion(thread, request, errorMessage(error));
      this.deps.status.announceAgent(
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
      this.deps.refreshItem(thread.itemId);
      this.deps.syncAgentControls();
    }
  }

  private async stopQuestion(thread: QuestionThread) {
    const operation = this.questionOperations.get(thread.id);
    const page = this.deps.page();
    const session = this.deps.session();
    if (!page || !session || !operation || operation.threadId !== thread.id) return;
    if (!beginStopping(thread, operation.request)) return;
    this.deps.status.announceAgent("Stopping the question…");
    this.deps.refreshItem(thread.itemId);
    try {
      await this.deps.api.cancelQuestion(session, {
        operation_id: operation.operationId,
        generation: page.generation,
        range: page.selected_range,
      });
    } catch (error) {
      stopFailed(thread, operation.request);
      this.deps.refreshItem(thread.itemId);
      this.deps.status.showInlineError(`Could not stop the question: ${errorMessage(error)}`);
      this.deps.status.announceAgent(`Could not stop the question: ${errorMessage(error)}`);
    }
  }

  threadElement(thread: QuestionThread) {
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
      if (body) void this.deps.renderMarkdown(body, message.body);
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
      turn.innerHTML = `<span>${escapeHtml(thread.turn.message)}</span><button class="button" data-agent-action data-thread-retry ${this.deps.agentUnavailable() ? "disabled" : ""}>Try again</button>`;
      turn.querySelector("[data-thread-retry]")?.addEventListener("click", () => this.retryThreadQuestion(thread));
      return element;
    }
    if (thread.turn.kind === "cancelled") {
      turn.className = "agent-thread-turn cancelled";
      turn.setAttribute("role", "status");
      turn.innerHTML = `<span>Question cancelled.</span><button class="button" data-agent-action data-thread-retry ${this.deps.agentUnavailable() ? "disabled" : ""}>Ask again</button>`;
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
      <textarea id="${inputId}" data-thread-input aria-describedby="${contextId} ${linesId} thread-${thread.id}-validation" rows="3" placeholder="Ask about this code" ${this.deps.agentUnavailable() ? "disabled" : ""}></textarea>
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
        if (ask) ask.disabled = !textarea.value.trim() || this.deps.agentUnavailable();
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
}
