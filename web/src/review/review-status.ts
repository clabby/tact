import { escapeHtml } from "./markup";

/**
 * The panel's status line, which shows transient notices and actionable errors, and the live
 * region that announces agent activity to screen readers.
 */
export class ReviewStatus {
  constructor(private readonly root: HTMLElement) {}

  showNotice(message: string) {
    const status = this.root.querySelector<HTMLElement>("#review-status");
    if (!status) return;
    status.className = "review-status notice";
    status.setAttribute("role", "status");
    status.textContent = message;
    window.setTimeout(() => {
      if (status.textContent === message) this.clearStatus();
    }, 4000);
  }

  showInlineError(message: string, retry?: () => void) {
    const status = this.root.querySelector<HTMLElement>("#review-status");
    if (!status) return;
    status.className = "review-status error";
    status.setAttribute("role", "alert");
    status.innerHTML = `${escapeHtml(message)}${retry ? ` <button class="text-button" data-status-retry>Retry</button>` : ""}`;
    if (retry) status.querySelector("[data-status-retry]")?.addEventListener("click", retry);
  }

  clearInlineError() {
    if (this.root.querySelector("#review-status.error")) this.clearStatus();
  }

  private clearStatus() {
    const status = this.root.querySelector<HTMLElement>("#review-status");
    if (!status) return;
    status.className = "review-status";
    status.textContent = "";
  }

  /** Tells screen readers what the agent is doing. */
  announceAgent(message: string) {
    const announcement = this.root.querySelector<HTMLElement>("#agent-announcement");
    if (!announcement) return;
    announcement.textContent = "";
    queueMicrotask(() => { announcement.textContent = message; });
  }
}
