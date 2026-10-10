import { FileDiff } from "@pierre/diffs";
import { glyph } from "../ui/glyphs";
import { renderMarkdown } from "../core/markdown";
import { presentMemory } from "./memory-detail";
import { parsePlan, presentPlan } from "./plan-detail";
import { messageElement, sentMessage, sessionMessageElement, type Markdown, type Participants } from "./directed";
import { parseApplyPatch, patchFileDiff, patchStats } from "./patch";
import type { Theme } from "../core/theme";
import type { ToolDetail } from "../core/wire";

/** Height of a patch before "Show full diff" is needed, in pixels. */
export const PATCH_PREVIEW_HEIGHT = 260;

/** Names shown for tool calls; unlisted tools show their own name. */
export const TOOL_LABELS: Record<string, string> = {
  exec_command: "Shell",
  write_stdin: "Stdin",
  apply_patch: "Patch",
  exec: "Code",
  web__run: "Web",
  view_image: "Image",
  update_plan: "Plan",
  wait: "Wait",
  image_gen__imagegen: "Image",
  spawn_agent: "Agent",
  wait_agent: "Wait",
  list_agents: "Agents",
  close_agent: "Close",
  interrupt_agent: "Stop",
  send_agent_message: "Message",
  message_session: "Message",
  current_session: "Session",
  read_session: "Session",
  find_sessions: "Sessions",
  memory: "Memory",
};

/**
 * The name shown for a tool call. Code Mode runs are a "Batch" once they have called tools of their
 * own; tools without a listed name read their identifier as words.
 */
export function toolLabel(name: string, childCount: number) {
  if (name === "exec") return childCount > 0 ? "Batch" : "Code";
  const label = TOOL_LABELS[name];
  if (label) return label;
  const words = name.replace(/_+/g, " ").trim();
  return words.charAt(0).toUpperCase() + words.slice(1);
}

/** Tools whose body is shown without being opened. */
export const TOOL_DEFAULT_OPEN = new Set(["apply_patch", "update_plan"]);

export type DetailContext = {
  theme: Theme;
  /** Names the parties of agent messages. */
  participants: Participants;
  markdown: Markdown;
  /** Whether this call's patch is shown in full rather than truncated. */
  full: boolean;
  /** Reports the Pierre instances created, so the owner can release them. */
  track(instances: FileDiff[]): void;
  /** Asks the owner to toggle between the truncated and full patch. */
  toggleFull(): void;
};

/** Renders a tool's arguments and result in the form that reads best for that tool. */
export function presentDetail(container: HTMLElement, name: string, detail: ToolDetail, context: DetailContext) {
  const args = detail.arguments;
  const field = (key: string) => (typeof args === "object" && args !== null ? (args as Record<string, unknown>)[key] : undefined);
  const markdownTheme = context.theme === "dark" ? "pierre-dark" : "pierre-light";
  if (name === "apply_patch") {
    const envelope = typeof args === "string" ? args : field("input");
    if (typeof envelope === "string" && presentPatch(container, envelope, context)) return;
  } else if (name === "exec_command" && typeof field("cmd") === "string") {
    container.replaceChildren(terminal(String(field("cmd")), resultOutput(detail.result), exitCode(detail.result)));
    return;
  } else if (name === "exec" && typeof field("code") === "string") {
    const code = document.createElement("div");
    code.className = "markdown tool-code";
    void renderMarkdown(code, "```js\n" + String(field("code")) + "\n```", markdownTheme, { highlight: true, placeholder: " " });
    container.replaceChildren(code, ...(detail.result === null ? [] : [terminal(null, resultOutput(detail.result), null)]));
    return;
  } else if (name === "memory") {
    const memory = presentMemory(args, detail.result, context);
    if (memory) {
      container.replaceChildren(memory);
      return;
    }
  } else if (name === "update_plan") {
    const plan = parsePlan(args);
    if (plan) {
      container.replaceChildren(presentPlan(plan));
      return;
    }
  } else if (name === "message_session") {
    const message = sessionMessageElement(args, detail.result, context.markdown);
    if (message) {
      container.replaceChildren(message);
      return;
    }
  } else if (name === "send_agent_message") {
    const message = sentMessage(args, detail.result, context.participants.viewer);
    if (message) {
      container.replaceChildren(messageElement(message, context.participants, context.markdown, { replyTo: message.in_reply_to === null ? undefined : "#" + message.in_reply_to }));
      return;
    }
  }
  container.replaceChildren(
    jsonBlock("Arguments", detail.arguments),
    ...(detail.result === null ? [] : [jsonBlock("Result", detail.result)]),
    ...(detail.metadata === null ? [] : [jsonBlock("Metadata", detail.metadata)]),
  );
}

function presentPatch(container: HTMLElement, envelope: string, context: DetailContext) {
  const files = parseApplyPatch(envelope);
  if (files.length === 0) return false;
  const { additions, deletions } = patchStats(files);
  const patch = document.createElement("div");
  patch.className = `patch${context.full ? " full" : ""}`;
  patch.dataset.additions = String(additions);
  patch.dataset.deletions = String(deletions);
  const view = document.createElement("div");
  view.className = "patch-view";
  const instances: FileDiff[] = [];
  const theme = context.theme === "dark" ? "pierre-dark" as const : "pierre-light" as const;
  for (const file of files) {
    const section = document.createElement("section");
    section.className = "patch-file";
    section.innerHTML = `<header class="patch-head"><span class="patch-kind"></span><span class="patch-path"></span></header>`;
    const kind = section.querySelector<HTMLElement>(".patch-kind")!;
    kind.dataset.kind = file.kind;
    kind.textContent = file.kind === "add" ? "Added" : file.kind === "delete" ? "Deleted" : file.moveTo ? "Moved" : "Edited";
    section.querySelector(".patch-path")!.textContent = file.moveTo ? `${file.path} → ${file.moveTo}` : file.path;
    const diff = patchFileDiff(file, `patch-${instances.length}-${envelope.length}-${file.path}`);
    if (diff) {
      const host = document.createElement("div");
      host.className = "patch-code";
      section.append(host);
      const instance = new FileDiff({
        theme,
        themeType: context.theme,
        diffStyle: "unified",
        diffIndicators: "bars",
        hunkSeparators: "simple",
        lineDiffType: "word-alt",
        disableLineNumbers: true,
        disableFileHeader: true,
        overflow: "scroll",
      });
      instance.render({ fileDiff: diff, containerWrapper: host });
      instances.push(instance);
    }
    view.append(section);
  }
  const more = document.createElement("button");
  more.type = "button";
  more.className = "patch-more";
  more.textContent = context.full ? "Show less" : "Show full diff";
  more.hidden = true;
  more.addEventListener("click", () => context.toggleFull());
  // The toggle is offered only when the diff is taller than the truncated view.
  new ResizeObserver(() => {
    more.hidden = view.scrollHeight <= PATCH_PREVIEW_HEIGHT + 1;
  }).observe(view);
  patch.append(view, more);
  container.replaceChildren(patch);
  context.track(instances);
  return true;
}

function terminal(command: string | null, output: string, code: number | null) {
  const element = document.createElement("div");
  element.className = "term";
  if (command !== null) {
    const line = document.createElement("div");
    line.className = "term-cmd";
    line.innerHTML = `<span class="term-prompt">$</span><code></code>`;
    line.querySelector("code")!.textContent = command;
    element.append(line);
  }
  if (output.trim()) {
    const pre = document.createElement("pre");
    pre.className = "term-out";
    pre.textContent = output.replace(/\n$/, "");
    element.append(pre);
  } else if (command !== null) {
    const empty = document.createElement("div");
    empty.className = "term-empty";
    empty.textContent = "No output";
    element.append(empty);
  }
  if (code !== null && code !== 0) {
    const foot = document.createElement("div");
    foot.className = "term-exit";
    foot.innerHTML = glyph("alert");
    foot.append(`Exited with code ${code}`);
    element.append(foot);
  }
  return element;
}

/** The text of a tool result: a command's output, a list of text parts, or the raw JSON. */
function resultOutput(result: unknown): string {
  if (typeof result === "string") return result;
  if (Array.isArray(result)) {
    return result.map((part) => (typeof part === "object" && part !== null && "text" in part ? String((part as { text: unknown }).text) : JSON.stringify(part))).join("\n");
  }
  if (typeof result === "object" && result !== null && typeof (result as { output?: unknown }).output === "string") {
    return (result as { output: string }).output;
  }
  return result === null || result === undefined ? "" : JSON.stringify(result, null, 2);
}

function exitCode(result: unknown): number | null {
  const code = typeof result === "object" && result !== null ? (result as { exit_code?: unknown }).exit_code : undefined;
  return typeof code === "number" ? code : null;
}

function jsonBlock(label: string, value: unknown) {
  const section = document.createElement("section");
  section.className = "tool-block";
  section.innerHTML = `<h4></h4><pre></pre>`;
  section.querySelector("h4")!.textContent = label;
  section.querySelector("pre")!.textContent = typeof value === "string" ? value : JSON.stringify(value, null, 2);
  return section;
}
