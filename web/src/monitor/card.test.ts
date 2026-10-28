import { expect, test } from "bun:test";
import { evidenceFor, liveLine, restates } from "./card";
import { plainText } from "./text";
import type { Assessment, Concern } from "./attention";
import type { SessionActivity, SessionNote, SessionSummary } from "../core/wire";

type ToolActivity = NonNullable<SessionActivity["tool"]>;

const summary = (state: SessionSummary["state"], title = "Choose the config loader") => ({ id: "a", title, state }) as SessionSummary;
const activity = (fields: Partial<SessionActivity>) => ({ agents: { running: 0, total: 0 }, ...fields }) as SessionActivity;
const tool = (state: ToolActivity["state"], name = "exec_command", outcome: ToolActivity["outcome"] = null): ToolActivity =>
  ({ name, summary: "cargo test", state, at_ms: null, outcome });
const note = (fields: Partial<SessionNote> = {}): SessionNote =>
  ({ session: "a", attention: "ok", goal: null, health: null, previous_health: null, blocks: [], reason: null, suggestion: null, ...fields });
const assessment = (concern: Concern, fields: Partial<SessionNote> | null = null): Assessment =>
  ({ concern, group: "needs", note: fields && note(fields), noteEarlier: false });

test("the live line names the call in flight, the phase, retries, and agents, and otherwise says idle", () => {
  expect(liveLine(summary("running"), activity({ status: { kind: "tool", name: "exec_command" }, tool: tool("running") })))
    .toMatchObject({ text: "Running", code: "cargo test" });
  expect(liveLine(summary("running"), activity({ tool: tool("running", "exec") }))).toMatchObject({ text: "Running code", code: null, detail: "cargo test" });
  expect(liveLine(summary("running"), activity({ status: { kind: "retrying", delay_ns: 1e9, next_attempt: 2, max_attempts: 5 } })))
    .toMatchObject({ text: "Retrying 2/5", tone: "warning" });
  expect(liveLine(summary("running"), activity({ status: { kind: "waiting_for_background_work" }, agents: { running: 2, total: 3 } })))
    .toMatchObject({ text: "Waiting on 2 agents" });
  expect(liveLine(summary("running"), activity({ status: { kind: "thinking" } }))).toMatchObject({ text: "Thinking", code: null });
  expect(liveLine(summary("running"), undefined)).toMatchObject({ text: "Working" });
  expect(liveLine(summary("idle"), activity({ tool: tool("failed") }))).toMatchObject({ text: "Idle", tone: "muted" });
  expect(liveLine(summary("error"), activity({ status: { kind: "error", message: "connection reset" } }))).toMatchObject({ text: "Error", tone: "danger" });
});

test("an error shows what stopped the session: its error, else the failed call and its outcome", () => {
  expect(evidenceFor(summary("error"), activity({ error: "connection reset" }), assessment("error"))).toEqual({ kind: "error", text: "connection reset" });
  expect(evidenceFor(summary("idle"), activity({ tool: tool("failed", "exec_command", { exit_code: 101, summary: null, tail: [] }) }), assessment("error")))
    .toEqual({ kind: "failed", text: "cargo test", detail: "exit 101" });
});

test("an idle session shows its last reply, else the monitor's goal, else the prompt", () => {
  const reply = activity({ message: { text: "Which loader do you prefer?", commentary: false, at_ms: 0 } });
  expect(evidenceFor(summary("idle"), reply, assessment("needs_input", { goal: "Pick a loader" })))
    .toEqual({ kind: "message", text: "Which loader do you prefer?", commentary: false });
  expect(evidenceFor(summary("idle"), activity({}), assessment("idle", { goal: "Pick a loader" }))).toEqual({ kind: "goal", text: "Pick a loader" });
  expect(evidenceFor(summary("idle"), activity({ prompt: { text: "Compare  the\nloaders", at_ms: 0 } }), assessment("idle")))
    .toEqual({ kind: "prompt", text: "Compare the loaders" });
});

test("stuck and off-track sessions show the monitor's reason", () => {
  const running = activity({ tool: tool("running") });
  expect(evidenceFor(summary("running"), running, assessment("off_track", { reason: "It is editing the wrong file.", goal: "Benchmark" })))
    .toEqual({ kind: "reason", text: "It is editing the wrong file." });
});

test("a running session shows what it is working toward, else its prompt", () => {
  expect(evidenceFor(summary("running"), activity({ tool: tool("running") }), assessment("working", { goal: "Fix the flaky test" })))
    .toEqual({ kind: "goal", text: "Fix the flaky test" });
  // A call that failed is not shown as a current error while the session works on.
  expect(evidenceFor(summary("running"), activity({ tool: tool("failed"), prompt: { text: "Run the tests again.", at_ms: 0 } }), assessment("working")))
    .toEqual({ kind: "prompt", text: "Run the tests again." });
});

test("a goal or prompt that only restates the title is left out", () => {
  const title = "Choose the config loader";
  expect(evidenceFor(summary("running", title), activity({ prompt: { text: "Choose the config loader.", at_ms: 0 } }), assessment("working", { goal: "choose the config loader" })))
    .toBeNull();
  expect(restates(title, "Choose the config loader!")).toBe(true);
  expect(restates(title, "config loader")).toBe(true);
  expect(restates(title, "Choose the config loader, then port web.rs and the TUI to it and delete the old parser")).toBe(false);
  expect(restates(title, "Benchmark the journal")).toBe(false);
});

test("messages are shown as plain prose", () => {
  expect(plainText("Done.[^1]\n\n[^1]: Retried once.")).toBe("Done.\n\nRetried once.");
  const markdown = [
    "## Done",
    "",
    "The loop now **polls** `requests` (see [bridge.rs](src/bridge.rs)):",
    "",
    "```rust",
    "tokio::select! {}",
    "```",
    "",
    "> Publications are dropped while suspended.",
    "",
    "> [!WARNING]",
    "> Drafts are lost on restart. ![shot](/tmp/shot.png)",
    "",
    "```text",
    "unterminated",
  ].join("\n");
  expect(plainText(markdown)).toBe([
    "Done",
    "",
    "The loop now polls requests (see bridge.rs):",
    "",
    "tokio::select! {}",
    "",
    "Publications are dropped while suspended.",
    "",
    "Drafts are lost on restart.",
    "",
    "unterminated",
  ].join("\n"));
});

test("a short fenced result reads as its lines, while a long block collapses to a marker", () => {
  expect(plainText("After creating it, the second cat printed:\n\n```text\nhello\n```")).toBe("After creating it, the second cat printed:\n\nhello");
  const long = ["```rust", "fn main() {", "    let x = 1;", "    println!(\"{x}\");", "}", "```"].join("\n");
  expect(plainText(`Here it is:\n\n${long}`)).toBe("Here it is:\n\n[code]");
});
