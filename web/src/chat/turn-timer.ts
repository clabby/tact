import { formatTurnDuration } from "../core/format";
import { realTimers, type Timers } from "./draft";

/**
 * The composer's clock for the oldest running turn, as the terminal shows beside its status line.
 * It counts on from the server-reported start with the monotonic clock and wakes only at the
 * second boundaries where its label changes.
 */
export class TurnTimer {
  private startedAt: number | null = null;
  private wake: unknown = null;

  constructor(
    private readonly element: HTMLElement,
    private readonly timers: Timers = realTimers,
    private readonly now: () => number = () => performance.now(),
  ) {
    element.hidden = true;
  }

  get running() {
    return this.startedAt !== null;
  }

  /** Counts from `startedAt` (a `performance.now()` time), or stops and hides with null. */
  show(startedAt: number | null) {
    if (startedAt === this.startedAt) return;
    this.startedAt = startedAt;
    if (this.wake !== null) this.timers.clear(this.wake);
    this.wake = null;
    this.element.hidden = startedAt === null;
    if (startedAt !== null) this.tick();
  }

  private tick = () => {
    const elapsed = Math.max(0, this.now() - this.startedAt!);
    this.element.textContent = formatTurnDuration(elapsed * 1e6);
    this.wake = this.timers.set(this.tick, 1000 - elapsed % 1000);
  };
}
