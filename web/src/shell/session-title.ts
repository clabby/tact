/** How long a generated title takes to reveal; matches `.title-reveal` in shell.css. */
export const TITLE_REVEAL_MS = 380;

/**
 * Renders session titles that may still be generated. A pending title shimmers; when the same
 * session's title stops being pending, the final title reveals once. First renders, rerenders, and
 * ordinary title changes swap instantly. Callers that rebuild their rows mid-reveal get a fresh
 * element that resumes the animation where the previous one was.
 */
export class SessionTitles {
  private readonly pending = new Set<string>();
  /** When each running reveal started. */
  private readonly reveals = new Map<string, number>();

  constructor(private readonly now: () => number = () => performance.now()) {}

  render(element: HTMLElement, session: string, text: string, pending: boolean) {
    element.textContent = text;
    element.classList.toggle("title-pending", pending);
    const now = this.now();
    if (pending) {
      this.pending.add(session);
      this.reveals.delete(session);
    } else if (this.pending.delete(session)) {
      this.reveals.set(session, now);
    }
    const started = this.reveals.get(session);
    const elapsed = started === undefined ? TITLE_REVEAL_MS : now - started;
    if (elapsed >= TITLE_REVEAL_MS) {
      this.reveals.delete(session);
      element.classList.remove("title-reveal");
      element.style.animationDelay = "";
    } else if (!element.classList.contains("title-reveal")) {
      element.style.animationDelay = `${-elapsed}ms`;
      element.classList.add("title-reveal");
    }
  }
}
