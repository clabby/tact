import type { DraftOrigin } from "../core/wire";

export type Timers = {
  set(callback: () => void, ms: number): unknown;
  clear(handle: unknown): void;
};

export const realTimers: Timers = {
  set: (callback, ms) => setTimeout(callback, ms),
  clear: (handle) => clearTimeout(handle as ReturnType<typeof setTimeout>),
};

export type DraftSyncOptions = {
  /** This tab's draft origin, `web:<client>`. */
  origin: string;
  /** Writes the session's shared draft (`set_draft`). Calls are serialized. */
  write(text: string): Promise<{ rev: number }>;
  /** Updates the composer when another writer changes the shared draft. */
  onRemote(text: string, origin: DraftOrigin | null): void;
  onError?(error: unknown): void;
  timers?: Timers;
  delay?: number;
};

/**
 * Keeps one composer and one session's shared draft in step.
 *
 * Local edits are serialized. A successful command returns the revision of its saved text,
 * even when its stream echo is lost. Remote changes wait for local writes and IME composition
 * to drain, then apply only if their revision is newer than the last local save.
 */
export class DraftSync {
  /** What the composer shows. */
  text: string;
  private serverRev: number;
  private serverText: string;
  private timer: unknown = null;
  private inflight = false;
  private retryNeeded = false;
  private rebases = 0;
  private composing = false;

  // Command replies can overtake snapshots. Only stream observations detect counter resets.
  private saved: { rev: number; text: string } | null = null;
  private held: { rev: number; text: string; origin: DraftOrigin | null } | null = null;
  private idleWaiters: (() => void)[] = [];
  private readonly timers: Timers;
  private readonly delay: number;

  constructor(draft: { rev: number; text: string }, private readonly options: DraftSyncOptions) {
    this.text = draft.text;
    this.serverRev = draft.rev;
    this.serverText = draft.text;
    this.timers = options.timers ?? realTimers;
    this.delay = options.delay ?? 75;
  }

  /** Whether local text has not yet been acknowledged by the server. */
  get dirty() {
    return this.timer !== null || this.inflight || this.retryNeeded;
  }

  get isComposing() {
    return this.composing;
  }

  edit(text: string) {
    // Programmatic insertions must match the textarea and server's newline normalization.
    this.text = text.replace(/\r\n?/g, "\n");
    if (this.timer === null) this.timer = this.timers.set(() => this.send(), this.delay);
  }

  compositionStart() {
    this.composing = true;
  }

  compositionEnd(text: string) {
    this.composing = false;
    if (text !== this.text) this.edit(text);
    if (this.retryNeeded) void this.flush();
    this.releaseHeld();
  }

  /** Applies a draft event (or a snapshot's draft, with a null origin). */
  receive(draft: { rev: number; text: string; origin: DraftOrigin | null }) {
    // Reopening a session resets its pane's revision counter. Keep local text and the active
    // write chain, but discard acknowledgements from the previous counter.
    if (draft.origin === null && draft.rev < this.serverRev) {
      this.serverRev = draft.rev;
      this.serverText = draft.text;
      this.saved = null;
      this.held = null;
      this.retryNeeded = this.text !== draft.text;
      this.rebases++;
      if (!this.composing) void this.flush();
      return;
    }
    if (draft.rev <= this.serverRev) return;
    this.serverRev = draft.rev;
    this.serverText = draft.text;
    if (this.saved && draft.rev <= this.saved.rev) return;
    if (draft.origin === this.options.origin || draft.origin === null) {
      if (draft.text === this.text) this.retryNeeded = false;
    }
    if (this.composing || this.dirty) {
      this.held = draft;
      return;
    }
    this.apply(draft);
  }

  /** Sends any pending edit now and resolves once every write has been acknowledged. */
  async flush() {
    while (this.inflight) await new Promise<void>((resolve) => this.idleWaiters.push(resolve));
    if (this.composing) return;
    if (this.timer !== null) {
      this.timers.clear(this.timer);
      this.timer = null;
    }
    if (this.retryNeeded || this.text !== this.latestSaved().text) await this.send();
    this.releaseHeld();
  }

  /** Waits for the visible draft's revision, restarting when a snapshot resets the counter. */
  async settledRev(): Promise<number> {
    for (;;) {
      const rebase = this.rebases;
      await this.flush();
      if (rebase !== this.rebases) continue;
      if (this.composing || this.retryNeeded) throw new Error("The draft has not been saved yet. Try sending again.");
      if (this.dirty) continue;
      const saved = this.latestSaved();
      if (saved.text !== this.text) throw new Error("The draft has not been saved yet. Try sending again.");
      return saved.rev;
    }
  }

  /** Retries a failed write, e.g. after the stream reconnects. */
  retry() {
    if (this.retryNeeded && !this.inflight) void this.send();
  }

  dispose() {
    if (this.timer !== null) this.timers.clear(this.timer);
    this.timer = null;
    for (const waiter of this.idleWaiters.splice(0)) waiter();
  }

  private latestSaved() {
    return this.saved && this.saved.rev > this.serverRev
      ? this.saved
      : { rev: this.serverRev, text: this.serverText };
  }

  private async send() {
    this.timer = null;
    if (this.composing) {
      this.retryNeeded = true;
      return;
    }
    if (this.inflight) return;
    const text = this.text;
    this.inflight = true;
    this.retryNeeded = false;
    const rebase = this.rebases;
    try {
      const { rev } = await this.options.write(text);
      if (rebase === this.rebases) this.saved = { rev, text };
    } catch (error) {
      if (rebase === this.rebases) this.retryNeeded = true;
      this.options.onError?.(error);
    } finally {
      this.inflight = false;
    }
    if (!this.retryNeeded && this.text !== text && this.timer === null) {
      if (this.composing) {
        this.retryNeeded = true;
      } else {
        await this.send();
        return;
      }
    }
    const idle = this.idleWaiters;
    this.idleWaiters = [];
    for (const waiter of idle) waiter();
    this.releaseHeld();
  }

  private releaseHeld() {
    const held = this.held;
    if (!held || this.composing || this.dirty) return;
    this.held = null;
    if (held.rev === this.serverRev && (!this.saved || held.rev > this.saved.rev)) this.apply(held);
  }

  private apply(draft: { text: string; origin: DraftOrigin | null }) {
    if (draft.text === this.text) return;
    this.text = draft.text;
    this.options.onRemote(draft.text, draft.origin);
  }
}
