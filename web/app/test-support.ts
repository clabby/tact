import type { Timers } from "./draft";

/** Deterministic timers for tests: callbacks run only when the test advances the clock. */
export class ManualTimers implements Timers {
  now = 0;
  private next = 1;
  private pending = new Map<number, { at: number; callback: () => void }>();

  set(callback: () => void, ms: number) {
    const id = this.next++;
    this.pending.set(id, { at: this.now + ms, callback });
    return id;
  }

  clear(handle: unknown) {
    this.pending.delete(handle as number);
  }

  /** Delays of the scheduled callbacks, relative to now. */
  get scheduled() {
    return [...this.pending.values()].map((timer) => timer.at - this.now);
  }

  advance(ms: number) {
    this.now += ms;
    for (const [id, timer] of [...this.pending].sort((a, b) => a[1].at - b[1].at)) {
      if (timer.at > this.now) continue;
      this.pending.delete(id);
      timer.callback();
    }
  }
}

/** Lets pending promise continuations run. */
export const settle = () => new Promise<void>((resolve) => setTimeout(resolve, 0));
