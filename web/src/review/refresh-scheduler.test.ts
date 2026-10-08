import { describe, expect, test } from "bun:test";
import { RefreshScheduler } from "./refresh-scheduler";

class FakeTimers {
  now = 0;
  private next = 1;
  private readonly pending = new Map<number, { at: number; callback: () => void }>();

  setTimeout(callback: () => void, delayMs: number) {
    const id = this.next++;
    this.pending.set(id, { at: this.now + delayMs, callback });
    return id as unknown as ReturnType<typeof setTimeout>;
  }

  clearTimeout(handle: ReturnType<typeof setTimeout>) {
    this.pending.delete(handle as unknown as number);
  }

  advance(delayMs: number) {
    this.now += delayMs;
    for (const [id, timer] of [...this.pending]) {
      if (timer.at > this.now) continue;
      this.pending.delete(id);
      timer.callback();
    }
  }
}

function harness() {
  const timers = new FakeTimers();
  let refreshes = 0;
  const scheduler = new RefreshScheduler(() => { refreshes++; }, 400, timers);
  return { timers, scheduler, refreshes: () => refreshes };
}

describe("refresh scheduling", () => {
  test("a burst of changes refreshes once after the last one", () => {
    const { timers, scheduler, refreshes } = harness();
    scheduler.markStale();
    timers.advance(300);
    scheduler.markStale();
    timers.advance(300);
    expect(refreshes()).toBe(0);
    timers.advance(100);
    expect(refreshes()).toBe(1);
  });

  test("a blocked panel waits for the blocking work to finish", () => {
    const { timers, scheduler, refreshes } = harness();
    scheduler.setBlocked(true);
    scheduler.markStale();
    timers.advance(5_000);
    expect(refreshes()).toBe(0);
    scheduler.setBlocked(false);
    timers.advance(400);
    expect(refreshes()).toBe(1);
  });

  test("a change during a refresh queues the next one", () => {
    const { timers, scheduler, refreshes } = harness();
    scheduler.markStale();
    timers.advance(400);
    scheduler.start();
    scheduler.setBlocked(true);
    scheduler.markStale();
    timers.advance(1_000);
    expect(refreshes()).toBe(1);
    scheduler.setBlocked(false);
    timers.advance(400);
    expect(refreshes()).toBe(2);
  });

  test("a failed refresh is not retried until a change or an explicit retry", () => {
    const { timers, scheduler, refreshes } = harness();
    scheduler.markStale();
    timers.advance(400);
    scheduler.start();
    scheduler.fail("offline");
    scheduler.setBlocked(false);
    timers.advance(10_000);
    expect(refreshes()).toBe(1);
    expect(scheduler.stale).toBe(true);
    expect(scheduler.failed).toBe("offline");

    scheduler.retry();
    expect(refreshes()).toBe(2);
    expect(scheduler.failed).toBeUndefined();

    scheduler.fail("offline");
    scheduler.markStale();
    timers.advance(400);
    expect(refreshes()).toBe(3);
  });

  test("repeated unblocking does not postpone a queued refresh", () => {
    const { timers, scheduler, refreshes } = harness();
    scheduler.markStale();
    timers.advance(300);
    scheduler.setBlocked(false);
    timers.advance(100);
    expect(refreshes()).toBe(1);
  });
});
