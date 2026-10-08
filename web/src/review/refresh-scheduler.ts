/** Timer functions, injectable so tests control time. */
type Timers = {
  setTimeout(callback: () => void, delayMs: number): ReturnType<typeof setTimeout>;
  clearTimeout(handle: ReturnType<typeof setTimeout>): void;
};

/**
 * Decides when a stale diff snapshot is refreshed. Changes are coalesced into one refresh after a
 * quiet period; nothing runs while another operation needs the snapshot to stay put, and a failed
 * refresh waits for an explicit retry instead of looping.
 */
export class RefreshScheduler {
  /** The workspace changed after the installed snapshot was taken. */
  stale = false;
  /** Why the last refresh failed; cleared by the next change or retry. */
  failed?: string;
  private timer?: ReturnType<typeof setTimeout>;
  private blocked = false;

  constructor(
    private readonly refresh: () => void,
    private readonly delayMs: number,
    private readonly timers: Timers = globalThis,
  ) {}

  /** Whether a refresh is queued behind the quiet period. */
  get waiting() { return this.timer !== undefined; }

  markStale() {
    this.stale = true;
    this.failed = undefined;
    this.cancel();
    this.schedule();
  }

  setBlocked(blocked: boolean) {
    this.blocked = blocked;
    this.schedule();
  }

  /** A refresh begins. Changes that land while it runs make the next snapshot stale again. */
  start() {
    this.stale = false;
  }

  fail(message: string) {
    this.stale = true;
    this.failed = message;
  }

  /** Refreshes now, regardless of the quiet period. */
  retry() {
    this.failed = undefined;
    this.cancel();
    this.refresh();
  }

  dispose() {
    this.cancel();
  }

  private cancel() {
    if (this.timer !== undefined) this.timers.clearTimeout(this.timer);
    this.timer = undefined;
  }

  private schedule() {
    if (this.timer !== undefined) return;
    if (!this.stale || this.failed !== undefined || this.blocked) return;
    this.timer = this.timers.setTimeout(() => {
      this.timer = undefined;
      this.refresh();
    }, this.delayMs);
  }
}
