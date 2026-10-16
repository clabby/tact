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
  write(text: string): Promise<void>;
  /** The shared draft changed under the composer; `text` is now what it must show. */
  onRemote(text: string, origin: DraftOrigin | null): void;
  onError?(error: unknown): void;
  timers?: Timers;
  delay?: number;
};

const ECHO_TIMEOUT_MS = 3000;

/**
 * Keeps one composer and one session's shared draft in step.
 *
 * Local edits are written at most once per `delay`, one request at a time so the server sees them
 * in order. Echoes of this tab's own writes are recognised by text and never applied, so typing is
 * never interrupted by its own past. A remote change is applied at once when the composer is
 * quiet, and otherwise held until the IME composition ends and local writes have drained; it is
 * then applied only if nothing newer reached the server meanwhile (last writer wins).
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
  /** Texts this tab wrote whose echo has not arrived yet, oldest first. */
  private unechoed: string[] = [];
  private held: { rev: number; text: string; origin: DraftOrigin | null } | null = null;
  private echoWaiters: (() => void)[] = [];
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
    return this.timer !== null || this.inflight || this.retryNeeded || this.unechoed.length > 0;
  }

  get isComposing() {
    return this.composing;
  }

  edit(text: string) {
    this.text = text;
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
      this.unechoed = [];
      this.held = null;
      this.retryNeeded = this.text !== draft.text;
      this.rebases++;
      this.drainEchoWaiters();
      if (!this.composing) void this.flush();
      return;
    }
    if (draft.rev <= this.serverRev) return;
    this.serverRev = draft.rev;
    this.serverText = draft.text;
    if (draft.origin === this.options.origin || draft.origin === null) {
      if (draft.text === this.text) this.retryNeeded = false;
      const echoed = this.unechoed.indexOf(draft.text);
      if (echoed >= 0) {
        this.unechoed.splice(0, echoed + 1);
        if (this.unechoed.length === 0) this.drainEchoWaiters();
        this.releaseHeld();
        return;
      }
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
    if (this.timer !== null) {
      this.timers.clear(this.timer);
      this.timer = null;
    }
    if (this.retryNeeded || this.text !== this.lastWritten()) await this.send();
  }

  /** Waits for the visible draft's revision, restarting when a snapshot resets the counter. */
  async settledRev(): Promise<number> {
    for (;;) {
      const rebase = this.rebases;
      await this.flush();
      if (rebase !== this.rebases) continue;
      if (this.unechoed.length > 0) {
        await new Promise<void>((resolve) => {
          const timeout = this.timers.set(resolve, ECHO_TIMEOUT_MS);
          this.echoWaiters.push(() => {
            this.timers.clear(timeout);
            resolve();
          });
        });
      }
      if (rebase !== this.rebases) continue;
      if (this.serverText !== this.text) throw new Error("The draft has not been saved yet. Try sending again.");
      return this.serverRev;
    }
  }

  /** Retries a failed write, e.g. after the stream reconnects. */
  retry() {
    if (this.retryNeeded && !this.inflight) void this.send();
  }

  dispose() {
    if (this.timer !== null) this.timers.clear(this.timer);
    this.timer = null;
    this.drainEchoWaiters();
    for (const waiter of this.idleWaiters.splice(0)) waiter();
  }

  private lastWritten() {
    return this.unechoed.at(-1) ?? this.serverText;
  }

  private async send() {
    this.timer = null;
    if (this.inflight) return;
    const text = this.text;
    this.inflight = true;
    this.retryNeeded = false;
    this.unechoed.push(text);
    try {
      await this.options.write(text);
    } catch (error) {
      this.retryNeeded = true;
      const index = this.unechoed.lastIndexOf(text);
      if (index >= 0) this.unechoed.splice(index, 1);
      this.options.onError?.(error);
    } finally {
      this.inflight = false;
    }
    if (!this.retryNeeded && this.text !== text && this.timer === null) {
      await this.send();
      return;
    }
    const idle = this.idleWaiters;
    this.idleWaiters = [];
    for (const waiter of idle) waiter();
    if (this.retryNeeded) this.drainEchoWaiters();
    this.releaseHeld();
  }

  private releaseHeld() {
    const held = this.held;
    if (!held || this.composing || this.dirty) return;
    this.held = null;
    if (held.rev === this.serverRev) this.apply(held);
  }

  private apply(draft: { text: string; origin: DraftOrigin | null }) {
    if (draft.text === this.text) return;
    this.text = draft.text;
    this.options.onRemote(draft.text, draft.origin);
  }

  private drainEchoWaiters() {
    const waiters = this.echoWaiters;
    this.echoWaiters = [];
    for (const waiter of waiters) waiter();
  }
}
