import { afterEach, beforeAll, expect, test } from "bun:test";
import { Window } from "happy-dom";
import type { ApiClient } from "../core/api-client";
import { initialState, reduce, type AppState } from "../core/store";
import type { SessionSnapshot, StreamEvent, Subagent, WireEntry } from "../core/wire";
import { settle } from "../core/test-support";
import { openSubagents } from "./subagents";
import { transcript } from "./test-entries";

let window: Window;
beforeAll(() => {
  window = new Window();
  const { document, IntersectionObserver, ResizeObserver, requestAnimationFrame, cancelAnimationFrame } = window;
  Object.assign(globalThis, { window, document, IntersectionObserver, ResizeObserver, requestAnimationFrame, cancelAnimationFrame });
});
afterEach(() => document.body.replaceChildren());

const auditor: Subagent = {
  id: 1, parent: null, session_id: "child", role: "auditor", task: "Audit the protocol.", model: "sol", thinking: "high",
  reasoning_mode: "standard", status: { state: "completed", output: "No gaps found." },
};

function snapshot(): SessionSnapshot {
  return {
    session: "s", title: "Session", title_pending: false, model: "sol", effort: "high", reasoning_mode: "standard", speed: "standard",
    entries: [], status: null, queue: [], draft: { rev: 1, text: "", images: [] }, running: false, context: null,
    subagents: { max_subagents: 4, agents: [auditor] },
  };
}

/**
 * A connected page whose server serves `history` as the auditor's transcript. `open` shows the
 * popup on the auditor, and `receive` applies a stream event, forwarding changes as the app does.
 */
function page(history: () => WireEntry[]) {
  const state: AppState = initialState();
  const api = { agentEntries: async () => ({ entries: structuredClone(history()) }) } as unknown as ApiClient;
  const open = () => openSubagents(api, () => state.session, () => "dark" as never, () => {}, auditor.id);
  let view: ReturnType<typeof open> | null = null;
  const receive = (event: StreamEvent) => {
    for (const change of reduce(state, event)) {
      if (change.type === "session") view?.sessionChanged();
      if (change.type === "subagent_entry") view?.entryChanged(change.agent, change.id);
    }
  };
  receive({ type: "snapshot", data: snapshot() });
  return {
    receive,
    open: () => {
      view = open();
    },
    stream: (entry: WireEntry) => receive({ type: "subagent_entry", data: { session: "s", agent: auditor.id, entry } }),
    shown: async () => {
      await settle();
      await new Promise((resolve) => requestAnimationFrame(resolve));
      return document.querySelector(".agent-scroller")!.textContent;
    },
  };
}

test("an agent opened after the page connected shows the transcript it had before", async () => {
  const history = transcript();
  history.user("Audit the protocol.");
  history.say("Reading the protocol first.");
  history.tool("exec_command", "rg -n version");
  const tab = page(() => history.entries);
  // The page connected while the agent was working, so the stream delivered only its latest entry.
  tab.stream(history.entries.at(-1)!);

  tab.open();
  const shown = await tab.shown();
  expect(shown).toContain("Reading the protocol first.");
  expect(shown).toContain("rg -n version");
});

test("a shown agent keeps following the stream after a new snapshot", async () => {
  const history = transcript();
  history.say("Reading the protocol first.");
  const tab = page(() => history.entries);
  tab.open();
  expect(await tab.shown()).toContain("Reading the protocol first.");

  tab.receive({ type: "snapshot", data: snapshot() });
  history.say("No gaps found in the stream.");
  tab.stream(history.entries.at(-1)!);
  const shown = await tab.shown();
  expect(shown).toContain("Reading the protocol first.");
  expect(shown).toContain("No gaps found in the stream.");
});
