import { describeError } from "../core/api-client";
import { formatAge, formatTokens, formatTurnDuration, modelColor, statusLabel } from "../core/format";
import { toolLabel } from "../chat/tool-label";
import { createSphere } from "../ui/dot-sphere";
import { glyph } from "../ui/glyphs";
import { openMenu } from "../ui/menu";
import type { ActivityView } from "../core/store";
import type { ModelCatalog, SessionActivity, SessionSummary, StatusBlock } from "../core/wire";
import type { Assessment, Concern } from "./attention";
import { blocksKey, closedBlocks, outcomeText, renderBlock, showsLive, type BlockContext, type BlockForm } from "./blocks";
import { healthReading, healthTrend } from "./health";
import { messageTail, plainText } from "./text";

/** The palette colours the Monitor marks sessions with; see `.monitor-card[data-tone]`. */
export type Tone = "danger" | "warning" | "accent" | "success" | "muted";

/** Each concern's badge; working and idle sessions carry none, so the badges that remain stand out. */
export const CONCERN_BADGES: Record<Concern, { label: string; tone: Tone } | null> = {
  error: { label: "Error", tone: "danger" },
  needs_input: { label: "Needs input", tone: "accent" },
  unread: { label: "New reply", tone: "accent" },
  stuck: { label: "Stuck", tone: "warning" },
  retrying: { label: "Retrying", tone: "warning" },
  off_track: { label: "Off track", tone: "warning" },
  quiet: { label: "Quiet", tone: "warning" },
  ready: { label: "Ready", tone: "success" },
  working: null,
  idle: null,
};

/** Context use at which the footer flags the session as close to compaction. */
const CONTEXT_ALERT = 0.85;

export type CardHost = {
  catalog(): ModelCatalog | null;
  /** A workspace's short name, or null for the default workspace. */
  workspaceToken(path: string): string | null;
  /** Activates the session and shows its chat. */
  open(session: string): void;
  /** Puts `text` in the active session's composer and shows its chat. */
  compose(text: string): void;
  /** Sends `text` to the session as a prompt, steering the turn when one runs. */
  send(session: string, text: string): Promise<void>;
  stop(session: string): void;
  /** Closes an idle session, which moves it to History. */
  close(session: string): void;
};

export type CardData = {
  summary: SessionSummary;
  activity: ActivityView | undefined;
  assessment: Assessment;
  /** When the monitor model read the sessions for the note, or null without a check. */
  checkedAt: number | null;
  /** Whether this is the shared active session, whose draft belongs to the composer. */
  active: boolean;
};

export type LiveLine = {
  text: string;
  code: string | null;
  detail: string | null;
  tone: Tone;
  /** The line names the call in flight, so nothing else on the tile needs to. */
  call: boolean;
};

/**
 * What the session is doing right now, from the live state alone: the call in flight, the model's
 * phase, a retry, its agents, or that it is idle. A shell command is shown as code; a Code Mode
 * cell and other tools read as the transcript names them, with their one-line summary.
 */
export function liveLine(summary: SessionSummary, activity: SessionActivity | undefined): LiveLine {
  const running = summary.state === "running";
  const tool = activity?.tool ?? null;
  const status = activity?.status ?? null;
  const line = (text: string, tone: Tone = "muted", code: string | null = null, detail: string | null = null) => ({ text, code, detail, tone, call: false });
  if (status?.kind === "error") return line("Error", "danger");
  if (running && tool?.state === "running") {
    const call = tool.name === "exec_command" || tool.name === "write_stdin"
      ? line("Running", "muted", tool.summary || tool.name)
      : line(`Running ${toolLabel(tool.name, 0).toLowerCase()}`, "muted", null, tool.summary || null);
    return { ...call, call: true };
  }
  if (status?.kind === "retrying") return line(`Retrying ${status.next_attempt}/${status.max_attempts}`, "warning");
  const agents = activity?.agents.running ?? 0;
  if (status?.kind === "waiting_for_background_work" && agents > 0) return line(`Waiting on ${agents} agent${agents === 1 ? "" : "s"}`);
  const label = statusLabel(status);
  if (label) return line(label);
  return line(running ? "Working" : "Idle");
}

/**
 * The one piece of evidence a closed card shows when the monitor's blocks do not take its place:
 * the error that stopped it, the evidence for a concern, the reply waiting for them, or what a
 * running session is working toward. Exactly one is chosen, so a card never restates the task in
 * several forms.
 */
export type Evidence =
  | { kind: "error"; text: string }
  | { kind: "failed"; text: string; detail: string | null }
  | { kind: "message"; text: string; commentary: boolean }
  | { kind: "reason"; text: string }
  | { kind: "goal"; text: string }
  | { kind: "prompt"; text: string };

export function evidenceFor(summary: SessionSummary, activity: SessionActivity | undefined, assessment: Assessment): Evidence | null {
  const note = assessment.note;
  // A goal or prompt that only restates the title says nothing the header does not.
  const fresh = (text: string | null | undefined) => (text && !restates(summary.title, text) ? text : null);
  const message: Evidence | null = activity?.message ? { kind: "message", text: plainText(activity.message.text), commentary: activity.message.commentary } : null;
  const promptText = fresh(activity?.prompt?.text.replace(/\s+/g, " ").trim());
  const prompt: Evidence | null = promptText ? { kind: "prompt", text: promptText } : null;
  const goal = fresh(note?.goal);
  const fromNote: Evidence | null = goal ? { kind: "goal", text: goal } : null;
  if (assessment.concern === "error") {
    const error = activity?.error ?? (activity?.status?.kind === "error" ? activity.status.message : null);
    if (error) return { kind: "error", text: error };
    const tool = activity?.tool;
    if (tool?.state === "failed") return { kind: "failed", text: tool.summary || tool.name, detail: outcomeText(tool.outcome) };
    return message ?? prompt;
  }
  if ((assessment.concern === "stuck" || assessment.concern === "off_track") && note?.reason) return { kind: "reason", text: note.reason };
  if (summary.state !== "running") return message ?? fromNote ?? prompt;
  return fromNote ?? prompt;
}

/**
 * Whether `text` says little more than `title`: one contains the other, ignoring case and
 * punctuation, and a title found inside the text makes up most of it.
 */
export function restates(title: string, text: string) {
  const words = (value: string) => value.toLowerCase().replace(/[^\p{L}\p{N}]+/gu, " ").trim();
  const heading = words(title);
  const body = words(text);
  if (!heading || !body) return false;
  return heading.includes(body) || (body.includes(heading) && heading.length >= body.length * 0.6);
}


/**
 * Sets text only when it differs, so the once-a-second refresh leaves unchanged nodes alone.
 * Returns whether it changed.
 */
function setText(element: Element, text: string) {
  if (element.textContent === text) return false;
  element.textContent = text;
  return true;
}

const plural = (count: number, word: string, words = word + "s") => `${count} ${count === 1 ? word : words}`;
const percent = (part: number, whole: number) => Math.round(Math.min(1, part / whole) * 100);

/** "just now" or "5m ago", for when the monitor model read the session. */
function checkedAge(at: number, now: number) {
  const age = formatAge(at, now);
  return age === "now" ? "just now" : `${age} ago`;
}

/**
 * One live session on the Monitor, as a compact tile. A fixed frame says what it is and whether it
 * needs the user, what it is doing now, and its actions; between them sit the monitor's blocks, or
 * one piece of deterministic evidence. Details opens in place with everything else known about it. A card is kept for as long as its session is live and updated in
 * place, so an open reply box or Details keeps its state while the session changes around it.
 */
export class MonitorCard {
  readonly element: HTMLElement;
  private data: CardData | null = null;
  private detailsOpen = false;
  /** The closed message shows only its end, so it reads as cut off whatever the layout. */
  private messageShortened = false;
  private sending = false;
  private sphere: HTMLCanvasElement | null = null;
  private readonly reply: HTMLFormElement;
  private readonly replyText: HTMLTextAreaElement;
  private readonly measure: ResizeObserver;
  /** Questions' options answer through the reply box, as the primary action does. */
  private readonly blockContext = (activity: SessionActivity | undefined, answerable: boolean): BlockContext =>
    ({ activity, answer: answerable ? (option) => this.answer(option) : null });

  constructor(readonly session: string, private readonly host: CardHost) {
    const card = document.createElement("article");
    card.className = "monitor-card";
    card.dataset.session = session;
    card.tabIndex = 0;
    const detailsId = `monitor-details-${session}`;
    card.innerHTML = `
      <header class="card-head">
        <span class="card-marker" aria-hidden="true"></span>
        <h3 class="card-title"></h3>
        <span class="card-badge"></span>
        <span class="card-health" role="img" hidden><span class="health-trend"></span><span class="health-bar">${"<span></span>".repeat(5)}</span></span>
      </header>
      <p class="card-live"><span class="card-now"><span class="card-now-text"></span><code class="card-now-code"></code><span class="card-now-detail"></span></span><span class="card-time"></span></p>
      <div class="card-evidence">
        <span class="evidence-mark" role="img"></span>
        <div class="evidence-body">
          <p class="evidence-text"><span class="evidence-inner"></span></p>
          <span class="evidence-meta"><span class="evidence-age"></span></span>
        </div>
      </div>
      <div class="card-blocks">
        <div class="blocks-list"></div>
        <span class="evidence-meta blocks-meta"><span class="evidence-source" title="From the monitor">${glyph("sparkles")}</span><span class="blocks-age"></span></span>
      </div>
      <div class="card-details" id="${detailsId}" hidden>
        <section class="details-monitor">
          <p class="details-goal"><span class="note-mark" title="From the monitor">${glyph("sparkles")}</span><span class="details-goal-text"></span></p>
          <p class="details-checked"><span class="note-earlier" title="The session has moved on since the monitor read it">earlier</span><span class="details-checked-text"></span></p>
          <div class="details-blocks"></div>
          <p class="details-reason"></p>
          <p class="details-suggestion"></p>
        </section>
        <section class="details-part details-prompt"><h4>Prompt</h4><p></p></section>
        <section class="details-part details-message"><h4>Latest reply</h4><p></p></section>
        <p class="details-line details-plan"></p>
        <p class="details-line details-counters"></p>
        <p class="details-line details-where"></p>
      </div>
      <footer class="card-foot">
        <span class="card-facts"><span class="model-dot"></span><span class="card-model"></span><span class="card-workspace"></span><span class="card-tags"></span></span>
        <button type="button" class="card-action card-primary"><span></span></button>
        <button type="button" class="icon-button small card-details-toggle" aria-expanded="false" aria-controls="${detailsId}" aria-label="Details" title="Details">${glyph("chevron-down")}</button>
        <button type="button" class="icon-button small card-more-actions" aria-haspopup="menu" aria-label="Session actions" title="Session actions">${glyph("more")}</button>
      </footer>
      <form class="card-reply" hidden>
        <textarea rows="2" aria-label="Reply" placeholder="Message the agent"></textarea>
        <p class="card-reply-error" role="alert" hidden></p>
        <div class="card-reply-bar"><button type="button" class="card-link insert-suggestion" hidden>Insert suggestion</button><span class="card-reply-hint">Enter to send · Esc to cancel</span><button type="button" class="card-link cancel-reply">Cancel</button><button type="submit" class="card-send">Send</button></div>
      </form>`;
    this.element = card;
    this.reply = card.querySelector(".card-reply")!;
    this.replyText = this.reply.querySelector("textarea")!;
    this.bind();
    // Whether a message is cut off depends on layout: the text's height, which changes with the text
    // and the card's width, against the box's limit, which changes with the evidence shown and the
    // viewport. Both settle when the card is attached.
    this.measure = new ResizeObserver(() => this.measureMessage());
    this.measure.observe(card.querySelector(".evidence-inner")!);
    this.measure.observe(card.querySelector(".evidence-text")!);
  }

  update(data: CardData, clock: number, now: number) {
    this.data = data;
    const { summary, assessment, active } = data;
    const card = this.element;
    const running = summary.state === "running";
    const badge = CONCERN_BADGES[assessment.concern];
    card.dataset.concern = assessment.concern;
    card.dataset.tone = badge?.tone ?? (running ? "accent" : "muted");
    card.dataset.state = summary.state;
    card.classList.toggle("active", active);

    const title = summary.title || "New chat";
    const heading = card.querySelector<HTMLElement>(".card-title")!;
    setText(heading, title);
    heading.title = title;
    const badgeElement = card.querySelector<HTMLElement>(".card-badge")!;
    badgeElement.hidden = badge === null;
    setText(badgeElement, badge?.label ?? "");
    card.setAttribute("aria-label", [title, badge?.label ?? (running ? "working" : "idle"), active ? "active session" : ""].filter(Boolean).join(", "));
    this.renderMarker(running);
    this.renderHealth(assessment);

    this.renderLive(data, clock, now);
    this.renderBody(data, now);
    this.renderFooter(data);
    if (this.detailsOpen) this.renderDetails(data, now);
  }

  private renderMarker(running: boolean) {
    const marker = this.element.querySelector(".card-marker")!;
    if (running && !this.sphere) {
      this.sphere = createSphere(12, 14);
      marker.append(this.sphere);
    } else if (!running && this.sphere) {
      this.sphere.remove();
      this.sphere = null;
    }
  }

  /**
   * The monitor's health score as a five-segment gauge, with a mark when it moved a band's width
   * since the previous check. Like the rest of the note, it dims once the session has moved on.
   */
  private renderHealth({ note, noteEarlier }: Assessment) {
    const gauge = this.element.querySelector<HTMLElement>(".card-health")!;
    const health = note?.health ?? null;
    gauge.hidden = health === null;
    if (note === null || health === null) return;
    const { segments, tone, label } = healthReading(health);
    const trend = healthTrend(note);
    gauge.dataset.tone = tone;
    gauge.dataset.segments = String(segments);
    gauge.classList.toggle("earlier", noteEarlier);
    const was = note.previous_health === null ? "" : `, was ${note.previous_health}`;
    const description = `Health ${health} of 100, ${label}${was}${noteEarlier ? ", earlier" : ""}`;
    gauge.setAttribute("aria-label", description);
    gauge.title = note.reason ? `${description}. ${plainText(note.reason)}` : description;
    const mark = gauge.querySelector<HTMLElement>(".health-trend")!;
    mark.hidden = trend === null;
    if (mark.dataset.trend !== (trend ?? "")) {
      mark.dataset.trend = trend ?? "";
      mark.innerHTML = trend === null ? "" : glyph(trend === "up" ? "arrow-up" : "arrow-down");
    }
  }

  private renderLive({ summary, activity, assessment }: CardData, clock: number, now: number) {
    const element = this.element.querySelector<HTMLElement>(".card-live")!;
    const line = liveLine(summary, activity);
    const quiet = assessment.concern === "quiet";
    element.dataset.tone = quiet ? "warning" : line.tone;
    element.classList.toggle("idle", summary.state !== "running" && line.tone === "muted");
    setText(element.querySelector(".card-now-text")!, quiet ? "No recent activity" : line.text);
    const code = element.querySelector<HTMLElement>(".card-now-code")!;
    code.hidden = !line.code;
    setText(code, line.code ?? "");
    const detail = element.querySelector<HTMLElement>(".card-now-detail")!;
    detail.hidden = !line.detail;
    setText(detail, line.detail ?? "");
    element.querySelector<HTMLElement>(".card-now")!.title = [line.text, line.code, line.detail].filter(Boolean).join(" ");

    // A running session counts its turn; any other counts the time since anything happened, which
    // is all the live state can say about how long it has waited.
    const time = element.querySelector<HTMLElement>(".card-time")!;
    const started = activity?.turnStartedAt ?? null;
    const age = formatAge(summary.last_activity_unix_ms, now);
    setText(time, summary.state === "running" && started !== null
      ? `turn ${formatTurnDuration((clock - started) * 1e6)}`
      : age === "now" ? "just now" : `${age} ago`);
  }

  /**
   * The tile's body between the live line and the footer. A deterministic error always shows as
   * itself. Otherwise the monitor's blocks take the body while its note still describes the
   * session: always for a running one (dimmed once the session has moved on), and for an idle one
   * only while nothing has happened since the check. Without them the tile falls back to one piece
   * of deterministic evidence.
   */
  private renderBody(data: CardData, now: number) {
    const { summary, activity, assessment } = data;
    const note = assessment.note;
    const usable = assessment.concern !== "error" && note !== null && (summary.state === "running" || !assessment.noteEarlier);
    const blocks = usable ? closedBlocks(MonitorCard.closedOrder(note.blocks, data), activity) : [];
    this.renderBlocks(blocks, data, now);
    this.renderEvidence(blocks.length ? null : evidenceFor(summary, activity, assessment), data, now);
    this.measureMessage();
    // Open Details shows the blocks and any evidence it holds in full, so the closed forms give way;
    // an error, which Details does not repeat, stays.
    const evidence = this.element.querySelector<HTMLElement>(".card-evidence")!;
    const repeated = evidence.dataset.kind !== "error" && evidence.dataset.kind !== "failed";
    this.element.querySelector<HTMLElement>(".card-blocks")!.hidden ||= this.detailsOpen;
    evidence.hidden ||= this.detailsOpen && repeated;
  }

  /**
   * The note's blocks in the order the closed tile considers them. A live tool block showing the
   * call the live line already names would only repeat it. A stuck or off-track session leads with
   * the note's warning or danger text, so the problem shows without Details whatever the model put
   * first.
   */
  private static closedOrder(blocks: readonly StatusBlock[], { summary, activity, assessment }: CardData): StatusBlock[] {
    const namesCall = liveLine(summary, activity).call;
    const candidates = blocks.filter((block) => !(namesCall && block.type === "live" && block.source === "tool"));
    if (assessment.concern !== "stuck" && assessment.concern !== "off_track") return candidates;
    const problem = candidates.find((block) => block.type === "text" && (block.tone === "warning" || block.tone === "danger"));
    return problem ? [problem, ...candidates.filter((block) => block !== problem)] : candidates;
  }

  /**
   * Whether a question's options can answer it now: the session is idle and nothing has happened
   * since the check, so the question is still the one the agent is asking.
   */
  private static answerable({ summary, assessment }: CardData) {
    return summary.state === "idle" && !assessment.noteEarlier;
  }

  private renderBlocks(blocks: readonly StatusBlock[], data: CardData, now: number) {
    const { activity, assessment, checkedAt } = data;
    const element = this.element.querySelector<HTMLElement>(".card-blocks")!;
    element.hidden = blocks.length === 0;
    element.classList.toggle("earlier", assessment.noteEarlier);
    this.drawBlocks(element.querySelector<HTMLElement>(".blocks-list")!, blocks, "compact", activity, MonitorCard.answerable(data));
    const age = element.querySelector<HTMLElement>(".blocks-meta")!;
    age.hidden = checkedAt === null;
    setText(element.querySelector(".blocks-age")!, checkedAt === null ? "" : "checked " + checkedAge(checkedAt, now) + (assessment.noteEarlier ? " · earlier" : ""));
  }

  /** Redraws blocks only when they or the live data they show changed, so the clock tick leaves them alone. */
  private drawBlocks(container: HTMLElement, blocks: readonly StatusBlock[], form: BlockForm, activity: SessionActivity | undefined, answerable: boolean) {
    const key = `${answerable}:${blocksKey(blocks, activity)}`;
    if (container.dataset.key === key) return;
    container.dataset.key = key;
    const context = this.blockContext(activity, answerable);
    container.replaceChildren(...blocks.flatMap((block) => renderBlock(block, form, context) ?? []));
    for (const box of container.querySelectorAll<HTMLElement>(".clip-box")) {
      this.measure.observe(box);
      this.measure.observe(box.querySelector(".clip-inner")!);
    }
  }

  private renderEvidence(evidence: Evidence | null, { assessment, checkedAt }: CardData, now: number) {
    const element = this.element.querySelector<HTMLElement>(".card-evidence")!;
    element.hidden = evidence === null;
    this.messageShortened = false;
    if (!evidence) {
      delete element.dataset.kind;
      return;
    }
    element.dataset.kind = evidence.kind;
    const fromMonitor = evidence.kind === "reason" || evidence.kind === "goal";
    element.classList.toggle("earlier", fromMonitor && assessment.noteEarlier);
    element.classList.toggle("commentary", evidence.kind === "message" && evidence.commentary);

    const mark = element.querySelector<HTMLElement>(".evidence-mark")!;
    if (mark.dataset.key !== evidence.kind) {
      mark.dataset.key = evidence.kind;
      mark.innerHTML = evidence.kind === "error" ? glyph("alert")
        : evidence.kind === "failed" ? glyph("close")
        : evidence.kind === "reason" || evidence.kind === "goal" ? glyph("sparkles")
        : evidence.kind === "prompt" ? glyph("user")
        : "";
      mark.setAttribute("aria-label", { error: "error", failed: "failed", reason: "the monitor's evidence", goal: "the monitor's goal", prompt: "your prompt", message: "latest reply" }[evidence.kind]);
    }

    const [text, detail] =
      evidence.kind === "failed" ? [evidence.text, evidence.detail]
      : evidence.kind === "reason" || evidence.kind === "goal" ? [plainText(evidence.text), null]
      : [evidence.text, null];
    let shown = detail ? `${text} · ${detail}` : text;
    if (evidence.kind === "message") {
      const tail = messageTail(text);
      this.messageShortened = tail !== null;
      // Paragraphs only break lines here, so the few lines shown are all text.
      shown = (tail ?? text).replace(/\n{2,}/g, "\n");
    }
    const box = element.querySelector<HTMLElement>(".evidence-text")!;
    box.title = evidence.kind === "message" ? "" : shown;
    setText(element.querySelector(".evidence-inner")!, shown);

    const age = element.querySelector<HTMLElement>(".evidence-age")!;
    age.hidden = !fromMonitor || checkedAt === null;
    setText(age, checkedAt === null ? "" : "checked " + checkedAge(checkedAt, now) + (assessment.noteEarlier ? " · earlier" : ""));
    element.querySelector<HTMLElement>(".evidence-meta")!.hidden = age.hidden;
  }

  /**
   * Marks a closed message as cut off when it is shortened, or when its box, which keeps the last
   * lines, hides earlier ones; Details holds all of it. Reads layout, so it runs when text changes
   * and when sizes change.
   */
  private measureMessage() {
    const element = this.element.querySelector<HTMLElement>(".card-evidence")!;
    const message = element.dataset.kind === "message";
    const box = element.querySelector<HTMLElement>(".evidence-text")!;
    const inner = element.querySelector<HTMLElement>(".evidence-inner")!;
    element.classList.toggle("clipped", message && (this.messageShortened || inner.offsetHeight > box.clientHeight + 1));
    for (const clip of this.element.querySelectorAll<HTMLElement>(".card-blocks .clip-box")) {
      const text = clip.querySelector<HTMLElement>(".clip-inner")!;
      clip.classList.toggle("clipped", clip.dataset.shortened === "true" || text.offsetHeight > clip.clientHeight + 1);
    }
  }

  private renderFooter({ summary, activity, active }: CardData) {
    const card = this.element;
    const label = this.host.catalog()?.models.find((model) => model.id === summary.model)?.label ?? summary.model;
    setText(card.querySelector(".card-model")!, label);
    card.querySelector<HTMLElement>(".model-dot")!.style.background = modelColor(summary.model);
    const workspace = card.querySelector<HTMLElement>(".card-workspace")!;
    const token = this.host.workspaceToken(summary.workspace);
    workspace.hidden = token === null;
    setText(workspace, token ?? "");
    workspace.title = summary.workspace;

    // Only exceptional facts earn a tag; routine counters live in Details.
    const tags: [string, Tone][] = [];
    if (summary.has_draft) tags.push(["Draft", "muted"]);
    if (activity?.queued) tags.push([`${activity.queued} queued`, "muted"]);
    const context = activity?.context;
    if (context && context.window_tokens > 0 && context.active_tokens / context.window_tokens >= CONTEXT_ALERT) {
      tags.push([`${percent(context.active_tokens, context.window_tokens)}% context`, "danger"]);
    }
    const list = card.querySelector<HTMLElement>(".card-tags")!;
    const key = JSON.stringify(tags);
    if (list.dataset.key !== key) {
      list.dataset.key = key;
      list.replaceChildren(...tags.map(([text, tone]) => {
        const tag = document.createElement("span");
        tag.className = "card-chip";
        tag.dataset.tone = tone;
        tag.textContent = text;
        return tag;
      }));
    }

    const primary = card.querySelector<HTMLButtonElement>(".card-primary")!;
    const reply = this.canReply();
    primary.dataset.action = reply ? "reply" : "open";
    primary.hidden = reply && !this.reply.hidden;
    setText(primary.querySelector("span")!, !reply ? "Open" : active ? "Reply in chat" : summary.state === "running" ? "Steer" : "Reply");
    if (!reply && !this.reply.hidden) this.closeReply();
  }

  /**
   * Replying writes the session's shared draft, so a draft someone is writing is left alone; the
   * active session's draft belongs to the composer, where the reply goes instead.
   */
  private canReply() {
    const data = this.data;
    return data !== null && (data.active || !data.summary.has_draft || this.sending);
  }

  /**
   * Everything the closed card leaves out: the monitor's goal, all its blocks in full, its reason
   * and suggestion, then the prompt, the reply, the plan, and the counters.
   */
  private renderDetails(data: CardData, now: number) {
    const { summary, activity, assessment, checkedAt } = data;
    const details = this.element.querySelector<HTMLElement>(".card-details")!;
    const note = assessment.note;
    const monitor = details.querySelector<HTMLElement>(".details-monitor")!;
    monitor.hidden = note === null;
    if (note) {
      monitor.classList.toggle("earlier", assessment.noteEarlier);
      details.querySelector<HTMLElement>(".details-goal")!.hidden = !note.goal;
      setText(details.querySelector(".details-goal-text")!, plainText(note.goal ?? ""));
      setText(details.querySelector(".details-checked-text")!, checkedAt === null ? "" : "checked " + checkedAge(checkedAt, now));
      const blocks = details.querySelector<HTMLElement>(".details-blocks")!;
      blocks.hidden = note.blocks.length === 0;
      this.drawBlocks(blocks, note.blocks, "full", activity, MonitorCard.answerable(data));
      const reason = note.attention === "ok" ? null : note.reason;
      const reasonElement = details.querySelector<HTMLElement>(".details-reason")!;
      reasonElement.hidden = !reason;
      setText(reasonElement, plainText(reason ?? ""));
      const suggestion = details.querySelector<HTMLElement>(".details-suggestion")!;
      suggestion.hidden = !note.suggestion;
      setText(suggestion, note.suggestion ?? "");
    }
    // A live block already shows its source in full, so its section is left out.
    const live = note?.blocks ?? [];
    const prompt = details.querySelector<HTMLElement>(".details-prompt")!;
    prompt.hidden = !activity?.prompt || showsLive(live, "prompt");
    setText(prompt.querySelector("p")!, activity?.prompt?.text ?? "");
    const message = details.querySelector<HTMLElement>(".details-message")!;
    message.hidden = !activity?.message || showsLive(live, "message");
    setText(message.querySelector("p")!, plainText(activity?.message?.text ?? ""));

    const plan = activity?.plan;
    const planElement = details.querySelector<HTMLElement>(".details-plan")!;
    planElement.hidden = !plan || plan.total === 0 || showsLive(live, "plan");
    setText(planElement, plan ? `Plan ${plan.done}/${plan.total}${plan.current ? " · " + plan.current : ""}` : "");
    const counters: string[] = [];
    if (activity) {
      const { turn, agents, queued, context } = activity;
      counters.push(plural(turn.tools, "call"));
      if (turn.failed) counters.push(`${turn.failed} failed`);
      if (turn.patches) counters.push(plural(turn.patches, "patch", "patches"));
      if (agents.total) counters.push(`${agents.running}/${agents.total} agents running`);
      if (queued) counters.push(`${queued} queued`);
      if (context && context.window_tokens > 0) {
        counters.push(`${percent(context.active_tokens, context.window_tokens)}% context (${formatTokens(context.active_tokens)} tokens)`);
      }
    }
    const countersElement = details.querySelector<HTMLElement>(".details-counters")!;
    countersElement.hidden = counters.length === 0;
    setText(countersElement, counters.join(" · "));
    const model = this.host.catalog()?.models.find((candidate) => candidate.id === summary.model)?.label ?? summary.model;
    setText(details.querySelector(".details-where")!, `${model} · ${summary.workspace}`);
  }

  private toggleDetails(open = !this.detailsOpen) {
    this.detailsOpen = open;
    this.element.classList.toggle("details-open", open);
    this.element.querySelector<HTMLElement>(".card-details")!.hidden = !open;
    this.element.querySelector(".card-details-toggle")!.setAttribute("aria-expanded", String(open));
    if (!this.data) return;
    if (open) this.renderDetails(this.data, Date.now());
    this.renderBody(this.data, Date.now());
  }

  private openActions(anchor: HTMLElement) {
    const data = this.data;
    if (!data) return;
    const title = data.summary.title || "New chat";
    openMenu(anchor, [
      { label: "Open", icon: glyph("message"), run: () => this.host.open(this.session) },
      data.summary.state === "running"
        ? { label: "Stop", icon: glyph("stop"), danger: true, run: () => {
          if (confirm(`Stop the running turn of “${title}”?`)) this.host.stop(this.session);
        } }
        : { label: "Close", icon: glyph("close"), run: () => this.host.close(this.session) },
    ], "Session actions");
  }

  private openReply() {
    const data = this.data;
    if (!data) return;
    if (data.active) {
      this.host.compose("");
      return;
    }
    this.reply.hidden = false;
    this.element.querySelector<HTMLElement>(".card-primary")!.hidden = true;
    this.reply.querySelector<HTMLElement>(".insert-suggestion")!.hidden = !data.assessment.note?.suggestion;
    this.replyText.focus();
    this.replyText.setSelectionRange(this.replyText.value.length, this.replyText.value.length);
  }

  /** Puts the monitor's suggestion where the cursor is, for editing; nothing is sent. */
  private insertSuggestion() {
    const suggestion = this.data?.assessment.note?.suggestion;
    if (!suggestion) return;
    this.replyText.setRangeText(suggestion, this.replyText.selectionStart, this.replyText.selectionEnd, "end");
    this.fitReply();
    this.replyText.focus();
  }

  /**
   * Puts a question's option in the reply for editing, as Insert suggestion does: in the open reply
   * box, else a newly opened one, or the composer for the active session. A session whose shared
   * draft someone else is writing is opened instead. Nothing is sent.
   */
  private answer(option: string) {
    const data = this.data;
    if (!data) return;
    if (data.active) {
      this.host.compose(option);
      return;
    }
    if (!this.canReply()) {
      this.host.open(this.session);
      return;
    }
    if (this.reply.hidden) this.openReply();
    this.replyText.setRangeText(option, this.replyText.selectionStart, this.replyText.selectionEnd, "end");
    this.fitReply();
    this.replyText.focus();
  }

  /** Grows the reply box with its text, up to a limit after which it scrolls. */
  private fitReply() {
    this.replyText.style.height = "auto";
    this.replyText.style.height = `${Math.min(this.replyText.scrollHeight, 160)}px`;
  }

  private closeReply() {
    this.reply.hidden = true;
    this.element.querySelector<HTMLElement>(".card-reply-error")!.hidden = true;
    if (this.data) this.renderFooter(this.data);
  }

  private async send() {
    const text = this.replyText.value.trim();
    if (!text || this.sending) return;
    // The active session's draft belongs to the composer, so a reply begun before the session
    // became active goes there instead of replacing that draft.
    if (this.data?.active) {
      this.replyText.value = "";
      this.closeReply();
      this.host.compose(text);
      return;
    }
    this.sending = true;
    const button = this.reply.querySelector<HTMLButtonElement>(".card-send")!;
    button.disabled = true;
    const error = this.reply.querySelector<HTMLElement>(".card-reply-error")!;
    error.hidden = true;
    try {
      await this.host.send(this.session, text);
      this.replyText.value = "";
      this.closeReply();
      this.element.focus({ preventScroll: true });
    } catch (failure) {
      error.textContent = describeError(failure);
      error.hidden = false;
    } finally {
      this.sending = false;
      button.disabled = false;
    }
  }

  private bind() {
    const card = this.element;
    card.addEventListener("click", (event) => {
      const target = event.target as HTMLElement;
      if (target.closest("button, a, textarea, form, .card-details") || getSelection()?.toString()) return;
      this.host.open(this.session);
    });
    card.addEventListener("keydown", (event) => {
      if (event.key === "Enter" && event.target === card) {
        event.preventDefault();
        this.host.open(this.session);
      }
    });
    card.querySelector(".card-details-toggle")!.addEventListener("click", () => this.toggleDetails());
    const actions = card.querySelector<HTMLElement>(".card-more-actions")!;
    actions.addEventListener("click", () => this.openActions(actions));
    const primary = card.querySelector<HTMLElement>(".card-primary")!;
    primary.addEventListener("click", () => {
      if (primary.dataset.action === "open") this.host.open(this.session);
      else this.openReply();
    });
    card.querySelector(".insert-suggestion")!.addEventListener("click", () => this.insertSuggestion());
    card.querySelector(".cancel-reply")!.addEventListener("click", () => {
      this.closeReply();
      card.focus({ preventScroll: true });
    });
    this.reply.addEventListener("submit", (event) => {
      event.preventDefault();
      void this.send();
    });
    this.replyText.addEventListener("keydown", (event) => {
      if (event.key === "Enter" && !event.shiftKey && !event.isComposing) {
        event.preventDefault();
        void this.send();
      } else if (event.key === "Escape") {
        event.preventDefault();
        event.stopPropagation();
        this.closeReply();
        card.focus({ preventScroll: true });
      }
    });
    this.replyText.addEventListener("input", () => this.fitReply());
  }
}
