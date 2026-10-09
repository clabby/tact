import { afterAll, beforeAll, expect, test } from "bun:test";
import { GlobalRegistrator } from "@happy-dom/global-registrator";
import { ApiClient } from "../core/api-client";
import { initialState, reduce, transcriptData } from "../core/store";
import { settle } from "../core/test-support";
import type { Subagent } from "../core/wire";

const agent: Subagent = {
  id: 1, parent: null, session_id: "child", role: "worker", task: "t", model: "sol", thinking: "high",
  reasoning_mode: "standard", status: { state: "running" },
};

beforeAll(() => {
  GlobalRegistrator.register({ url: "https://hub.test/" });
  (globalThis as Record<string, unknown>).ResizeObserver = class { observe() {} unobserve() {} disconnect() {} };
  // Transcript Markdown renders once its row is visible; every observed row counts as visible.
  (globalThis as Record<string, unknown>).IntersectionObserver = class {
    constructor(private readonly callback: (entries: unknown[]) => void) {}
    observe(target: Element) { queueMicrotask(() => this.callback([{ isIntersecting: true, target }])); }
    unobserve() {}
    disconnect() {}
  };
  HTMLCanvasElement.prototype.getContext = (() => new Proxy({}, { get: () => () => {}, set: () => true })) as never;
});
afterAll(() => GlobalRegistrator.unregister());

async function imageInSubagentTranscript(base: string) {
  const { openSubagents } = await import("./subagents");
  const state = initialState();
  reduce(state, {
    type: "snapshot",
    data: {
      session: "s1", title: "t", model: "sol", effort: "high", reasoning_mode: "standard", speed: "standard", entries: [],
      status: null, queue: [], draft: { rev: 1, text: "", images: [] }, running: false, context: null,
      subagents: { max_subagents: 2, agents: [agent] },
    },
  });
  const session = state.session!;
  session.agents.set(1, transcriptData([{ id: 1, revision: 1, parent: null, kind: "assistant", text: "![plot](out/plot.png)", complete: true, commentary: false }]));
  openSubagents(new ApiClient(base), () => session, () => "light", () => {}, 1);
  for (let attempt = 0; attempt < 20 && !document.querySelector(".agent-scroller img"); attempt += 1) await settle();
  return document.querySelector(".agent-scroller img")?.getAttribute("src");
}

test("a Markdown image in a subagent transcript loads through the page's API root", async () => {
  expect(await imageInSubagentTranscript("./api")).toBe("./api/file?path=out%2Fplot.png&session=s1");
  document.body.innerHTML = "";
  expect(await imageInSubagentTranscript("./api/m/devbox")).toBe("./api/m/devbox/file?path=out%2Fplot.png&session=s1");
});
