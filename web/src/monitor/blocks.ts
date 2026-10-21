import { glyph } from "../ui/glyphs";
import { plainText } from "./text";
import type { BlockTone, ChecklistItem, HistoryItem, LiveSource, SessionActivity, StatusBlock, ToolOutcome } from "../core/wire";

/**
 * The monitor model's status blocks as the Monitor draws them: a compact form for the closed tile
 * and a full form for Details. Every string from the model is set as text, never as markup, so a
 * note cannot add links, images, or HTML. Live blocks name a source and are filled from the
 * session's activity, so they stay as current as the rest of the tile.
 */

export type BlockForm = "compact" | "full";

export type BlockContext = {
  activity: SessionActivity | undefined;
  /**
   * Puts a question's option in the reply for editing; nothing is sent. Null while the question
   * cannot be answered, so its options read as plain text.
   */
  answer: ((option: string) => void) | null;
};

/** Lines of a code block shown on the closed tile; Details shows all of them (at most 4). */
const CODE_LINES_CLOSED = 3;

/** How each step outcome is marked and announced; the marks follow the card tones. */
export const OUTCOME_MARKS: Record<HistoryItem["outcome"], { mark: string; label: string }> = {
  worked: { mark: glyph("check"), label: "worked" },
  failed: { mark: glyph("close"), label: "failed" },
  running: { mark: "", label: "in progress" },
  unclear: { mark: glyph("minus"), label: "unclear" },
};

/** How each checklist state is marked and announced; pending and active draw their own dots. */
const CHECK_MARKS: Record<ChecklistItem["state"], { mark: string; label: string }> = {
  done: { mark: glyph("check"), label: "done" },
  active: { mark: "", label: "in progress" },
  pending: { mark: "", label: "pending" },
  failed: { mark: glyph("close"), label: "failed" },
  skipped: { mark: glyph("minus"), label: "skipped" },
};

/** How a text block's tone is marked beside it; the sentence itself keeps the normal text colour. */
const TONE_MARKS: Partial<Record<BlockTone, { mark: string; label: string }>> = {
  success: { mark: glyph("check"), label: "good" },
  warning: { mark: glyph("alert"), label: "warning" },
  danger: { mark: glyph("alert"), label: "problem" },
};

/** A finished shell-like call's outcome in a few words: its test summary, else its exit code. */
export function outcomeText(outcome: ToolOutcome | null) {
  if (!outcome) return null;
  return outcome.summary ?? (outcome.exit_code === null ? null : `exit ${outcome.exit_code}`);
}

type Tool = NonNullable<SessionActivity["tool"]>;

/** What a live block shows, read from the session's activity. */
export type LiveData =
  | { source: "message"; text: string; commentary: boolean }
  | { source: "plan"; done: number; total: number; current: string | null }
  | { source: "tool"; tool: Tool }
  | { source: "prompt"; text: string }
  | { source: "agents"; running: number; total: number }
  | { source: "error"; text: string };

/** The data a live source names, or null when the session has none of it right now. */
export function liveData(source: LiveSource, activity: SessionActivity | undefined): LiveData | null {
  if (!activity) return null;
  switch (source) {
    case "message":
      return activity.message ? { source, text: plainText(activity.message.text), commentary: activity.message.commentary } : null;
    case "plan":
      return activity.plan && activity.plan.total > 0 ? { source, ...activity.plan } : null;
    case "tool":
      return activity.tool ? { source, tool: activity.tool } : null;
    case "prompt": {
      const text = activity.prompt?.text.trim();
      return text ? { source, text } : null;
    }
    case "agents":
      return activity.agents.total > 0 ? { source, ...activity.agents } : null;
    case "error": {
      const text = activity.error ?? (activity.status?.kind === "error" ? activity.status.message : null);
      return text ? { source, text } : null;
    }
    default:
      return null;
  }
}

/** Whether a block has anything to show; a live block's source can be empty, and a newer server's block type unknown. */
export function hasContent(block: StatusBlock, activity: SessionActivity | undefined): boolean {
  switch (block.type) {
    case "text": return block.text.trim() !== "";
    case "progress": return block.total > 0;
    case "metric": return block.value.trim() !== "";
    case "checklist": return block.items.length > 0;
    case "history": return block.items.length > 0;
    case "question": return block.text.trim() !== "";
    case "code": return block.text.trim() !== "";
    case "live": return liveData(block.source, activity) !== null;
    default: return false;
  }
}

/** Whether a block's closed form takes a single line, so two of them fit where one block would. */
export function isOneLine(block: StatusBlock) {
  return block.type === "progress" || block.type === "metric"
    || (block.type === "live" && (block.source === "tool" || block.source === "agents" || block.source === "prompt"));
}

/**
 * Whether a block reports a problem by itself: a warning or danger text or metric, a newest step
 * that failed, a checklist whose shown item failed, or the session's error.
 */
export function reportsProblem(block: StatusBlock, activity: SessionActivity | undefined): boolean {
  if (!hasContent(block, activity)) return false;
  switch (block.type) {
    case "text":
    case "metric":
      return block.tone === "warning" || block.tone === "danger";
    case "history": return block.items[0]!.outcome === "failed";
    case "checklist": return currentItem(block.items).state === "failed";
    case "live": return block.source === "error";
    default: return false;
  }
}

/**
 * The blocks a closed tile shows: the first one with something to show, and the next as well when
 * both take a single line. That keeps the body to about four lines whatever the model composed.
 */
export function closedBlocks(blocks: readonly StatusBlock[], activity: SessionActivity | undefined): StatusBlock[] {
  const [first, second] = blocks.filter((block) => hasContent(block, activity));
  if (!first) return [];
  return second && isOneLine(first) && isOneLine(second) ? [first, second] : [first];
}

/** Whether a live block in `blocks` already shows `source`, so Details need not repeat it. */
export function showsLive(blocks: readonly StatusBlock[], source: LiveSource) {
  return blocks.some((block) => block.type === "live" && block.source === source);
}

/** The data a set of blocks depends on, so a tile redraws them only when it changes. */
export function blocksKey(blocks: readonly StatusBlock[], activity: SessionActivity | undefined) {
  return JSON.stringify(blocks.map((block) => block.type === "live" ? liveData(block.source, activity) : block));
}

/** Draws one block, or returns null when it has nothing to show. */
export function renderBlock(block: StatusBlock, form: BlockForm, context: BlockContext): HTMLElement | null {
  if (!hasContent(block, context.activity)) return null;
  const element = renderContent(block, form, context);
  element.classList.add("block");
  element.dataset.block = block.type === "live" ? `live-${block.source}` : block.type;
  element.dataset.form = form;
  return element;
}

function renderContent(block: StatusBlock, form: BlockForm, context: BlockContext): HTMLElement {
  switch (block.type) {
    case "text": {
      const element = toned(node("p", "block-text"), block.tone);
      const tone = TONE_MARKS[element.dataset.tone as BlockTone];
      if (tone) element.append(mark(tone.mark, tone.label));
      element.append(node("span", "block-text-body", block.text));
      return element;
    }
    case "progress":
      return progress(block.label, block.done, block.total, block.unit);
    case "metric": {
      const element = toned(node("p", "block-metric"), block.tone);
      element.append(node("span", "block-label", block.label), node("span", "metric-value", block.value));
      if (block.previous) element.append(node("span", "metric-was", `was ${block.previous}`));
      return element;
    }
    case "checklist":
      return form === "compact" ? checklistLine(block.title, block.items) : checklist(block.title, block.items);
    case "history":
      return form === "compact" ? step(block.items[0]!, "p") : steps(block.items);
    case "question":
      return question(block.text, block.options, context);
    case "code":
      return code(block.label, block.text, form);
    case "live":
      return live(liveData(block.source, context.activity)!, form);
  }
}

function node<K extends keyof HTMLElementTagNameMap>(tag: K, className: string, text?: string) {
  const element = document.createElement(tag);
  element.className = className;
  if (text !== undefined) element.textContent = text;
  return element;
}

function toned<T extends HTMLElement>(element: T, tone: string) {
  element.dataset.tone = tone === "success" || tone === "warning" || tone === "danger" ? tone : "neutral";
  return element;
}

/** A mark from the app's own glyphs, announced by `label`. */
function mark(markup: string, label: string) {
  const element = node("span", "block-mark");
  element.setAttribute("role", "img");
  element.setAttribute("aria-label", label);
  element.title = label;
  element.innerHTML = markup;
  return element;
}

function bar(done: number, total: number, label: string) {
  const element = node("span", "block-bar");
  element.setAttribute("role", "progressbar");
  element.setAttribute("aria-label", label);
  element.setAttribute("aria-valuemin", "0");
  element.setAttribute("aria-valuemax", String(total));
  element.setAttribute("aria-valuenow", String(done));
  const fill = node("span", "block-bar-fill");
  fill.style.width = `${Math.round(Math.min(1, Math.max(0, done / total)) * 100)}%`;
  element.append(fill);
  return element;
}

function progress(label: string, done: number, total: number, unit: string | null) {
  const element = node("p", "block-progress");
  element.append(node("span", "block-label", label), bar(done, total, label), node("span", "block-count", `${done}/${total}${unit ? " " + unit : ""}`));
  return element;
}

/** The item a checklist is on: the active one, else the first failed or pending, else the last. */
function currentItem(items: readonly ChecklistItem[]) {
  return items.find((item) => item.state === "active") ?? items.find((item) => item.state === "failed")
    ?? items.find((item) => item.state === "pending") ?? items.at(-1)!;
}

function checkMark(state: ChecklistItem["state"]) {
  const known = state in CHECK_MARKS ? state : "pending";
  return mark(CHECK_MARKS[known].mark, CHECK_MARKS[known].label);
}

function checklistLine(title: string | null, items: readonly ChecklistItem[]) {
  const element = node("p", "block-checklist-line");
  const current = currentItem(items);
  element.dataset.state = current.state;
  element.append(checkMark(current.state));
  if (title) element.append(node("span", "block-label", title));
  element.append(node("span", "block-count", `${items.filter((item) => item.state === "done").length}/${items.length}`), node("span", "checklist-current", current.label));
  return element;
}

function checklist(title: string | null, items: readonly ChecklistItem[]) {
  const element = node("div", "block-checklist");
  if (title) element.append(node("p", "block-title", title));
  const list = node("ul", "block-items");
  list.setAttribute("role", "list");
  list.append(...items.map((item) => {
    const row = node("li", "block-item");
    row.dataset.state = item.state;
    row.append(checkMark(item.state), node("span", "block-item-text", item.label));
    return row;
  }));
  element.append(list);
  return element;
}

function step({ action, outcome, detail }: HistoryItem, tag: "p" | "li") {
  const element = node(tag, "block-step");
  // A newer server's outcome this browser does not know reads as unclear.
  const known = outcome in OUTCOME_MARKS ? outcome : "unclear";
  element.dataset.outcome = known;
  const body = node("span", "block-step-body");
  body.append(node("span", "block-step-text", action));
  if (detail) body.append(node("span", "block-detail", detail));
  element.append(mark(OUTCOME_MARKS[known].mark, OUTCOME_MARKS[known].label), body);
  return element;
}

function steps(items: readonly HistoryItem[]) {
  const list = node("ul", "block-steps");
  list.setAttribute("role", "list");
  list.setAttribute("aria-label", "Recent steps, newest first");
  list.append(...items.map((item) => step(item, "li")));
  return list;
}

function question(text: string, options: readonly string[], { answer }: BlockContext) {
  const element = node("div", "block-question");
  element.append(node("p", "question-text", text));
  if (options.length) {
    const choices = node("div", "question-options");
    choices.append(...options.map((option) => {
      if (!answer) return node("span", "question-choice", option);
      const button = node("button", "question-option", option);
      button.type = "button";
      button.title = "Put this in the reply";
      button.addEventListener("click", () => answer(option));
      return button;
    }));
    element.append(choices);
  }
  return element;
}

function code(label: string | null, text: string, form: BlockForm) {
  const element = node("figure", "block-code");
  if (label) element.append(node("figcaption", "block-label", label));
  const lines = text.replace(/\n+$/, "").split("\n");
  const shown = form === "compact" ? lines.slice(0, CODE_LINES_CLOSED) : lines;
  if (shown.length < lines.length) shown[shown.length - 1] += " …";
  const pre = node("pre", "code-text");
  pre.append(...shown.map((line) => node("code", "code-line", line)));
  element.append(pre);
  return element;
}

function live(data: LiveData, form: BlockForm): HTMLElement {
  switch (data.source) {
    case "message": {
      if (form === "full") return node("p", "block-message-full", data.text);
      // The closed form keeps the end of the reply, where its question or result sits; the tile
      // marks the box as cut off when the text overflows it.
      const element = node("div", "block-message");
      element.classList.toggle("commentary", data.commentary);
      const box = node("p", "clip-box");
      box.append(node("span", "clip-inner", data.text.replace(/\n{2,}/g, "\n")));
      element.append(box);
      return element;
    }
    case "plan": {
      const element = node("div", "block-plan");
      const head = node("p", "block-progress");
      head.append(node("span", "block-label", "Plan"), bar(data.done, data.total, "Plan"), node("span", "block-count", `${data.done}/${data.total}`));
      element.append(head);
      if (data.current) element.append(node("p", "plan-current", data.current));
      return element;
    }
    case "tool": {
      const { tool } = data;
      const outcome = tool.state === "running" ? "running" : tool.state === "failed" ? "failed" : "worked";
      const element = node("p", "block-tool");
      element.dataset.outcome = outcome;
      const body = node("span", "block-step-body");
      body.append(node("code", "tool-command", tool.summary || tool.name));
      const detail = outcomeText(tool.outcome);
      if (detail) body.append(node("span", "block-detail", detail));
      element.append(mark(OUTCOME_MARKS[outcome].mark, OUTCOME_MARKS[outcome].label), body);
      return element;
    }
    case "prompt": {
      const element = node("p", "block-prompt");
      element.append(mark(glyph("user"), "your prompt"), node("span", "block-prompt-text", form === "compact" ? data.text.replace(/\s+/g, " ") : data.text));
      return element;
    }
    case "agents": {
      const element = node("p", "block-agents");
      element.append(mark(glyph("agents"), "subagents"), node("span", "", `${data.running} of ${data.total} subagent${data.total === 1 ? "" : "s"} running`));
      return element;
    }
    case "error": {
      const element = node("p", "block-error");
      element.append(mark(glyph("alert"), "error"), node("span", "block-error-text", data.text));
      return element;
    }
  }
}
