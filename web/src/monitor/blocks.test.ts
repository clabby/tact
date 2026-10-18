import { beforeAll, expect, test } from "bun:test";
import { Window } from "happy-dom";
import { closedBlocks, renderBlock, type BlockContext, type BlockForm } from "./blocks";
import type { SessionActivity, StatusBlock } from "../core/wire";

beforeAll(() => {
  Object.assign(globalThis, { document: new Window().document });
});

const activity = (fields: Partial<SessionActivity> = {}): SessionActivity => ({
  session: "a", status: null, turn_elapsed_ns: null, prompt: null, message: null, tool: null,
  turn: { tools: 0, failed: 0, patches: 0 }, plan: null, error: null, agents: { running: 0, total: 0 }, queued: 0, context: null,
  ...fields,
});

const busy = activity({
  message: { text: "Done. Which loader do you prefer?", commentary: false, at_ms: null },
  plan: { done: 2, total: 4, current: "Run the focused tests" },
  tool: { name: "exec_command", summary: "cargo test -p tact", state: "failed", at_ms: null, outcome: { exit_code: 101, tail: [], summary: "17 passed, 1 failed" } },
  prompt: { text: "Fix the  flaky\ntest", at_ms: null },
  agents: { running: 2, total: 3 },
  error: "connection reset",
});

function draw(block: StatusBlock, form: BlockForm, from: SessionActivity = busy, answer: BlockContext["answer"] = () => {}) {
  return renderBlock(block, form, { activity: from, answer });
}

const text = (element: Element | null) => element?.textContent?.replace(/\s+/g, " ").trim();

const progress: StatusBlock = { type: "progress", label: "Views ported", done: 3, total: 5, unit: "views" };
const metric: StatusBlock = { type: "metric", label: "p99 frame time", value: "42 ms", previous: "51 ms", tone: "success" };
const sentence: StatusBlock = { type: "text", text: "The invariant holds.", tone: "warning" };
const liveTool: StatusBlock = { type: "live", source: "tool" };
const liveAgents: StatusBlock = { type: "live", source: "agents" };

test("a closed tile shows the first block, and the second too when both take one line", () => {
  expect(closedBlocks([sentence, progress], busy)).toEqual([sentence]);
  expect(closedBlocks([progress, sentence], busy)).toEqual([progress]);
  expect(closedBlocks([progress, metric, sentence], busy)).toEqual([progress, metric]);
  expect(closedBlocks([liveTool, liveAgents, metric], busy)).toEqual([liveTool, liveAgents]);
  // A live block with nothing to show yet, or a block type this browser does not know, is passed over.
  expect(closedBlocks([liveTool, { type: "chart" } as unknown as StatusBlock, metric, sentence], activity())).toEqual([metric]);
  expect(closedBlocks([liveAgents], activity())).toEqual([]);
  expect(closedBlocks([], busy)).toEqual([]);
});

test("text, progress, and metric blocks read the same closed and in Details", () => {
  for (const form of ["compact", "full"] as const) {
    const line = draw(sentence, form)!;
    expect([line.dataset.block, line.dataset.tone, text(line)]).toEqual(["text", "warning", "The invariant holds."]);
    const bar = draw(progress, form)!;
    expect(text(bar)).toBe("Views ported3/5 views");
    expect(bar.querySelector(".block-bar")!.getAttribute("aria-valuenow")).toBe("3");
    expect(bar.querySelector<HTMLElement>(".block-bar-fill")!.style.width).toBe("60%");
    const value = draw(metric, form)!;
    expect([value.dataset.tone, text(value.querySelector(".metric-value")), text(value.querySelector(".metric-was"))]).toEqual(["success", "42 ms", "was 51 ms"]);
  }
  expect(draw({ ...metric, previous: null, tone: "mauve" as "neutral" }, "compact")!.querySelector(".metric-was")).toBeNull();
  expect(draw({ ...metric, tone: "mauve" as "neutral" }, "compact")!.dataset.tone).toBe("neutral");
});

test("a checklist closes to its count and current item, and lists every item with its state in Details", () => {
  const checklist: StatusBlock = {
    type: "checklist",
    title: "Migration",
    items: [
      { label: "Port the parser", state: "done" },
      { label: "Port the writer", state: "done" },
      { label: "Port the tests", state: "active" },
      { label: "Drop the old crate", state: "pending" },
      { label: "Benchmark", state: "skipped" },
    ],
  };
  const line = draw(checklist, "compact")!;
  expect([line.dataset.state, text(line.querySelector(".block-count")), text(line.querySelector(".checklist-current")), text(line.querySelector(".block-label"))])
    .toEqual(["active", "2/5", "Port the tests", "Migration"]);
  const full = draw(checklist, "full")!;
  expect([...full.querySelectorAll<HTMLElement>(".block-item")].map((item) => [item.dataset.state, text(item)]))
    .toEqual(checklist.items.map((item) => [item.state, item.label]));
});

test("history closes to its newest step and lists every step newest first in Details", () => {
  const history: StatusBlock = {
    type: "history",
    items: [
      { action: "Ran the suite", outcome: "worked", detail: "312 passed" },
      { action: "Ran the bridge tests", outcome: "failed", detail: "17 of 18" },
      { action: "Compared orderings", outcome: "unclear", detail: null },
    ],
  };
  const newest = draw(history, "compact")!;
  expect([newest.dataset.outcome, newest.querySelector(".block-mark")!.getAttribute("aria-label"), text(newest)]).toEqual(["worked", "worked", "Ran the suite312 passed"]);
  const full = draw(history, "full")!;
  expect([...full.querySelectorAll<HTMLElement>(".block-step")].map((step) => [step.dataset.outcome, text(step.querySelector(".block-step-text"))]))
    .toEqual([["worked", "Ran the suite"], ["failed", "Ran the bridge tests"], ["unclear", "Compared orderings"]]);
});

test("a question offers its options as buttons that hand the option to the reply", () => {
  const answered: string[] = [];
  const question = draw({ type: "question", text: "Which loader?", options: ["TOML", "figment"] }, "compact", busy, (option) => answered.push(option))!;
  expect(text(question.querySelector(".question-text"))).toBe("Which loader?");
  const options = [...question.querySelectorAll<HTMLButtonElement>(".question-option")];
  expect(options.map((option) => [option.type, option.textContent])).toEqual([["button", "TOML"], ["button", "figment"]]);
  options[1]!.click();
  expect(answered).toEqual(["figment"]);
  expect(draw({ type: "question", text: "Anything else?", options: [] }, "full")!.querySelector(".question-options")).toBeNull();
});

test("code keeps three lines closed and all four in Details", () => {
  const code: StatusBlock = { type: "code", label: "Failing assertion", text: "one\ntwo\nthree\nfour" };
  const lines = (form: BlockForm) => [...draw(code, form)!.querySelectorAll(".code-line")].map((line) => line.textContent);
  expect(lines("compact")).toEqual(["one", "two", "three …"]);
  expect(lines("full")).toEqual(["one", "two", "three", "four"]);
  expect(text(draw(code, "full")!.querySelector("figcaption"))).toBe("Failing assertion");
});

test("live blocks show the session's own data, and nothing when it has none", () => {
  const live = (source: "message" | "plan" | "tool" | "prompt" | "agents" | "error", form: BlockForm = "compact", from = busy) => draw({ type: "live", source }, form, from);

  expect(text(live("message")!.querySelector(".clip-inner"))).toBe("Done. Which loader do you prefer?");
  expect(live("message")!.querySelector<HTMLElement>(".clip-box")!.dataset.shortened).toBeUndefined();
  const long = activity({ message: { text: "Background. ".repeat(40) + "Which loader do you prefer?", commentary: false, at_ms: null } });
  expect(live("message", "compact", long)!.querySelector<HTMLElement>(".clip-box")!.dataset.shortened).toBe("true");
  expect(text(live("message", "full", long))).toContain("Background. Background.");
  expect([text(live("plan")!.querySelector(".block-count")), text(live("plan")!.querySelector(".plan-current"))]).toEqual(["2/4", "Run the focused tests"]);
  const tool = live("tool")!;
  expect([tool.dataset.outcome, text(tool.querySelector(".tool-command")), text(tool.querySelector(".block-detail"))]).toEqual(["failed", "cargo test -p tact", "17 passed, 1 failed"]);
  expect(text(live("prompt"))).toBe("Fix the flaky test");
  expect(live("prompt", "full")!.querySelector(".block-prompt-text")!.textContent).toBe("Fix the  flaky\ntest");
  expect(text(live("agents"))).toBe("2 of 3 subagents running");
  expect(text(live("error"))).toBe("connection reset");

  for (const source of ["message", "plan", "tool", "prompt", "agents", "error"] as const) expect(live(source, "compact", activity())).toBeNull();
});

test("every string from the model is shown as text, never as markup", () => {
  const markup = "<img src=x onerror=alert(1)><a href='https://example.com'>link</a>";
  const blocks: StatusBlock[] = [
    { type: "text", text: markup, tone: "neutral" },
    { type: "progress", label: markup, done: 1, total: 2, unit: markup },
    { type: "metric", label: markup, value: markup, previous: markup, tone: "danger" },
    { type: "checklist", title: markup, items: [{ label: markup, state: "active" }] },
    { type: "history", items: [{ action: markup, outcome: "worked", detail: markup }] },
    { type: "question", text: markup, options: [markup] },
    { type: "code", label: markup, text: markup },
  ];
  for (const block of blocks) {
    for (const form of ["compact", "full"] as const) {
      const element = draw(block, form)!;
      expect(element.querySelector("img, a")).toBeNull();
      expect(element.textContent).toContain(markup);
    }
  }
});

test("a question that cannot be answered now lists its options as plain text", () => {
  const question = draw({ type: "question", text: "Which loader?", options: ["TOML", "figment"] }, "full", busy, null)!;
  expect(question.querySelector("button")).toBeNull();
  expect([...question.querySelectorAll(".question-choice")].map((choice) => choice.textContent)).toEqual(["TOML", "figment"]);
});

test("a text block marks its tone beside the sentence instead of colouring it, and a neutral one has no mark", () => {
  const labels = (tone: string) => {
    const line = draw({ type: "text", text: "The suite passes.", tone: tone as "neutral" }, "compact")!;
    expect(text(line.querySelector(".block-text-body"))).toBe("The suite passes.");
    return line.querySelector(".block-mark")?.getAttribute("aria-label") ?? null;
  };
  expect([labels("success"), labels("warning"), labels("danger"), labels("neutral"), labels("mauve")]).toEqual(["good", "warning", "problem", null, null]);
});
