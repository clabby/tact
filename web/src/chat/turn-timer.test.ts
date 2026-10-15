import { beforeAll, describe, expect, test } from "bun:test";
import { Window } from "happy-dom";
import { initialState, reduce } from "../core/store";
import { ManualTimers } from "../core/test-support";
import type { SessionSnapshot } from "../core/wire";
import { TurnTimer } from "./turn-timer";

beforeAll(() => {
  Object.assign(globalThis, { document: new Window().document });
});

function harness() {
  const timers = new ManualTimers();
  const element = document.createElement("span") as unknown as HTMLElement;
  const timer = new TurnTimer(element, timers, () => timers.now);
  return { timers, element, timer, label: () => (element.hidden ? null : element.textContent) };
}

function snapshot(turnElapsedNs?: number | null): SessionSnapshot {
  return {
    session: "a", title: "A", title_pending: false, model: "sol", effort: "high", reasoning_mode: "standard", speed: "standard",
    entries: [], status: null, queue: [], draft: { rev: 1, text: "", images: [] }, running: turnElapsedNs != null,
    turn_elapsed_ns: turnElapsedNs, context: null, subagents: { max_subagents: 4, agents: [] },
  };
}

describe("turn timer", () => {
  test("is hidden until a turn starts, then counts from zero", () => {
    const { timers, timer, label } = harness();
    expect(label()).toBeNull();
    timer.show(timers.now);
    expect(label()).toBe("0s");
  });

  test("wakes once per second boundary and prints like the terminal", () => {
    const { timers, timer, label } = harness();
    timer.show(timers.now - 64_750);
    expect(label()).toBe("1m 4s");
    expect(timers.scheduled).toEqual([250]);
    timers.advance(249);
    expect(label()).toBe("1m 4s");
    timers.advance(1);
    expect(label()).toBe("1m 5s");
    expect(timers.scheduled).toEqual([1000]);
    timers.advance(3_600_000);
    expect(label()).toBe("1h 1m 5s");
  });

  test("showing the same turn again keeps its clock running", () => {
    const { timers, timer } = harness();
    const started = timers.now - 500;
    timer.show(started);
    timer.show(started);
    expect(timers.scheduled).toEqual([500]);
  });

  test("stops and hides when the turn ends", () => {
    const { timers, timer, label } = harness();
    timer.show(timers.now);
    timer.show(null);
    expect(label()).toBeNull();
    expect(timer.running).toBe(false);
    expect(timers.scheduled).toEqual([]);
  });

  test("a reload mid-turn resumes from the server's elapsed time", () => {
    const { timers, timer, label } = harness();
    timers.now = 10_000;
    const state = initialState();
    reduce(state, { type: "snapshot", data: snapshot(65_400_000_000) }, timers.now);
    timer.show(state.session!.turnStartedAt);
    expect(label()).toBe("1m 5s");
    expect(timers.scheduled).toEqual([600]);
  });

  test("turn timer events move the clock to the next turn and clear it", () => {
    const state = initialState();
    reduce(state, { type: "snapshot", data: snapshot() }, 0);
    expect(state.session!.turnStartedAt).toBeNull();
    expect(reduce(state, { type: "turn_timer", data: { session: "a", elapsed_ns: 5_000_000_000 } }, 20_000))
      .toEqual([{ type: "status" }]);
    expect(state.session!.turnStartedAt).toBe(15_000);
    expect(reduce(state, { type: "turn_timer", data: { session: "b", elapsed_ns: 1 } }, 20_000)).toEqual([]);
    reduce(state, { type: "turn_timer", data: { session: "a", elapsed_ns: null } }, 21_000);
    expect(state.session!.turnStartedAt).toBeNull();
  });
});
