import "./monitor.css";
import { describeError, type ApiClient } from "../core/api-client";
import { formatAge } from "../core/format";
import { glyph } from "../ui/glyphs";
import { toast } from "../ui/toast";
import type { Store } from "../core/store";
import type { ModelCatalog, MonitorState } from "../core/wire";
import { rankSessions, type MonitorGroup, type MonitorOrder } from "./attention";
import { MonitorCard, type CardHost } from "./card";

/**
 * How often a visible Monitor renews its interest in checks by the monitor model. The server keeps
 * each declaration for 60 seconds, so a missed renewal or two does not stop the checks.
 */
const MONITOR_LEASE_MS = 20_000;

/** Which cards the grid shows: every live session, or one group of them. */
type Filter = "all" | MonitorGroup;

const FILTERS: { filter: Filter; label: string; empty: string }[] = [
  { filter: "all", label: "All", empty: "No live sessions. Start a chat from the sidebar and it appears here." },
  { filter: "needs", label: "Needs you", empty: "Nothing needs you right now." },
  { filter: "working", label: "Working", empty: "No session is working." },
  { filter: "idle", label: "Idle", empty: "No session is idle." },
];

const ORDERS: { order: MonitorOrder; label: string; title: string }[] = [
  { order: "attention", label: "Attention", title: "Sessions that need you first" },
  { order: "health", label: "Health", title: "The sessions going worst first" },
];

export type MonitorHost = {
  api: ApiClient;
  store: Store;
  catalog(): ModelCatalog | null;
  workspaceToken(path: string): string | null;
  /** Activates `session` and shows its chat. */
  open(session: string): void;
  /** Puts `text` in `session`'s draft in the composer, or focuses it when `text` is empty. */
  compose(session: string, text: string): void;
};

/** Sets text only when it differs, so the once-a-second refresh leaves unchanged nodes alone. */
function setText(element: Element, text: string) {
  if (element.textContent !== text) element.textContent = text;
}

/** Makes `nodes` the children of `parent` in order, moving only the nodes that are out of place. */
function placeChildren(parent: HTMLElement, nodes: readonly HTMLElement[]) {
  nodes.forEach((node, index) => {
    const current = parent.children[index] ?? null;
    if (current !== node) parent.insertBefore(node, current);
  });
  while (parent.children.length > nodes.length) parent.lastElementChild!.remove();
}

/**
 * The Monitor: every live session of this Tact as a tile in one grid, the sessions that need the
 * user first or those going worst, under a toolbar that filters and orders the grid and reports
 * the monitor model's checks. It
 * reads the store's live summaries, activity, and monitor state, and while it is on screen it
 * keeps the model checking and its clocks ticking.
 */
export class Monitor {
  private readonly cards = new Map<string, MonitorCard>();
  private readonly cardHost: CardHost;
  private readonly grid: HTMLElement;
  private visible = false;
  private frame = 0;
  private poller: ReturnType<typeof setInterval> | null = null;
  private ticker: ReturnType<typeof setInterval> | null = null;
  /** Why the last request to the monitor failed, until one succeeds. */
  private requestError: string | null = null;
  /** Window-local, like the view itself. */
  private filter: Filter = "all";
  /** Window-local, like the filter. */
  private order: MonitorOrder = "attention";

  constructor(private readonly root: HTMLElement, private readonly host: MonitorHost) {
    root.innerHTML = `
      <div class="monitor-scroller">
        <div class="monitor">
          <div class="monitor-toolbar">
            <div class="segmented compact monitor-filter" role="group" aria-label="Show">
              ${FILTERS.map(({ filter, label }) => `<button type="button" class="segment" data-filter="${filter}" aria-pressed="${filter === "all"}">${label}<span class="filter-count"></span></button>`).join("")}
            </div>
            <div class="monitor-order">
              <span class="monitor-order-label" aria-hidden="true">Order</span>
              <div class="segmented compact" role="group" aria-label="Order">
                ${ORDERS.map(({ order, label, title }) => `<button type="button" class="segment" data-order="${order}" title="${title}" aria-pressed="${order === "attention"}">${label}</button>`).join("")}
              </div>
            </div>
            <div class="monitor-status" role="status">
              <span class="spinner monitor-spinner" hidden></span>
              <span class="monitor-status-text"></span>
              <button type="button" class="card-link monitor-retry" hidden>Retry</button>
              <button type="button" class="icon-button small monitor-refresh" aria-label="Check the sessions now" title="Check the sessions now">${glyph("refresh")}</button>
            </div>
          </div>
          <p class="monitor-empty" hidden></p>
          <div class="monitor-grid"></div>
        </div>
      </div>`;
    this.grid = root.querySelector(".monitor-grid")!;
    const { api } = host;
    const report = (error: unknown) => toast(describeError(error), "warning");
    this.cardHost = {
      catalog: () => host.catalog(),
      workspaceToken: (path) => host.workspaceToken(path),
      open: (session) => host.open(session),
      // A card's idea of the active session can lag a frame behind, so composing opens its
      // session first; the composer then holds the text until that session's snapshot arrives.
      compose: (session, text) => {
        host.open(session);
        host.compose(session, text);
      },
      send: async (session, text) => {
        const { rev } = await api.command("set_draft", { session, text });
        await api.command("submit", { session, rev });
      },
      stop: (session) => void api.command("interrupt", { session }).catch(report),
      close: (session) => void api.command("close_session", { session }).catch(report),
    };
    for (const button of root.querySelectorAll<HTMLButtonElement>(".monitor-filter .segment")) {
      button.addEventListener("click", () => {
        this.filter = button.dataset.filter as Filter;
        this.render();
      });
    }
    for (const button of root.querySelectorAll<HTMLButtonElement>(".monitor-order .segment")) {
      button.addEventListener("click", () => {
        this.order = button.dataset.order as MonitorOrder;
        this.render();
      });
    }
    root.querySelector(".monitor-refresh")!.addEventListener("click", () => void this.askMonitor(true));
    root.querySelector(".monitor-retry")!.addEventListener("click", () => void this.askMonitor(true));
    document.addEventListener("visibilitychange", () => this.watch());
  }

  setVisible(visible: boolean) {
    if (visible === this.visible) return;
    this.visible = visible;
    this.watch();
    if (visible) this.render();
  }

  /** Something the Monitor shows changed; it re-renders once per frame while visible. */
  changed() {
    if (!this.visible || this.frame) return;
    this.frame = requestAnimationFrame(() => {
      this.frame = 0;
      this.render();
    });
  }

  /**
   * While the Monitor is on screen in a visible tab, asks for checks now and on every lease
   * renewal, and refreshes each second so clocks and ages count on; otherwise it does neither.
   */
  private watch() {
    const watching = this.visible && document.visibilityState === "visible";
    if (watching === (this.poller !== null)) return;
    if (watching) {
      void this.askMonitor(false);
      this.poller = setInterval(() => void this.askMonitor(false), MONITOR_LEASE_MS);
      this.ticker = setInterval(() => this.render(), 1000);
    } else {
      clearInterval(this.poller!);
      clearInterval(this.ticker!);
      this.poller = this.ticker = null;
    }
  }

  /**
   * Renews the lease and applies the state the server returns, unless other state arrived while
   * the request was in flight: the store replaces its state object on every `monitor` event, so
   * a changed object means the reply may be older than what is shown. Every change on the server
   * is also broadcast, so a dropped reply loses at most an availability change, which the next
   * renewal brings.
   */
  private async askMonitor(refresh: boolean) {
    const { store } = this.host;
    const shown = store.state.monitor;
    try {
      const state = await this.host.api.monitor(refresh);
      this.requestError = null;
      if (store.state.monitor === shown) store.dispatch({ type: "monitor", data: state });
      else this.changed();
    } catch (error) {
      this.requestError = describeError(error);
      this.changed();
    }
  }

  private render() {
    const state = this.host.store.state;
    const now = Date.now();
    const clock = performance.now();
    const check = state.monitor?.check ?? null;
    const ranked = rankSessions(state.live, state.activity, check, now, this.order);
    const counts: Record<Filter, number> = { all: ranked.length, needs: 0, working: 0, idle: 0 };
    let shown = 0;
    for (const entry of ranked) {
      const id = entry.summary.id;
      let card = this.cards.get(id);
      if (!card) this.cards.set(id, card = new MonitorCard(id, this.cardHost));
      card.update({ ...entry, checkedAt: check?.as_of_ms ?? null, active: id === state.active }, clock, now);
      counts[entry.assessment.group] += 1;
      // Filtered cards stay in the grid, hidden, so their running markers keep animating.
      card.element.hidden = this.filter !== "all" && this.filter !== entry.assessment.group;
      if (!card.element.hidden) shown += 1;
    }
    for (const [id, card] of this.cards) {
      if (ranked.some((entry) => entry.summary.id === id)) continue;
      card.dispose();
      this.cards.delete(id);
    }
    // Moving a card to another place detaches it for a moment, which drops focus from inside it.
    const focused = document.activeElement;
    placeChildren(this.grid, ranked.map((entry) => this.cards.get(entry.summary.id)!.element));
    if (focused instanceof HTMLElement && focused !== document.activeElement && focused.isConnected) focused.focus({ preventScroll: true });

    for (const button of this.root.querySelectorAll<HTMLElement>(".monitor-filter .segment")) {
      const filter = button.dataset.filter as Filter;
      button.setAttribute("aria-pressed", String(filter === this.filter));
      setText(button.querySelector(".filter-count")!, String(counts[filter]));
      button.dataset.empty = String(counts[filter] === 0);
    }
    for (const button of this.root.querySelectorAll<HTMLElement>(".monitor-order .segment")) {
      button.setAttribute("aria-pressed", String(button.dataset.order === this.order));
    }
    const empty = this.root.querySelector<HTMLElement>(".monitor-empty")!;
    empty.hidden = shown > 0;
    setText(empty, FILTERS.find(({ filter }) => filter === this.filter)!.empty);
    this.renderStatus(state.monitor, state.live.length, now);
  }

  /** The toolbar's account of the monitor model: when it last read the sessions, or why it cannot. */
  private renderStatus(state: MonitorState | null, live: number, now: number) {
    const status = this.root.querySelector<HTMLElement>(".monitor-status")!;
    const ready = state?.availability === "ready";
    const running = ready && state.running;
    let text = "";
    let title = "";
    let tone = "muted";
    let retry = false;
    if (this.requestError) {
      text = `The monitor could not be reached: ${this.requestError}`;
      tone = "warning";
      retry = true;
    } else if (!state) {
      text = "Connecting to the monitor…";
    } else if (!ready) {
      text = "Monitor checks unavailable";
      title = state.detail ?? (state.availability === "disabled" ? "The monitor model is turned off in the configuration." : "The monitor model has no credentials.");
    } else if (state.error) {
      text = `The last check failed: ${state.error}`;
      tone = "danger";
      retry = !state.running;
    } else if (state.check) {
      const age = formatAge(state.check.as_of_ms, now);
      const model = this.host.catalog()?.models.find((candidate) => candidate.id === state.check!.model)?.label ?? state.check.model;
      text = `${age === "now" ? "Checked just now" : `Checked ${age} ago`} · ${model}`;
      title = `Checked by ${model}`;
    } else {
      text = running ? `Checking ${live} session${live === 1 ? "" : "s"}…`
        : live ? "The first check is on its way." : "Checks start once a session is live.";
    }
    status.dataset.tone = tone;
    const textElement = status.querySelector<HTMLElement>(".monitor-status-text")!;
    setText(textElement, text);
    textElement.title = title || text;
    status.querySelector<HTMLElement>(".monitor-retry")!.hidden = !retry;
    status.querySelector<HTMLElement>(".monitor-spinner")!.hidden = !running && !(state === null && !this.requestError);
    status.querySelector<HTMLElement>(".monitor-refresh")!.hidden = !ready || running || retry;
  }
}
