// Conversations between agents: the thread card in a transcript and the message presentation
// shared with `send_agent_message` tool details.
import { modelColor } from "../core/format";
import { glyph } from "../ui/glyphs";
import type { AgentMessage, MessageDelivery, MessagePurpose, Subagent, WireEntry } from "../core/wire";

export type DirectedEntry = Extract<WireEntry, { kind: "directed_message" }>;

/**
 * Who a transcript belongs to and who the session's agents are. `viewer` is the agent whose own
 * transcript is shown; it is absent in the session's transcript.
 */
export type Participants = { viewer?: number; agents: readonly Subagent[] };

/** How a message's sender or recipient is shown: a text label, a description, and a dot colour. */
export type Party = { label: string; title: string; color: string };

/** Bodies longer than this many lines are clamped until "Show more" is pressed. */
export const CLAMP_LINES = 12;
/** Characters per visual line at the transcript's width, used to count wrapped lines. */
const WRAP_COLUMNS = 96;

/** `id` is an agent, or `null` for the root session. Unknown agents read as their id. */
export function party(id: number | null, { viewer, agents }: Participants): Party {
  if (id === null) return { label: "root", title: "The root session", color: "var(--text)" };
  const agent = agents.find((candidate) => candidate.id === id);
  const title = agent ? `${agent.role} · #${id} · ${agent.model}` : `Agent #${id}`;
  const color = agent ? modelColor(agent.model) : "var(--faint)";
  if (id === viewer) return { label: "you", title: `You: ${title}`, color };
  return { label: agent?.role || `#${id}`, title, color };
}

/** Whether `body` is taller than `CLAMP_LINES` once long lines wrap. */
export function needsClamp(body: string) {
  let lines = 0;
  for (const line of body.trimEnd().split("\n")) {
    lines += Math.max(1, Math.ceil(line.length / WRAP_COLUMNS));
    if (lines > CLAMP_LINES) return true;
  }
  return false;
}

/**
 * The thread's delivery at a glance. A failure anywhere in the thread outranks a message still in
 * flight, which outranks a delivered one, so a failure is never hidden by later traffic.
 */
export function deliverySummary(messages: readonly AgentMessage[]): { state: MessageDelivery; label: string } {
  const failed = messages.filter((message) => message.delivery === "failed");
  if (failed.length) {
    const reason = failed.at(-1)!.detail ?? "unknown error";
    const count = failed.length === 1 ? "Delivery failed" : `${failed.length} deliveries failed`;
    return { state: "failed", label: `${count}: ${reason}` };
  }
  const pending = messages.filter((message) => message.delivery === "admitted");
  if (pending.length) return { state: "admitted", label: `Not yet delivered: ${pending.at(-1)!.detail ?? "admitted"}` };
  if (messages.length && messages.every((message) => message.delivery === "delivered")) {
    return { state: "delivered", label: "Delivered" };
  }
  return { state: "unknown", label: "Delivery state not yet known" };
}

/** A message's delivery as a short trailing note, or "" before any state is known. */
export function deliveryNote(message: AgentMessage) {
  switch (message.delivery) {
    case "admitted":
      return message.detail ?? "admitted";
    case "delivered":
      return message.detail ? `delivered · ${message.detail}` : "delivered";
    case "failed":
      return `failed: ${message.detail ?? "unknown error"}`;
    default:
      return "";
  }
}

/** Renders Markdown into a container, in the transcript's theme. */
export type Markdown = (container: HTMLElement, text: string) => void;

const PURPOSES: readonly MessagePurpose[] = ["delegate", "coordinate", "finding", "question", "reply"];

type Json = Record<string, unknown>;
const record = (value: unknown): Json | null => (typeof value === "object" && value !== null && !Array.isArray(value) ? value as Json : null);

/**
 * The message a `send_agent_message` call sent, from its arguments and its receipt (`result`,
 * possibly JSON-encoded). The sender is `viewer`, the transcript's owner. A receipt names how the
 * recipient took the message; a result that is not a receipt is the failure.
 */
export function sentMessage(args: unknown, result: unknown, viewer: number | undefined): AgentMessage | null {
  const fields = record(args);
  if (typeof fields?.agent_id !== "number" || typeof fields.message !== "string") return null;
  let receipt: unknown = result;
  if (typeof result === "string") {
    try {
      receipt = JSON.parse(result);
    } catch {
      receipt = { error: result };
    }
  }
  const decoded = record(receipt);
  let delivery: MessageDelivery = "unknown";
  let detail: string | null = null;
  if (typeof decoded?.disposition === "string") {
    delivery = "admitted";
    detail = decoded.disposition;
  } else if (typeof decoded?.error === "string") {
    delivery = "failed";
    detail = decoded.error;
  }
  return {
    id: typeof decoded?.message_id === "number" ? decoded.message_id : 0,
    from: viewer ?? null,
    to: fields.agent_id,
    purpose: PURPOSES.find((purpose) => purpose === fields.purpose) ?? "coordinate",
    priority: fields.priority === "urgent" ? "urgent" : "deferred",
    in_reply_to: typeof fields.in_reply_to === "number" ? fields.in_reply_to : null,
    body: fields.message,
    delivery,
    detail,
  };
}

/** What a thread card needs from its transcript. */
export type ThreadView = {
  participants: Participants;
  open: boolean;
  markdown: Markdown;
  /** Whether a long body was shown in full, and a way to remember the reader's choice. */
  full(message: number): boolean;
  setFull(message: number, full: boolean): void;
};

/** Renders a thread as a one-line card that expands into the whole conversation. */
export function renderThread(element: HTMLElement, entry: DirectedEntry, view: ThreadView) {
  const messages = entry.messages ?? [];
  const latest = messages.at(-1);
  const card = document.createElement("div");
  card.className = "dm";
  card.classList.toggle("open", view.open);
  const head = document.createElement("button");
  head.type = "button";
  head.className = "tool-row dm-head";
  head.setAttribute("aria-expanded", String(view.open));
  const { state, label } = deliverySummary(messages);
  card.dataset.delivery = state;
  const mark = span("tool-state", "");
  mark.setAttribute("role", "img");
  mark.setAttribute("aria-label", label);
  mark.title = label;
  const route = span("dm-route", "");
  const meta = span("tool-meta", "");
  if (latest) {
    route.append(...routeParts(latest, view.participants));
    meta.append(...labels(latest));
    if (messages.length > 1) meta.append(` · ${messages.length}`);
  } else {
    route.textContent = `${entry.from} → ${entry.to}`;
  }
  head.append(mark, route, span("dm-preview", (latest?.body ?? entry.body).replace(/\s+/g, " ").trim()), meta);
  head.insertAdjacentHTML("beforeend", glyph("chevron-right", "glyph chevron"));
  card.append(head);

  if (view.open) {
    const thread = document.createElement("div");
    thread.className = "dm-thread";
    if (latest) {
      for (const message of messages) {
        const answered = messages.find((candidate) => candidate.id === message.in_reply_to);
        const item = messageElement(message, view.participants, view.markdown, {
          full: view.full(message.id),
          setFull: (full) => view.setFull(message.id, full),
          replyTo: answered ? party(answered.from, view.participants).label : undefined,
        });
        item.classList.toggle("latest", message === latest);
        thread.append(item);
      }
    } else {
      const body = document.createElement("div");
      body.className = "markdown dm-body";
      view.markdown(body, entry.body);
      thread.append(body);
    }
    card.append(thread);
  }
  element.replaceChildren(card);
}

/**
 * One message: its sender's avatar, then sender, recipient, purpose, urgency, who it answers and its
 * delivery note on one line, and the body as Markdown below. Long bodies are clamped; `setFull`
 * hears the reader's choice. `replyTo` names the sender of the message this one answers.
 */
export function messageElement(
  message: AgentMessage,
  participants: Participants,
  markdown: Markdown,
  { full = false, setFull, replyTo }: { full?: boolean; setFull?(full: boolean): void; replyTo?: string } = {},
) {
  const item = document.createElement("article");
  item.className = "dm-message";
  item.dataset.purpose = message.purpose;
  const head = document.createElement("header");
  head.className = "dm-message-head";
  head.append(...routeParts(message, participants), ...labels(message));
  if (replyTo) head.append(span("dm-reply-to", `↩ ${replyTo}`));
  const note = deliveryNote(message);
  if (note) {
    const trailing = span("dm-note", note);
    trailing.dataset.state = message.delivery;
    trailing.title = note;
    head.append(trailing);
  }
  const body = document.createElement("div");
  body.className = "markdown dm-body";
  markdown(body, message.body);
  item.append(head, body);
  if (needsClamp(message.body)) {
    const more = document.createElement("button");
    more.type = "button";
    more.className = "dm-more";
    const show = (expanded: boolean) => {
      body.classList.toggle("clamped", !expanded);
      more.textContent = expanded ? "Show less" : "Show more";
      more.setAttribute("aria-expanded", String(expanded));
    };
    show(full);
    more.addEventListener("click", () => {
      const expanded = body.classList.contains("clamped");
      show(expanded);
      setFull?.(expanded);
    });
    item.append(more);
  }
  return item;
}

function routeParts(message: AgentMessage, participants: Participants) {
  const arrow = span("dm-arrow", "→");
  arrow.setAttribute("aria-label", "to");
  return [chip(party(message.from, participants)), arrow, chip(party(message.to, participants))];
}

function labels(message: AgentMessage) {
  const purpose = span("dm-purpose", message.purpose);
  if (message.priority !== "urgent") return [purpose];
  const urgent = document.createElement("span");
  urgent.className = "dm-urgent";
  urgent.title = "Urgent";
  urgent.setAttribute("role", "img");
  urgent.setAttribute("aria-label", "urgent");
  urgent.innerHTML = glyph("bolt");
  return [purpose, urgent];
}

function chip({ label, title, color }: Party) {
  const element = span("agent-chip", "");
  element.title = title;
  const dot = span("agent-chip-dot", "");
  dot.style.background = color;
  element.append(dot, span("agent-chip-label", label));
  return element;
}

function span(className: string, text: string) {
  const element = document.createElement("span");
  element.className = className;
  element.textContent = text;
  return element;
}
