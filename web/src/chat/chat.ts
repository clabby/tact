import { FileDiff } from "@pierre/diffs";
import { ApiError, errorMessage } from "../core/api-client";
import { firstLine, formatAge, formatDuration, modelColor } from "../core/format";
import { glyph } from "../ui/glyphs";
import { toast } from "../ui/toast";
import { openLightbox } from "./lightbox";
import { renderMarkdown } from "../core/markdown";
import type { TranscriptData } from "../core/store";
import type { Theme } from "../core/theme";
import { promptParts } from "./user-prompt";
import { presentDetail, TOOL_DEFAULT_OPEN, toolLabel } from "./tool-detail";
import { renderThread, type Participants } from "./directed";
import { explorationLabel, type ToolEntry } from "./exploration";
import { resultText, turnMarkdown, turnOutcome, type TurnOutcome } from "./outcome";
import { planTurn, segmentTurns, type LogItem, type Turn, type TurnPlan } from "./turns";
import { firstUnseen, latestEntry, newSinceLabel, readSeen, writeSeen, type Seen } from "./seen";
import { spawnedAgentId } from "./agent-links";
import type { Subagent, ToolDetail, ToolOutcome, WireEntry } from "../core/wire";

/** What the surrounding turn says about an entry; a change re-renders the entry. */
type EntryContext = { role?: "answer" | "narration"; recovered?: true; outcome?: TurnOutcome; turn?: number };

type Rendered = { element: HTMLElement; revision: number; context: string };

/** A turn's DOM: the section, the row that folds its work log, the log, and the log's groups. */
type TurnView = { section: HTMLElement; fold: HTMLElement; log: HTMLElement; groups: Map<number, HTMLElement>; batches: Map<number, HTMLElement> };

/** What a transcript view shows: a session's or a subagent's entries, and where details live. */
export type TranscriptSource = {
  /** Identifies the transcript; a different key starts a fresh view. */
  key: string;
  data: TranscriptData;
  detail(entry: number): Promise<ToolDetail>;
  /** The URL of an image attached to a user entry, when this transcript has them. */
  image?(entry: number, index: number): string;
  /** The transcript's owner and the session's agents, which name the parties of agent messages. */
  participants(): Participants;
  /** A shareable link to an entry, when this transcript has them. */
  link?(entry: number): string;
  /** The session's agents, listed under the calls that started them. */
  agents?(): Subagent[];
  /** One line on what an agent is doing or did. */
  agentNote?(agent: Subagent): string;
  openAgent?(agent: number): void;
};

export type TranscriptOptions = {
  empty?: { title: string; body: string };
  /** Shows the review of the session's changes. */
  openReview?(): void;
  /** Remembers per transcript the last entry the reader saw, to mark what arrived since. */
  seen?: Storage;
  /** Called after turns were laid out again, so summaries of them (the prompt rail) can follow. */
  laidOut?(): void;
};

/** Distance from the bottom, in pixels, within which the transcript keeps following new output. */
const FOLLOW_THRESHOLD = 72;

/** How long the page must be in front of the reader before what it shows counts as seen. */
const SEEN_AFTER_MS = 3000;

/** Kinds whose rows carry a time and a link in the gutter. */
const GUTTER_KINDS = new Set(["user", "assistant", "reasoning", "tool", "directed_message", "error", "compaction_failed", "interrupted"]);

/**
 * The transcript of the active session, laid out as turns: the prompt, a work log that folds once
 * the turn is over, the answer, and an outcome strip. Entry nodes are keyed by id and re-render
 * only when their revision or their role in the turn changes; a turn is laid out again only when
 * its entries or their states change, batched into one animation frame. Markdown of settled
 * entries is rendered when the entry nears the viewport, so opening a long session costs little
 * more than its visible part.
 */
export class Transcript {
  private readonly list: HTMLElement;
  private rendered = new Map<number, Rendered>();
  private dirty = new Set<number>();
  /** Turns to lay out again in the next frame although none of their entries changed. */
  private dirtyTurns = new Set<number>();
  private relayoutAll = true;
  private frame = 0;
  private source: TranscriptSource | null = null;
  private following = true;
  private readonly lazy: IntersectionObserver;
  /**
   * Tool calls and agent threads whose open state differs from their default, patches shown
   * untruncated, failed commands showing their full output, and long agent messages shown in full
   * (keyed "entry:message").
   */
  private readonly toggled = new Set<number>();
  private readonly showAll = new Set<number>();
  private readonly fullOutput = new Set<number>();
  private readonly fullMessages = new Set<string>();
  private readonly diffs = new Map<number, FileDiff[]>();
  private readonly lazyDetail: IntersectionObserver;
  private readonly lazyAgents: IntersectionObserver;
  private readonly details = new Map<number, { revision: number; detail: Promise<ToolDetail> }>();
  /** The agents each call started, once known. */
  private readonly agentIds = new Map<number, Promise<number | null>>();
  private agentsFrame = 0;

  private turns: Turn[] = [];
  private readonly turnOf = new Map<number, Turn>();
  private readonly turnByKey = new Map<number, Turn>();
  private readonly shapes = new Map<number, string>();
  private readonly plans = new Map<number, TurnPlan>();
  private readonly views = new Map<number, TurnView>();
  private readonly contexts = new Map<number, EntryContext>();
  private readonly signatures = new Map<number, string>();
  /** The reader's fold choice per turn, which outlives re-renders; absent turns use the default. */
  private readonly folds = new Map<number, boolean>();
  private readonly openGroups = new Set<number>();

  private seen: Seen | null = null;
  private newSince: { id: number; label: string } | null = null;
  private readonly newSinceMarker: HTMLElement;
  private observing = false;
  private seenTimer = 0;

  /** Drives the clocks of running tool calls; runs only while one is on screen. */
  private clockTimer = 0;
  /** Ages relative times ("12m ago") while the transcript is on the page. */
  private ageTimer = 0;

  constructor(
    private readonly scroller: HTMLElement,
    private readonly jump: HTMLButtonElement,
    private readonly theme: () => Theme,
    private readonly options: TranscriptOptions = {},
  ) {
    this.list = document.createElement("div");
    this.list.className = "transcript";
    this.list.setAttribute("role", "log");
    this.list.setAttribute("aria-live", "off");
    this.list.setAttribute("aria-label", "Transcript");
    scroller.append(this.list);
    this.newSinceMarker = document.createElement("div");
    this.newSinceMarker.className = "new-since";
    this.newSinceMarker.setAttribute("role", "separator");

    this.lazy = new IntersectionObserver((records) => {
      for (const record of records) {
        if (!record.isIntersecting) continue;
        const element = record.target as HTMLElement;
        this.lazy.unobserve(element);
        const entry = this.entryOf(element);
        if (entry) this.renderBody(element, entry, true);
      }
    }, { root: scroller, rootMargin: "1200px 0px" });

    this.lazyDetail = new IntersectionObserver((records) => {
      for (const record of records) {
        if (!record.isIntersecting) continue;
        const container = record.target as HTMLElement;
        this.lazyDetail.unobserve(container);
        const entry = this.entryOf(container);
        if (entry?.kind === "tool") void this.renderDetail(container, entry);
      }
    }, { root: scroller, rootMargin: "1200px 0px" });

    this.lazyAgents = new IntersectionObserver((records) => {
      for (const record of records) {
        if (!record.isIntersecting) continue;
        const list = record.target as HTMLElement;
        this.lazyAgents.unobserve(list);
        const entry = this.entryOf(list);
        if (entry?.kind === "tool") void this.resolveAgents(list, entry);
      }
    }, { root: scroller, rootMargin: "600px 0px" });

    // Following stops only when the reader scrolls up, so programmatic and smooth scrolls toward
    // the end never cancel it; reaching the end again resumes it.
    let lastTop = 0;
    scroller.addEventListener("scroll", () => {
      const top = scroller.scrollTop;
      const distance = scroller.scrollHeight - top - scroller.clientHeight;
      if (distance < FOLLOW_THRESHOLD) this.setFollowing(true);
      else if (top < lastTop) this.setFollowing(false);
      lastTop = top;
    }, { passive: true });
    // Late layout (Markdown upgrades, highlighting, images) must not leave a follower short of the end.
    new ResizeObserver(() => {
      if (this.following) scroller.scrollTop = scroller.scrollHeight;
    }).observe(this.list);
    jump.addEventListener("click", () => this.scrollToEnd("smooth"));
    this.list.addEventListener("click", (event) => this.handleClick(event));
    if (options.seen) {
      document.addEventListener("visibilitychange", () => this.attentionChanged());
      addEventListener("focus", () => this.attentionChanged());
      addEventListener("blur", () => this.attentionChanged());
    }
  }

  /** Shows `source`, reusing nodes whose revision is unchanged when it is the same transcript. */
  show(source: TranscriptSource | null) {
    const same = source !== null && source.key === this.source?.key;
    if (!same && this.observing) this.stopObserving();
    this.source = source;
    if (!same) {
      this.rendered.clear();
      this.toggled.clear();
      this.showAll.clear();
      this.fullOutput.clear();
      this.fullMessages.clear();
      for (const id of [...this.diffs.keys()]) this.releaseDiffs(id);
      this.lazyDetail.disconnect();
      this.lazyAgents.disconnect();
      this.details.clear();
      this.agentIds.clear();
      this.lazy.disconnect();
      this.turns = [];
      for (const map of [this.turnOf, this.turnByKey, this.shapes, this.plans, this.views, this.contexts, this.signatures, this.folds]) map.clear();
      this.openGroups.clear();
      this.dirtyTurns.clear();
      this.relayoutAll = true;
      this.list.replaceChildren();
      this.following = true;
      this.seen = source && this.options.seen ? readSeen(this.options.seen, source.key) : null;
      this.newSince = null;
    }
    if (!source) {
      const empty = this.options.empty ?? { title: "No live session", body: "Start a new chat or pick one from the sidebar." };
      this.list.replaceChildren(emptyState(empty.title, empty.body));
      return;
    }
    const data = source.data;
    for (const [id, rendered] of this.rendered) {
      if (!data.entries.has(id)) {
        rendered.element.remove();
        this.rendered.delete(id);
        this.relayoutAll = true;
      }
    }
    for (const id of data.order) this.dirty.add(id);
    if (!same) this.markUnseen();
    this.flush();
    if (!same) {
      this.scrollToEnd("instant");
      this.attentionChanged();
    }
  }

  entryChanged(id: number) {
    this.dirty.add(id);
    this.schedule();
  }

  /** Re-renders settled Markdown, e.g. after the theme changed code highlighting. */
  rerender() {
    if (!this.source) return;
    for (const rendered of this.rendered.values()) rendered.revision = -1;
    this.show(this.source);
  }

  /** The agents roster or an agent's activity changed; refreshes the agent lists under calls. */
  agentsChanged() {
    this.agentsFrame ||= requestAnimationFrame(() => {
      this.agentsFrame = 0;
      for (const list of this.list.querySelectorAll<HTMLElement>(".tool-agents[data-agents]")) this.renderAgentList(list);
    });
  }

  /** The entries find-in-transcript searches. */
  data(): TranscriptData | null {
    return this.source?.data ?? null;
  }

  /**
   * Brings an entry into view, unfolding its turn and opening its group as needed, and returns its
   * element. With `flash` it is briefly highlighted.
   */
  reveal(id: number, options: { flash?: boolean; scroll?: boolean } = {}): HTMLElement | null {
    if (!this.source?.data.entries.has(id)) return null;
    this.flush();
    const turn = this.turnOf.get(id);
    const plan = turn && this.plans.get(turn.key);
    if (!turn || !plan) return null;
    let changed = false;
    if (turn.body.includes(id) && id !== plan.answer && this.isFolded(turn, plan)) {
      this.folds.set(turn.key, false);
      changed = true;
    }
    // A child entry sits in its outermost parent's place.
    let row = id;
    for (let parent = this.source.data.entries.get(row)?.parent; parent != null && turn.body.includes(parent); parent = this.source.data.entries.get(row)?.parent) row = parent;
    const group = plan.log.find((item): item is Extract<LogItem, { kind: "group" }> => item.kind === "group" && item.members.includes(row));
    if (group && !this.openGroups.has(group.key)) {
      this.openGroups.add(group.key);
      changed = true;
    }
    if (changed) {
      this.layoutTurn(turn);
      this.placeNewSince();
    }
    const element = this.rendered.get(id)?.element ?? null;
    if (!element) return null;
    if (options.scroll !== false) {
      this.setFollowing(false);
      this.jump.hidden = false;
      const offset = element.getBoundingClientRect().top - this.scroller.getBoundingClientRect().top;
      this.scroller.scrollTo({ top: this.scroller.scrollTop + offset - this.scroller.clientHeight / 3, behavior: "instant" });
    }
    if (options.flash) {
      element.classList.remove("entry-flash");
      void element.offsetWidth;
      element.classList.add("entry-flash");
    }
    return element;
  }

  /** Opens a tool call's row, e.g. to show the output a search matched. */
  openTool(id: number) {
    const entry = this.source?.data.entries.get(id);
    if (entry?.kind !== "tool" || this.isOpen(entry)) return;
    if (!this.toggled.delete(id)) this.toggled.add(id);
    this.rerenderEntry(id);
    this.flush();
  }

  /** Whether a prompt's turn edited files or ended with failures, for the prompt rail. */
  marks(prompt: number): { edits: boolean; failures: boolean } {
    const plan = this.plans.get(prompt);
    const turn = this.turnByKey.get(prompt);
    const entries = this.source?.data.entries;
    if (!plan || !turn || !entries) return { edits: false, failures: false };
    const edits = turn.body.some((id) => {
      const entry = entries.get(id);
      return entry?.kind === "tool" && entry.name === "apply_patch" && entry.state === "succeeded";
    });
    return { edits, failures: plan.unrecovered.length > 0 };
  }

  private schedule() {
    if (!this.frame) this.frame = requestAnimationFrame(() => this.flush());
  }

  private relayout(turn: number) {
    this.dirtyTurns.add(turn);
    this.schedule();
  }

  private flush() {
    cancelAnimationFrame(this.frame);
    this.frame = 0;
    const data = this.source?.data;
    if (!data || (this.dirty.size === 0 && this.dirtyTurns.size === 0 && !this.relayoutAll)) return;
    if (this.list.querySelector(":scope > .empty-state")) this.list.replaceChildren();

    const dirty = [...this.dirty].filter((id) => data.entries.has(id)).sort((a, b) => a - b);
    this.dirty.clear();
    const turns = new Set(this.dirtyTurns);
    this.dirtyTurns.clear();
    const resegment = this.relayoutAll || dirty.some((id) => !this.rendered.has(id));
    if (resegment) this.segment(data, turns);
    // A turn is laid out again only when an entry's place in it may have changed: streamed text
    // never changes it, a call finishing or failing does.
    for (const id of dirty) {
      const signature = structure(data.entries.get(id)!);
      if (signature === this.signatures.get(id)) continue;
      this.signatures.set(id, signature);
      const turn = this.turnOf.get(id);
      if (turn) turns.add(turn.key);
    }

    const render = new Set(dirty);
    for (const key of turns) {
      const turn = this.turnByKey.get(key);
      if (!turn) continue;
      const plan = planTurn(turn, data.entries);
      this.plans.set(key, plan);
      const outcome = turn.end === null ? undefined : turnOutcome(turn, plan, data.entries);
      for (const id of members(turn)) {
        const context: EntryContext = {};
        if (id === plan.answer) context.role = "answer";
        else if (plan.narration.has(id)) context.role = "narration";
        if (plan.recovered.has(id)) context.recovered = true;
        if (id === turn.end) Object.assign(context, { outcome, turn: key });
        const before = this.contexts.get(id);
        if (JSON.stringify(before ?? {}) === JSON.stringify(context)) continue;
        this.contexts.set(id, context);
        render.add(id);
      }
    }
    for (const id of [...render].sort((a, b) => a - b)) {
      const entry = data.entries.get(id);
      if (!entry) continue;
      const context = JSON.stringify(this.contexts.get(id) ?? {});
      const held = this.rendered.get(id);
      if (held && held.revision === entry.revision && held.context === context) continue;
      const element = held?.element ?? document.createElement("article");
      this.renderEntry(element, entry);
      this.rendered.set(id, { element, revision: entry.revision, context });
    }
    for (const key of turns) {
      const turn = this.turnByKey.get(key);
      if (turn) this.layoutTurn(turn);
    }
    if (resegment) setChildren(this.list, this.turns.map((turn) => this.view(turn.key).section));
    this.placeNewSince();
    if (this.observing) this.recordSeen();
    if (turns.size) this.options.laidOut?.();
    if (this.following) this.scrollToEnd("instant");
    else this.jump.hidden = false;
  }

  /** Splits the transcript into turns again and notes which turns changed shape. */
  private segment(data: TranscriptData, changed: Set<number>) {
    const previousLatest = this.turns.at(-1)?.key;
    this.turns = segmentTurns(data);
    this.turnOf.clear();
    this.turnByKey.clear();
    for (const turn of this.turns) {
      this.turnByKey.set(turn.key, turn);
      for (const id of members(turn)) this.turnOf.set(id, turn);
      const shape = `${turn.user}|${turn.body.join(",")}|${turn.end}|${turn.trailing.join(",")}`;
      if (shape === this.shapes.get(turn.key)) continue;
      this.shapes.set(turn.key, shape);
      changed.add(turn.key);
    }
    // The latest turn stays unfolded, so the one before it folds when a new turn starts.
    if (previousLatest !== undefined && previousLatest !== this.turns.at(-1)?.key) changed.add(previousLatest);
    for (const [key, view] of this.views) {
      if (this.turnByKey.has(key)) continue;
      view.section.remove();
      this.views.delete(key);
      this.shapes.delete(key);
      this.plans.delete(key);
    }
    this.relayoutAll = false;
  }

  private view(key: number): TurnView {
    let view = this.views.get(key);
    if (!view) {
      const section = document.createElement("section");
      section.className = "turn";
      section.dataset.turn = String(key);
      const fold = document.createElement("div");
      fold.className = "turn-fold";
      fold.innerHTML = `<button class="tool-row fold-row" type="button"><span class="tool-state"></span><span class="fold-text"></span><span class="tool-meta"></span>${glyph("chevron-right", "glyph chevron")}</button>`;
      const log = document.createElement("div");
      log.className = "turn-log";
      view = { section, fold, log, groups: new Map(), batches: new Map() };
      this.views.set(key, view);
    }
    return view;
  }

  /**
   * Finished turns fold their work log behind one row, except the latest turn; the reader's own
   * choice wins. A turn still running never folds by default.
   */
  private isFolded(turn: Turn, plan: TurnPlan) {
    const choice = this.folds.get(turn.key);
    if (choice !== undefined) return choice;
    return turn.end !== null && plan.log.length > 0 && turn !== this.turns.at(-1);
  }

  /** Puts a turn's nodes in order: prompt, fold row, work log, pinned notices, answer, outcome. */
  private layoutTurn(turn: Turn) {
    const plan = this.plans.get(turn.key);
    if (!plan) return;
    const view = this.view(turn.key);
    // Folding a turn above the viewport must not move what the reader is looking at.
    const rect = this.following || !view.section.isConnected ? null : view.section.getBoundingClientRect();
    const above = rect !== null && rect.bottom <= this.scroller.getBoundingClientRect().top;
    const folded = this.isFolded(turn, plan);
    // A call with child entries is laid out as a batch: its row, then its children in order.
    const batches = new Set<number>();
    const element = (id: number): HTMLElement | undefined => {
      const article = this.rendered.get(id)?.element;
      const children = plan.children.get(id);
      if (!article || !children?.length) return article;
      let batch = view.batches.get(id);
      if (!batch) {
        batch = document.createElement("div");
        batch.className = "batch";
        batch.dataset.parent = String(id);
        view.batches.set(id, batch);
      }
      batches.add(id);
      setChildren(batch, [article, ...children.flatMap((child) => element(child) ?? [])]);
      return batch;
    };
    const children: HTMLElement[] = [];
    const push = (into: HTMLElement[], id: number) => {
      const node = element(id);
      if (node) into.push(node);
    };
    if (turn.user !== null) push(children, turn.user);
    if (turn.end !== null && plan.log.length > 0) {
      this.renderFold(view.fold, turn, plan, folded);
      children.push(view.fold);
    }
    const log: HTMLElement[] = [];
    const groups = new Set<number>();
    for (const item of plan.log) {
      if (item.kind === "entry") {
        if (!(folded && plan.pinned.has(item.id))) push(log, item.id);
        continue;
      }
      groups.add(item.key);
      log.push(this.renderGroup(view, item, (id) => element(id)));
    }
    for (const key of view.groups.keys()) if (!groups.has(key)) view.groups.delete(key);
    for (const key of view.batches.keys()) if (!batches.has(key)) view.batches.delete(key);
    setChildren(view.log, log);
    view.log.hidden = folded;
    if (log.length) children.push(view.log);
    if (folded) for (const id of plan.pinned) push(children, id);
    if (plan.answer !== null) push(children, plan.answer);
    if (turn.end !== null) push(children, turn.end);
    for (const id of turn.trailing) push(children, id);
    setChildren(view.section, children);
    view.section.classList.toggle("folded", folded);
    if (above) this.scroller.scrollTop += view.section.getBoundingClientRect().height - rect!.height;
  }

  private renderFold(fold: HTMLElement, turn: Turn, plan: TurnPlan, folded: boolean) {
    const end = turn.end === null ? undefined : this.source?.data.entries.get(turn.end);
    const steps = `${plan.steps} step${plan.steps === 1 ? "" : "s"}`;
    const text = end?.kind === "turn_completed" ? `Worked for ${formatDuration(end.duration_ns)} · ${steps}` : `Worked · ${steps}`;
    fold.dataset.state = plan.unrecovered.length ? "failed" : "succeeded";
    const row = fold.querySelector<HTMLElement>(".fold-row")!;
    row.setAttribute("aria-expanded", String(!folded));
    fold.classList.toggle("open", !folded);
    row.querySelector(".fold-text")!.textContent = text;
    const failures = plan.unrecovered.length;
    row.querySelector(".tool-meta")!.textContent = failures ? `${failures} failed` : "";
  }

  private renderGroup(view: TurnView, item: Extract<LogItem, { kind: "group" }>, element: (id: number) => HTMLElement | undefined) {
    let group = view.groups.get(item.key);
    if (!group) {
      group = document.createElement("div");
      group.className = "step-group";
      group.innerHTML = `<button class="tool-row group-row" type="button"><span class="tool-state"></span><span class="tool-name">Explored</span><span class="tool-summary"></span><span class="tool-meta"></span>${glyph("chevron-right", "glyph chevron")}</button><div class="step-group-body"></div>`;
      view.groups.set(item.key, group);
    }
    const entries = this.source!.data.entries;
    const calls = item.tools.map((id) => entries.get(id)).filter((entry): entry is ToolEntry => entry?.kind === "tool");
    const open = this.openGroups.has(item.key);
    group.dataset.key = String(item.key);
    group.dataset.state = calls.some((call) => call.state === "running") ? "running" : "succeeded";
    group.classList.toggle("open", open);
    group.querySelector(".group-row")!.setAttribute("aria-expanded", String(open));
    group.querySelector(".tool-summary")!.textContent = explorationLabel(calls);
    const total = calls.reduce((sum, call) => sum + (call.duration_ns ?? 0), 0);
    group.querySelector(".tool-meta")!.textContent = total ? formatDuration(total) : "";
    const first = entries.get(item.key);
    group.querySelector(".entry-link")?.remove();
    if (first) {
      const gutter = this.gutter(first);
      if (gutter) group.querySelector(".group-row")!.after(gutter);
    }
    const body = group.querySelector<HTMLElement>(".step-group-body")!;
    body.hidden = !open;
    setChildren(body, item.members.flatMap((id) => element(id) ?? []));
    return group;
  }

  private entryOf(node: HTMLElement) {
    return this.source?.data.entries.get(Number(node.closest<HTMLElement>(".entry")?.dataset.id));
  }

  private renderEntry(element: HTMLElement, entry: WireEntry) {
    const context = this.contexts.get(entry.id) ?? {};
    element.className = `entry entry-${entry.kind}${entry.parent === null ? "" : " nested"}`;
    element.dataset.id = String(entry.id);
    switch (entry.kind) {
      case "user": {
        const bubble = child(element, "div", "user-bubble");
        const image = this.source?.image;
        bubble.replaceChildren(...promptParts(entry.text, image ? entry.images ?? 0 : 0).map((part) => {
          const block = document.createElement("div");
          if (part.kind === "text") {
            block.className = "user-text";
            block.textContent = part.text;
          } else {
            block.className = "user-image";
            const picture = document.createElement("img");
            picture.alt = part.marker;
            picture.loading = "lazy";
            picture.src = image!(entry.id, part.index);
            block.append(picture);
          }
          return block;
        }));
        break;
      }
      case "assistant": {
        // Within a turn the last message answers and the ones before narrate the work.
        element.classList.toggle("narration", context.role === "narration");
        element.classList.toggle("answer", context.role === "answer");
        element.classList.toggle("streaming", !entry.complete);
        this.renderBody(child(element, "div", "markdown"), entry, !entry.complete);
        break;
      }
      case "reasoning": {
        let details = element.querySelector<HTMLDetailsElement>(":scope > details");
        if (!details) {
          details = document.createElement("details");
          details.className = "reasoning";
          details.innerHTML = `<summary>${glyph("brain")}<span class="reasoning-label">Thought</span><span class="reasoning-preview"></span>${glyph("chevron-right", "glyph chevron")}</summary><div class="markdown reasoning-body"></div>`;
          element.replaceChildren(details);
        }
        details.querySelector(".reasoning-preview")!.textContent = firstLine(entry.text);
        const body = details.querySelector<HTMLElement>(".reasoning-body")!;
        if (details.open) this.renderBody(body, entry, true);
        else body.dataset.stale = "true";
        break;
      }
      case "tool":
        this.renderTool(element, entry, context.recovered === true);
        break;
      case "directed_message": {
        const key = (message: number) => `${entry.id}:${message}`;
        renderThread(element, entry, {
          participants: this.source!.participants(),
          open: this.toggled.has(entry.id),
          markdown: this.markdown,
          full: (message) => this.fullMessages.has(key(message)),
          setFull: (message, full) => {
            if (full) this.fullMessages.add(key(message));
            else this.fullMessages.delete(key(message));
          },
        });
        break;
      }
      case "turn_completed":
        if (context.outcome) this.renderOutcome(element, context.outcome, context.turn!);
        else marker(element, `Worked for ${formatDuration(entry.duration_ns)}`);
        return;
      case "context_compacted":
        marker(element, `Context compacted · ${formatDuration(entry.duration_ns)}`, "compact");
        return;
      case "interrupted":
        notice(element, "Interrupted", entry.count > 1 ? `${entry.count} pending prompts discarded` : "", "warning");
        break;
      case "compaction_failed":
        notice(element, "Compaction failed", entry.message, "danger");
        break;
      case "error":
        notice(element, "Error", entry.message, "danger");
        break;
      case "effort_changed":
        quiet(element, `Effort set to ${entry.to}`);
        return;
      case "fast_mode_changed":
        quiet(element, entry.enabled ? "Fast mode on" : "Fast mode off");
        return;
      case "forked_from":
        quiet(element, `Forked from ${entry.session.slice(0, 8)}`);
        return;
      case "reflection_started":
        quiet(element, "Reflecting on the session");
        return;
      default: {
        const kind = (entry as { kind: string }).kind;
        element.className = "entry entry-unknown";
        quiet(element, `Unsupported entry · ${kind}`);
        return;
      }
    }
    element.querySelector(":scope > .entry-link")?.remove();
    const gutter = this.gutter(entry);
    if (gutter) element.append(gutter);
  }

  /**
   * The time an entry was recorded, in the gutter beside wide transcripts, which doubles as the
   * entry's link: clicking it copies a URL that opens the transcript at this entry.
   */
  private gutter(entry: WireEntry): HTMLElement | null {
    if (!GUTTER_KINDS.has(entry.kind)) return null;
    const at = entry.at_ms ?? null;
    const link = this.source?.link?.(entry.id) ?? null;
    if (at === null && link === null) return null;
    const anchor = document.createElement(link ? "a" : "span");
    anchor.className = "entry-link";
    const when = at === null ? "" : new Date(at).toLocaleString(undefined, { dateStyle: "medium", timeStyle: "medium" });
    if (link) {
      (anchor as HTMLAnchorElement).href = link;
      anchor.title = when ? `${when} · Copy link` : "Copy link";
      anchor.setAttribute("aria-label", "Copy link to this entry");
    } else {
      anchor.title = when;
    }
    anchor.innerHTML = `${at === null ? "" : `<time></time>`}${link ? glyph("link") : ""}`;
    const time = anchor.querySelector("time");
    if (time) {
      time.dateTime = new Date(at!).toISOString();
      time.textContent = clockTime(at!);
    }
    return anchor;
  }

  /**
   * Renders an entry's Markdown body. Streaming text renders at once without highlighting;
   * settled text shows as plain text until it nears the viewport.
   */
  private renderBody(container: HTMLElement, entry: WireEntry, now: boolean) {
    const text = "text" in entry ? entry.text : "body" in entry ? entry.body : "";
    if (!now) {
      container.textContent = text;
      container.classList.add("plain");
      this.lazy.observe(container);
      return;
    }
    container.classList.remove("plain");
    delete container.dataset.stale;
    const streaming = entry.kind === "assistant" && !entry.complete;
    void renderMarkdown(container, text, this.theme() === "dark" ? "pierre-dark" : "pierre-light", {
      imageSource: this.imageSource,
      highlight: !streaming,
      placeholder: streaming ? "" : " ",
    });
  }

  /** Local images resolve against the viewed session workspace; a subagent key is session/agent. */
  private imageSource = (destination: string) =>
    localImageSource(destination, this.source?.key.split("/")[0]);

  /** Renders settled Markdown at once, highlighted, as assistant text is. */
  private markdown = (container: HTMLElement, text: string) => {
    void renderMarkdown(container, text, this.theme() === "dark" ? "pierre-dark" : "pierre-light", {
      imageSource: this.imageSource,
      placeholder: " ",
    });
  };

  /**
   * A failed command that nothing made good opens on its own, showing the end of its output; so do
   * patches and plans.
   */
  private defaultOpen(entry: ToolEntry) {
    if (TOOL_DEFAULT_OPEN.has(entry.name)) return true;
    const recovered = this.contexts.get(entry.id)?.recovered === true;
    return entry.state === "failed" && !recovered && (entry.outcome?.tail.length ?? 0) > 0;
  }

  private isOpen(entry: ToolEntry) {
    return this.defaultOpen(entry) !== this.toggled.has(entry.id);
  }

  private renderTool(element: HTMLElement, entry: ToolEntry, recovered: boolean) {
    const open = this.isOpen(entry);
    element.classList.toggle("open", open);
    element.dataset.state = entry.state;
    element.dataset.tool = entry.name;
    element.toggleAttribute("data-recovered", recovered);
    this.releaseDiffs(entry.id);
    const extra = [
      // A Code Mode run reports its tool count in its summary; other tools with children spawned agents.
      entry.child_count && entry.name !== "exec" ? `${entry.child_count} agent${entry.child_count === 1 ? "" : "s"}` : "",
      entry.substeps.length ? `${entry.substeps.length} step${entry.substeps.length === 1 ? "" : "s"}` : "",
      recovered ? "retried" : "",
    ].filter(Boolean).join(" · ");
    element.innerHTML = `<button class="tool-row" type="button" aria-expanded="${open}">
      <span class="tool-state" aria-label="${recovered ? "failed, retried" : entry.state}"></span>
      <span class="tool-name"></span>
      <span class="tool-subject"><span class="tool-summary"></span></span>
      <span class="tool-meta"></span>
      ${glyph("chevron-right", "glyph chevron")}
    </button>`;
    const name = element.querySelector<HTMLElement>(".tool-name")!;
    name.textContent = toolLabel(entry.name, entry.child_count);
    name.title = entry.name;
    element.querySelector(".tool-summary")!.textContent = entry.summary;
    const result = entry.state === "running" ? null : resultText(entry);
    if (result) child(element.querySelector<HTMLElement>(".tool-subject")!, "span", "tool-result").textContent = result;
    const meta = element.querySelector<HTMLElement>(".tool-meta")!;
    // The server reports how long a running call has already run; the browser counts on from its own
    // clock, so a skewed clock cannot misstate the time. A call that is still running can already
    // carry a duration (a backgrounded command reports when it yielded), which the live clock replaces.
    const live = entry.state === "running" && entry.elapsed_ns != null;
    meta.textContent = [extra, entry.duration_ns === null || live ? "" : formatDuration(entry.duration_ns)]
      .filter(Boolean).join(" · ");
    if (entry.stats) prependStats(meta, entry.stats.additions, entry.stats.deletions);
    if (live) {
      element.dataset.since = String(performance.now() - entry.elapsed_ns! / 1e6);
      if (meta.textContent) meta.append(" · ");
      child(meta, "span", "tool-clock").textContent = formatDuration(entry.elapsed_ns);
      // The element may not be attached yet, so the timer cannot depend on finding it in the list.
      if (!this.clockTimer) this.clockTimer = window.setInterval(this.tickClocks, 1000);
    } else {
      delete element.dataset.since;
    }
    if (entry.name === "spawn_agent" && entry.state === "succeeded" && entry.has_detail && this.source?.agents) {
      // Empty until the agent is known; an element that is not displayed would never intersect.
      this.lazyAgents.observe(child(element, "ul", "tool-agents"));
    }
    if (!open) return;
    const body = child(element, "div", "tool-body");
    if (entry.substeps.length) {
      const steps = child(body, "ol", "tool-steps");
      for (const step of entry.substeps) child(steps, "li", "").textContent = step;
    }
    const outcome = entry.outcome;
    if (entry.state === "failed" && outcome?.tail.length && !this.fullOutput.has(entry.id)) {
      body.append(tailView(outcome));
      if (entry.has_detail) {
        const more = child(body, "button", "tool-more");
        more.type = "button";
        more.textContent = "Show full output";
      }
      return;
    }
    if (entry.has_detail) {
      const detail = child(body, "div", "tool-io");
      detail.innerHTML = `<div class="tool-loading"><span class="spinner"></span>Loading</div>`;
      this.lazyDetail.observe(detail);
    }
  }

  /**
   * Learns which agent a spawn started from its result, fetched once. A batch that spawns agents
   * shows its spawns as nested rows, so each agent is listed under the call that started it.
   */
  private async resolveAgents(list: HTMLElement, entry: ToolEntry) {
    let id = this.agentIds.get(entry.id);
    if (!id) {
      id = this.loadDetail(entry).then((detail) => spawnedAgentId(detail.result), () => null);
      this.agentIds.set(entry.id, id);
    }
    const agent = await id;
    if (!list.isConnected || agent === null) return;
    list.dataset.agents = String(agent);
    this.renderAgentList(list);
  }

  private renderAgentList(list: HTMLElement) {
    const roster = this.source?.agents?.() ?? [];
    const agents = (list.dataset.agents ?? "").split(",").flatMap((id) => roster.find((agent) => agent.id === Number(id)) ?? []);
    list.hidden = agents.length === 0;
    list.replaceChildren(...agents.map((agent) => {
      const item = document.createElement("li");
      item.innerHTML = `<button type="button" class="agent-line"><span class="tool-state"></span><span class="agent-line-role"></span><span class="model-dot"></span><span class="agent-line-note"></span>${glyph("chevron-right", "glyph chevron")}</button>`;
      const button = item.querySelector<HTMLElement>(".agent-line")!;
      button.dataset.agent = String(agent.id);
      button.dataset.state = agent.status.state === "pending" ? "running" : agent.status.state;
      button.title = `${agent.role} · ${agent.model} · ${agent.status.state}`;
      item.querySelector(".agent-line-role")!.textContent = agent.role;
      item.querySelector<HTMLElement>(".model-dot")!.style.background = modelColor(agent.model);
      item.querySelector(".agent-line-note")!.textContent = this.source?.agentNote?.(agent) ?? agent.status.state;
      return item;
    }));
  }

  /** Refreshes every running tool call's clock; the timer stops once none remain. */
  private tickClocks = () => {
    const now = performance.now();
    const running = this.list.querySelectorAll<HTMLElement>(".entry-tool[data-since]");
    for (const element of running) {
      const clock = element.querySelector(".tool-clock");
      if (clock) clock.textContent = formatDuration((now - Number(element.dataset.since)) * 1e6);
    }
    if (!running.length) {
      clearInterval(this.clockTimer);
      this.clockTimer = 0;
    }
  };

  private loadDetail(entry: ToolEntry) {
    let cached = this.details.get(entry.id);
    if (!cached || cached.revision !== entry.revision) {
      cached = { revision: entry.revision, detail: this.source!.detail(entry.id) };
      this.details.set(entry.id, cached);
    }
    return cached.detail;
  }

  private async renderDetail(container: HTMLElement, entry: ToolEntry) {
    const source = this.source;
    if (!source) return;
    try {
      const detail = await this.loadDetail(entry);
      if (!container.isConnected) return;
      presentDetail(container, entry.name, detail, {
        theme: this.theme(),
        participants: source.participants(),
        markdown: this.markdown,
        full: this.showAll.has(entry.id),
        track: (instances) => this.diffs.set(entry.id, instances),
        toggleFull: () => {
          if (!this.showAll.delete(entry.id)) this.showAll.add(entry.id);
          this.rerenderEntry(entry.id);
        },
      });
      const patch = container.querySelector<HTMLElement>(".patch");
      const meta = container.closest(".entry")?.querySelector<HTMLElement>(".tool-meta");
      if (patch && meta && !entry.stats) prependStats(meta, Number(patch.dataset.additions), Number(patch.dataset.deletions));
    } catch (error) {
      this.details.delete(entry.id);
      container.innerHTML = `<p class="tool-error"></p>`;
      container.firstElementChild!.textContent = error instanceof ApiError && error.status === 404
        ? "Details are no longer available."
        : `Could not load details: ${errorMessage(error)}`;
    }
  }

  /**
   * The line under a finished turn's answer: how long it took, how many calls it made, what it
   * changed, the last test summary, and the failures nothing made good, each of which links to its
   * row in the work log.
   */
  private renderOutcome(element: HTMLElement, outcome: TurnOutcome, turn: number) {
    element.innerHTML = `<div class="turn-outcome"></div>`;
    const strip = element.firstElementChild as HTMLElement;
    strip.dataset.turn = String(turn);
    const item = (text: string, tone = "") => {
      const span = document.createElement("span");
      span.className = "outcome-item";
      if (tone) span.dataset.tone = tone;
      span.textContent = text;
      strip.append(span);
      return span;
    };
    if (outcome.durationNs !== null) item(formatDuration(outcome.durationNs));
    if (outcome.toolCalls) item(`${outcome.toolCalls} tool call${outcome.toolCalls === 1 ? "" : "s"}`);
    if (outcome.files) {
      const files = item(`${outcome.files} file${outcome.files === 1 ? "" : "s"}`);
      files.insertAdjacentHTML("beforeend", ` <span class="add">+${outcome.additions}</span> <span class="del">−${outcome.deletions}</span>`);
    }
    if (outcome.tests) item(outcome.tests.summary, outcome.tests.failed ? "danger" : "");
    if (outcome.failures.length) {
      const failures = document.createElement("button");
      failures.type = "button";
      failures.className = "outcome-item outcome-failures";
      failures.dataset.tone = "danger";
      failures.dataset.failures = outcome.failures.join(",");
      failures.textContent = `${outcome.failures.length} failure${outcome.failures.length === 1 ? "" : "s"}`;
      failures.title = "Show in the work log";
      strip.append(failures);
    }
    if (outcome.files && this.options.openReview) {
      const review = document.createElement("button");
      review.type = "button";
      review.className = "outcome-item outcome-review";
      review.textContent = "Review changes";
      strip.append(review);
    }
    const end = document.createElement("span");
    end.className = "outcome-end";
    end.innerHTML = `<button type="button" class="icon-button small outcome-copy" title="Copy turn as Markdown" aria-label="Copy turn as Markdown">${glyph("copy")}</button>`;
    if (outcome.endedAt !== null) {
      const time = document.createElement("time");
      time.className = "outcome-time";
      time.dataset.at = String(outcome.endedAt);
      time.dateTime = new Date(outcome.endedAt).toISOString();
      time.title = new Date(outcome.endedAt).toLocaleString(undefined, { dateStyle: "medium", timeStyle: "short" });
      time.textContent = ago(outcome.endedAt);
      end.append(time);
      this.ageTimer ||= window.setInterval(this.refreshAges, 60_000);
    }
    strip.append(end);
  }

  private refreshAges = () => {
    if (!this.list.isConnected) {
      clearInterval(this.ageTimer);
      this.ageTimer = 0;
      return;
    }
    for (const time of this.list.querySelectorAll<HTMLElement>("[data-at]")) time.textContent = ago(Number(time.dataset.at));
    if (this.newSince && this.seen) this.newSince.label = newSinceLabel(this.seen.at);
    this.placeNewSince();
  };

  private async copyTurn(key: number) {
    const turn = this.turnByKey.get(key);
    const plan = this.plans.get(key);
    const entries = this.source?.data.entries;
    if (!turn || !plan || !entries) return;
    const prompt = turn.user === null ? undefined : entries.get(turn.user);
    const answer = plan.answer === null ? undefined : entries.get(plan.answer);
    const markdown = turnMarkdown(
      prompt?.kind === "user" ? prompt.text : "",
      answer?.kind === "assistant" ? answer.text : null,
      turnOutcome(turn, plan, entries),
    );
    await copy(markdown, "Copied the turn as Markdown.");
  }

  // The "new since you last looked" marker. What the page shows counts as seen once it has been
  // visible and focused for a moment; the marker then stays until the reader looks away and back.

  private attentionChanged() {
    if (!this.options.seen || !this.source) return;
    const attending = document.visibilityState === "visible" && document.hasFocus();
    if (!attending) {
      if (this.observing) this.stopObserving();
      clearTimeout(this.seenTimer);
      this.seenTimer = 0;
      return;
    }
    if (this.observing || this.seenTimer) return;
    this.markUnseen();
    this.placeNewSince();
    this.seenTimer = window.setTimeout(() => {
      this.seenTimer = 0;
      if (document.visibilityState !== "visible" || !document.hasFocus()) return;
      this.observing = true;
      this.recordSeen();
    }, SEEN_AFTER_MS);
  }

  private stopObserving() {
    this.observing = false;
    this.recordSeen(true);
  }

  private markUnseen() {
    const data = this.source?.data;
    if (!data || !this.seen) {
      this.newSince = null;
      return;
    }
    const id = firstUnseen(data, this.seen);
    this.newSince = id === null ? null : { id, label: newSinceLabel(this.seen.at) };
  }

  private recordSeen(force = false) {
    const data = this.source?.data;
    const storage = this.options.seen;
    if (!data || !storage || !this.source) return;
    const latest = latestEntry(data);
    if (latest === null || (!force && latest === this.seen?.id)) return;
    this.seen = { id: latest, at: Date.now() };
    writeSeen(storage, this.source.key, this.seen);
  }

  /** Puts the marker above the first unseen entry, or above the fold or group that hides it. */
  private placeNewSince() {
    const marker = this.newSinceMarker;
    const target = this.newSince && this.rendered.get(this.newSince.id)?.element;
    if (!this.newSince || !target?.isConnected) {
      marker.remove();
      return;
    }
    let anchor: HTMLElement = target;
    const turn = this.turnOf.get(this.newSince.id);
    const view = turn && this.views.get(turn.key);
    const group = anchor.parentElement?.closest<HTMLElement>(".step-group");
    if (group && !this.openGroups.has(Number(group.dataset.key))) anchor = group;
    if (view && anchor.closest(".turn-log") === view.log && view.log.hidden) anchor = view.fold;
    if (view && turn?.user === this.newSince.id) anchor = view.section;
    marker.textContent = this.newSince.label;
    if (marker.nextElementSibling !== anchor) anchor.before(marker);
  }

  private releaseDiffs(id: number) {
    for (const instance of this.diffs.get(id) ?? []) instance.cleanUp();
    this.diffs.delete(id);
  }

  private rerenderEntry(id: number) {
    const rendered = this.rendered.get(id);
    if (rendered) rendered.revision = -1;
    this.entryChanged(id);
  }

  private handleClick(event: MouseEvent) {
    const target = event.target as HTMLElement;
    if (target instanceof HTMLImageElement && target.closest(".markdown, .user-image")) {
      openLightbox(target.currentSrc || target.src, target.alt);
      return;
    }
    const link = target.closest<HTMLAnchorElement>("a.entry-link");
    if (link) {
      event.preventDefault();
      void copy(link.href, "Copied a link to this entry.");
      return;
    }
    const agent = target.closest<HTMLElement>(".agent-line");
    if (agent) {
      this.source?.openAgent?.(Number(agent.dataset.agent));
      return;
    }
    const fold = target.closest<HTMLElement>(".fold-row");
    if (fold) {
      const key = Number(fold.closest<HTMLElement>(".turn")!.dataset.turn);
      const turn = this.turnByKey.get(key);
      const plan = this.plans.get(key);
      if (turn && plan) {
        this.folds.set(key, !this.isFolded(turn, plan));
        this.relayout(key);
      }
      return;
    }
    const groupRow = target.closest<HTMLElement>(".group-row");
    if (groupRow) {
      const key = Number(groupRow.closest<HTMLElement>(".step-group")!.dataset.key);
      if (!this.openGroups.delete(key)) this.openGroups.add(key);
      const turn = this.turnOf.get(key);
      if (turn) this.relayout(turn.key);
      return;
    }
    const failures = target.closest<HTMLElement>(".outcome-failures");
    if (failures) {
      // Each click shows the next failure.
      const ids = failures.dataset.failures!.split(",").map(Number);
      const next = (Number(failures.dataset.next ?? 0)) % ids.length;
      failures.dataset.next = String(next + 1);
      this.reveal(ids[next]!, { flash: true });
      return;
    }
    if (target.closest(".outcome-review")) {
      this.options.openReview?.();
      return;
    }
    const copyButton = target.closest<HTMLElement>(".outcome-copy");
    if (copyButton) {
      void this.copyTurn(Number(copyButton.closest<HTMLElement>(".turn-outcome")!.dataset.turn));
      return;
    }
    const more = target.closest<HTMLElement>(".tool-more");
    if (more) {
      const id = Number(more.closest<HTMLElement>(".entry")!.dataset.id);
      this.fullOutput.add(id);
      this.rerenderEntry(id);
      return;
    }
    const row = target.closest<HTMLElement>(".tool-row, .dm-head");
    if (row) {
      const id = Number(row.closest<HTMLElement>(".entry")!.dataset.id);
      if (!this.toggled.delete(id)) this.toggled.add(id);
      this.rerenderEntry(id);
      return;
    }
    const summary = target.closest("summary");
    if (summary) {
      // Render the reasoning body on first open; the toggle itself is native.
      const body = summary.parentElement!.querySelector<HTMLElement>(".reasoning-body")!;
      const entry = this.entryOf(summary);
      if (entry && (body.dataset.stale || body.childElementCount === 0)) this.renderBody(body, entry, true);
    }
  }

  private setFollowing(following: boolean) {
    if (following === this.following) return;
    this.following = following;
    if (following) this.jump.hidden = true;
  }

  private scrollToEnd(behavior: ScrollBehavior) {
    this.following = true;
    this.jump.hidden = true;
    this.scroller.scrollTo({ top: this.scroller.scrollHeight, behavior });
  }
}

function members(turn: Turn) {
  return [...(turn.user === null ? [] : [turn.user]), ...turn.body, ...(turn.end === null ? [] : [turn.end]), ...turn.trailing];
}

/** The parts of an entry that decide its place in the turn's layout and outcome. */
function structure(entry: WireEntry) {
  if (entry.kind !== "tool") return entry.kind;
  return JSON.stringify([entry.state, entry.summary, entry.outcome ?? null, entry.stats ?? null, entry.duration_ns]);
}

/** Moves `parent`'s children into the `desired` order, touching only nodes out of place. */
function setChildren(parent: HTMLElement, desired: readonly HTMLElement[]) {
  let cursor = parent.firstElementChild;
  for (const node of desired) {
    if (node === cursor) {
      cursor = cursor.nextElementSibling;
      continue;
    }
    parent.insertBefore(node, cursor);
  }
  while (cursor) {
    const next = cursor.nextElementSibling;
    cursor.remove();
    cursor = next;
  }
}

function tailView(outcome: ToolOutcome) {
  const element = document.createElement("div");
  element.className = "term tool-tail";
  const pre = document.createElement("pre");
  pre.className = "term-out";
  pre.textContent = outcome.tail.join("\n");
  element.append(pre);
  if (outcome.exit_code !== null && outcome.exit_code !== 0) {
    const foot = document.createElement("div");
    foot.className = "term-exit";
    foot.innerHTML = glyph("alert");
    foot.append(`Exited with code ${outcome.exit_code}`);
    element.append(foot);
  }
  return element;
}

function prependStats(meta: HTMLElement, additions: number, deletions: number) {
  const stats = `<span class="add">+${additions}</span><span class="del">−${deletions}</span>`;
  meta.innerHTML = meta.textContent ? `${stats} · ${meta.innerHTML}` : stats;
}

function clockTime(unixMs: number) {
  return new Date(unixMs).toLocaleTimeString(undefined, { hour: "numeric", minute: "2-digit" });
}

function ago(unixMs: number) {
  const age = formatAge(unixMs);
  return age === "now" ? "just now" : /^\d/.test(age) ? `${age} ago` : age;
}

async function copy(text: string, done: string) {
  try {
    await navigator.clipboard.writeText(text);
    toast(done);
  } catch {
    toast("Could not copy to the clipboard.", "warning");
  }
}

/**
 * Local image destinations (absolute, file://, or workspace-relative) are served by the instance;
 * remote ones are never loaded by the page.
 */
function localImageSource(destination: string, session?: string) {
  if (!destination || /^([a-z][a-z0-9+.-]*:(?!\/\/\/)|\/\/)/i.test(destination)) return null;
  let path = destination;
  try {
    path = decodeURIComponent(destination);
  } catch {
    // A destination that is not percent-encoded is used as written.
  }
  const query = session ? `&session=${encodeURIComponent(session)}` : "";
  return `./api/file?path=${encodeURIComponent(path)}${query}`;
}

function child<K extends keyof HTMLElementTagNameMap>(parent: HTMLElement, tag: K, className: string) {
  const existing = className ? parent.querySelector<HTMLElementTagNameMap[K]>(`:scope > ${tag}.${className.split(" ")[0]}`) : null;
  if (existing) return existing;
  const element = document.createElement(tag);
  if (className) element.className = className;
  parent.append(element);
  return element;
}

function marker(element: HTMLElement, text: string, variant = "") {
  element.innerHTML = `<div class="marker ${variant}"><span></span></div>`;
  element.querySelector("span")!.textContent = text;
}

function quiet(element: HTMLElement, text: string) {
  element.innerHTML = `<p class="quiet"></p>`;
  element.firstElementChild!.textContent = text;
}

function notice(element: HTMLElement, title: string, body: string, tone: "warning" | "danger") {
  element.innerHTML = `<div class="notice ${tone}" role="note">${glyph("alert")}<div><strong></strong><p></p></div></div>`;
  element.querySelector("strong")!.textContent = title;
  const paragraph = element.querySelector("p")!;
  if (body) paragraph.textContent = body;
  else paragraph.remove();
}

function emptyState(title: string, body: string) {
  const element = document.createElement("div");
  element.className = "empty-state";
  element.innerHTML = `<div class="empty-mark">${glyph("sparkles")}</div><h2></h2><p></p>`;
  element.querySelector("h2")!.textContent = title;
  element.querySelector("p")!.textContent = body;
  return element;
}
