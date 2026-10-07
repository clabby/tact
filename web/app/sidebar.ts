import { describeError, type ApiClient } from "./api-client";
import { formatAge, modelColor } from "./format";
import { glyph } from "./glyphs";
import { openMenu } from "./menu";
import type { Connection } from "./store";
import { toast } from "./toast";
import type { ModelCatalog, OpenSpec, PersistedSession, SessionSummary, SiblingInstance } from "./wire";

export type SidebarHost = {
  api: ApiClient;
  catalog(): ModelCatalog | null;
  /** Called after the user picked a session, so phones can close the drawer. */
  navigated(): void;
  toggleTheme(): void;
  themeChoice(): "system" | "light" | "dark";
};

const CONNECTION_LABELS: Record<Connection, string> = {
  connecting: "Connecting",
  open: "Connected",
  reconnecting: "Reconnecting",
  locked: "Signed out",
};

/** Sessions navigation: new chat, live sessions, history search, instances, and connection. */
export class Sidebar {
  private readonly liveList: HTMLElement;
  private readonly historyList: HTMLElement;
  private readonly search: HTMLInputElement;
  private readonly more: HTMLButtonElement;
  private readonly footer: HTMLElement;
  private live: SessionSummary[] = [];
  private active: string | null = null;
  private historyCursor: string | null = null;
  private historyQuery = "";
  private historyRequest: AbortController | null = null;
  private historyLoaded = false;
  private searchTimer = 0;

  constructor(private readonly root: HTMLElement, private readonly host: SidebarHost) {
    root.innerHTML = `
      <div class="sidebar-head">
        <div class="brand"><span class="brand-mark" aria-hidden="true">t</span><div class="brand-text"><strong>Tact</strong><span class="brand-workspace"></span></div></div>
        <button type="button" class="icon-button drawer-close" aria-label="Close sidebar">${glyph("close")}</button>
      </div>
      <div class="new-chat">
        <button type="button" class="new-chat-button">${glyph("plus")}<span>New chat</span></button>
        <button type="button" class="new-chat-model" aria-label="New chat with model" aria-haspopup="menu">${glyph("chevron-down")}</button>
      </div>
      <nav class="sidebar-scroll" aria-label="Sessions">
        <h2 class="section-title">Live <span class="live-count"></span></h2>
        <ul class="session-list live-list" role="list"></ul>
        <h2 class="section-title">History</h2>
        <label class="history-search">${glyph("search")}<input type="search" placeholder="Search history" aria-label="Search history" autocomplete="off"></label>
        <ul class="session-list history-list" role="list"></ul>
        <button type="button" class="history-more" hidden>Load more</button>
      </nav>
      <div class="sidebar-foot"></div>`;
    this.liveList = root.querySelector(".live-list")!;
    this.historyList = root.querySelector(".history-list")!;
    this.search = root.querySelector(".history-search input")!;
    this.more = root.querySelector(".history-more")!;
    this.footer = root.querySelector(".sidebar-foot")!;
    this.bind();
    this.renderFooter("connecting", []);
  }

  setWorkspace(repository: string, workspace: string) {
    const label = this.root.querySelector(".brand-workspace")!;
    label.textContent = repository || workspace.split("/").pop() || workspace;
    label.setAttribute("title", workspace);
  }

  /** Re-renders the live list; rows are small and few (bounded by max_live_sessions). */
  setLive(live: SessionSummary[], active: string | null) {
    this.live = [...live].sort((a, b) => b.last_activity_unix_ms - a.last_activity_unix_ms);
    this.active = active;
    this.root.querySelector(".live-count")!.textContent = live.length ? String(live.length) : "";
    if (this.live.length === 0) {
      this.liveList.innerHTML = `<li class="list-empty">No live sessions</li>`;
      return;
    }
    this.liveList.replaceChildren(...this.live.map((session) => this.liveRow(session)));
  }

  renderFooter(connection: Connection, instances: SiblingInstance[]) {
    const running = this.live.filter((session) => session.state === "running").length;
    const others = instances.filter((instance) => !instance.current);
    const theme = this.host.themeChoice();
    this.footer.innerHTML = `
      <button type="button" class="instance-button" aria-haspopup="menu" ${others.length ? "" : "disabled"}>
        <span class="connection-dot" data-connection="${connection}" aria-hidden="true"></span>
        <span class="instance-text"><strong></strong><small></small></span>
        ${others.length ? glyph("chevron-down") : ""}
      </button>
      <button type="button" class="icon-button theme-toggle" aria-label="Theme: ${theme}" title="Theme: ${theme}">${glyph(theme === "dark" ? "moon" : theme === "light" ? "sun" : "monitor")}</button>`;
    this.footer.querySelector(".instance-text strong")!.textContent = CONNECTION_LABELS[connection];
    this.footer.querySelector(".instance-text small")!.textContent = [
      `${this.live.length} live`,
      running ? `${running} running` : "",
      others.length ? `${others.length + 1} instances` : "",
    ].filter(Boolean).join(" · ");
    this.footer.querySelector(".theme-toggle")!.addEventListener("click", () => {
      this.host.toggleTheme();
      this.renderFooter(connection, instances);
    });
    const button = this.footer.querySelector<HTMLButtonElement>(".instance-button")!;
    button.addEventListener("click", () => openMenu(button, instances.map((instance) => ({
      label: instance.workspace.split("/").pop() || instance.workspace,
      detail: instance.current ? "this" : `:${instance.port}`,
      checked: instance.current,
      run: () => {
        if (instance.current) return;
        const url = new URL(location.href);
        url.port = String(instance.port);
        url.hash = "";
        location.assign(url);
      },
    })), "Instances"));
  }

  /** Opens a new chat, optionally with an explicit model. */
  async newChat(model?: string) {
    await this.open({ new: model ? { model } : {} });
  }

  focusSearch() {
    this.search.focus();
  }

  private liveRow(session: SessionSummary) {
    const row = document.createElement("li");
    row.className = "session-row";
    row.dataset.state = session.state;
    row.classList.toggle("active", session.id === this.active);
    row.classList.toggle("unread", session.unread);
    const label = this.host.catalog()?.models.find((model) => model.id === session.model)?.label ?? session.model;
    row.innerHTML = `
      <button type="button" class="session-main" ${session.id === this.active ? 'aria-current="true"' : ""}>
        <span class="session-marker" aria-hidden="true"></span>
        <span class="session-text"><span class="session-title"></span><span class="session-meta"><span class="model-dot"></span><span class="session-model"></span><span class="session-age"></span></span></span>
        ${session.has_draft ? `<span class="draft-mark" title="Unsent draft">${glyph("pencil")}</span>` : ""}
      </button>
      <button type="button" class="icon-button row-menu" aria-label="Session actions" aria-haspopup="menu">${glyph("more")}</button>`;
    row.querySelector(".session-title")!.textContent = session.title || "New chat";
    row.querySelector(".session-model")!.textContent = label;
    row.querySelector(".session-age")!.textContent = formatAge(session.last_activity_unix_ms);
    row.querySelector<HTMLElement>(".model-dot")!.style.background = modelColor(session.model);
    row.querySelector<HTMLElement>(".session-main")!.setAttribute("aria-label", [
      session.title || "New chat", label, session.state === "running" ? "running" : session.state === "error" ? "error" : "",
      session.unread ? "unread" : "", session.has_draft ? "has draft" : "",
    ].filter(Boolean).join(", "));
    row.querySelector(".session-main")!.addEventListener("click", () => this.activate(session.id));
    const menu = row.querySelector<HTMLButtonElement>(".row-menu")!;
    menu.addEventListener("click", () => openMenu(menu, [
      { label: "Fork", detail: "", run: () => void this.open({ fork: { session: session.id } }) },
      { label: session.state === "running" ? "Stop and close…" : "Close", danger: true, run: () => void this.close(session) },
    ], "Session actions"));
    return row;
  }

  private bind() {
    this.root.querySelector(".new-chat-button")!.addEventListener("click", () => void this.newChat());
    const modelButton = this.root.querySelector<HTMLButtonElement>(".new-chat-model")!;
    modelButton.addEventListener("click", () => {
      const models = this.host.catalog();
      if (!models) return;
      openMenu(modelButton, models.models.map((model) => ({
        label: model.label,
        swatch: modelColor(model.id),
        run: () => void this.newChat(model.id),
      })), "New chat with model");
    });
    this.search.addEventListener("focus", () => {
      if (!this.historyLoaded) void this.loadHistory(true);
    });
    this.search.addEventListener("input", () => {
      clearTimeout(this.searchTimer);
      this.searchTimer = window.setTimeout(() => void this.loadHistory(true), 160);
    });
    this.more.addEventListener("click", () => void this.loadHistory(false));
    void this.loadHistory(true);
  }

  private async activate(id: string) {
    this.host.navigated();
    if (id === this.active) return;
    try {
      await this.host.api.command("activate", { session: id });
    } catch (error) {
      toast(describeError(error), "warning");
    }
  }

  private async open(spec: OpenSpec) {
    try {
      await this.host.api.command("open_session", spec);
      this.host.navigated();
    } catch (error) {
      toast(describeError(error), "warning");
    }
  }

  private async close(session: SessionSummary) {
    const running = session.state === "running";
    if (running && !confirm(`"${session.title || "New chat"}" is running. Stop it and close the session?`)) return;
    try {
      await this.host.api.command("close_session", { session: session.id, force: running });
    } catch (error) {
      toast(describeError(error), "warning");
    }
  }

  private async loadHistory(reset: boolean) {
    const query = this.search.value.trim();
    if (reset) {
      this.historyCursor = null;
      this.historyQuery = query;
    }
    this.historyRequest?.abort();
    const request = new AbortController();
    this.historyRequest = request;
    this.more.disabled = true;
    try {
      const page = await this.host.api.query("history", { query: this.historyQuery, cursor: this.historyCursor });
      if (this.historyRequest !== request) return;
      this.historyLoaded = true;
      const rows = page.sessions.map((session) => this.historyRow(session));
      if (reset) {
        this.historyList.replaceChildren(...rows);
        if (rows.length === 0) this.historyList.innerHTML = `<li class="list-empty">${query ? "No matches" : "No saved sessions"}</li>`;
      } else {
        this.historyList.append(...rows);
      }
      this.historyCursor = page.next_cursor;
      this.more.hidden = page.next_cursor === null;
    } catch (error) {
      if (request.signal.aborted) return;
      this.historyList.innerHTML = `<li class="list-empty error"></li>`;
      this.historyList.firstElementChild!.textContent = describeError(error);
    } finally {
      this.more.disabled = false;
    }
  }

  private historyRow(session: PersistedSession) {
    const row = document.createElement("li");
    row.className = "session-row history-row";
    row.innerHTML = `<button type="button" class="session-main"><span class="session-text"><span class="session-title"></span><span class="session-meta"><span class="model-dot"></span><span class="session-model"></span><span class="session-age"></span></span></span></button>`;
    row.querySelector(".session-title")!.textContent = session.preview || session.session_id.slice(0, 8);
    row.querySelector(".session-model")!.textContent = this.host.catalog()?.models.find((model) => model.id === session.model)?.label ?? session.model;
    row.querySelector(".session-age")!.textContent = formatAge(session.started_at_unix_ms);
    row.querySelector<HTMLElement>(".model-dot")!.style.background = modelColor(session.model);
    row.querySelector<HTMLElement>(".session-main")!.title = "Resume";
    row.querySelector(".session-main")!.addEventListener("click", () => void this.open({ resume: { session: session.session_id } }));
    return row;
  }
}
