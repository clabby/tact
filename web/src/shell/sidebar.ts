import { describeError, type ApiClient } from "../core/api-client";
import { formatAge, modelColor } from "../core/format";
import { glyph } from "../ui/glyphs";
import { createSphere } from "../ui/dot-sphere";
import { openMenu } from "../ui/menu";
import { orderSessions } from "./session-pins";
import { SessionTitles } from "./session-title";
import type { Connection } from "../core/store";
import { toast } from "../ui/toast";
import type { HistoryScope, ModelCatalog, OpenSpec, PersistedSession, SessionSummary, SiblingInstance } from "../core/wire";

const PINS_KEY = "tact.web.pinned-sessions";
const HISTORY_SCOPE_KEY = "tact.web.history-scope";
const isMac = /Mac|iPhone|iPad/.test(navigator.platform);

const HISTORY_SCOPE_LABELS: Record<HistoryScope, string> = {
  repository: "All checkouts",
  workspace: "This workspace",
};

export type SidebarHost = {
  api: ApiClient;
  catalog(): ModelCatalog | null;
  /** Called after the user picked a session, so phones can close the drawer. */
  navigated(): void;
  renameSession(session: string): void;
  setTheme(choice: "system" | "light" | "dark"): void;
  themeChoice(): "system" | "light" | "dark";
  /** The scheme in effect, which a "system" choice resolves to. */
  resolvedTheme(): "light" | "dark";
  /** A session row's workspace name, or null when the session works in the default workspace. */
  workspaceToken(workspace: string): string | null;
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
  private readonly scopeButton: HTMLButtonElement;
  private readonly footer: HTMLElement;
  private live: SessionSummary[] = [];
  private active: string | null = null;
  private historyCursor: string | null = null;
  private historyQuery = "";
  /**
   * Which stored sessions History lists: every checkout of the repository, or only the active
   * session's workspace, which the server resolves.
   */
  private historyScope: HistoryScope = localStorage.getItem(HISTORY_SCOPE_KEY) === "workspace" ? "workspace" : "repository";
  /** The active session's workspace, which a "workspace" History scope lists. */
  private activeWorkspace: string | null = null;
  /** What the search field filters the live list by; History is searched on the server. */
  private filter = "";
  /** Sessions this browser keeps above the rest of the live list. */
  private pinned = new Set<string>(JSON.parse(localStorage.getItem(PINS_KEY) ?? "[]") as string[]);
  private historyRequest: AbortController | null = null;
  private historyLoaded = false;
  /** Every History page loaded so far; rows for sessions that are live are not shown. */
  private history: PersistedSession[] = [];
  /** Snippets of the live sessions whose conversation matches `historyQuery`, by session id. */
  private liveMatches: Record<string, string> = {};
  private searchTimer = 0;
  private readonly titles = new SessionTitles();

  constructor(private readonly root: HTMLElement, private readonly host: SidebarHost) {
    root.innerHTML = `
      <div class="sidebar-head">
        <div class="brand"><span class="brand-mark" aria-hidden="true">t</span><div class="brand-text"><strong>Tact</strong><span class="brand-workspace"></span></div></div>
        <button type="button" class="icon-button drawer-close" aria-label="Hide sidebar" title="Hide sidebar (${isMac ? "⌘B" : "Ctrl B"})">${glyph("sidebar", "glyph glyph-fold")}${glyph("close", "glyph glyph-close")}</button>
      </div>
      <div class="new-chat">
        <button type="button" class="new-chat-button">${glyph("plus")}<span>New chat</span></button>
        <button type="button" class="new-chat-model" aria-label="New chat with model" aria-haspopup="menu">${glyph("chevron-down")}</button>
      </div>
      <label class="chat-search">${glyph("search")}<input type="search" placeholder="Search chats" aria-label="Search chats" autocomplete="off"></label>
      <nav class="sidebar-scroll" aria-label="Sessions">
        <h2 class="section-title live-title">Live <span class="live-count"></span></h2>
        <ul class="session-list live-list" role="list"></ul>
        <h2 class="section-title">History <button type="button" class="history-scope" aria-haspopup="menu"><span class="history-scope-label"></span>${glyph("chevron-down")}</button></h2>
        <ul class="session-list history-list" role="list"></ul>
        <button type="button" class="history-more" hidden>Load more</button>
      </nav>
      <div class="sidebar-foot"></div>`;
    this.liveList = root.querySelector(".live-list")!;
    this.historyList = root.querySelector(".history-list")!;
    this.search = root.querySelector(".chat-search input")!;
    this.more = root.querySelector(".history-more")!;
    this.scopeButton = root.querySelector(".history-scope")!;
    this.footer = root.querySelector(".sidebar-foot")!;
    this.renderScope();
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
    const closed = this.live.some((old) => !live.some((session) => session.id === old.id));
    const workspace = live.find((session) => session.id === active)?.workspace ?? this.activeWorkspace;
    // History loaded before the active session was known already lists the server's workspace.
    const moved = this.activeWorkspace !== null && workspace !== this.activeWorkspace;
    this.activeWorkspace = workspace;
    this.live = orderSessions(live, this.pinned);
    this.active = active;
    this.renderLive();
    // A closed session is only in History once it has been listed again, and the loaded pages
    // predate it. A reload also recomputes which rows the live list now hides. A workspace-scoped
    // History lists the active session's workspace, so it follows the active session.
    const stale = closed || (moved && this.historyScope === "workspace");
    if (stale && this.historyLoaded) void this.loadHistory(true);
    else this.renderHistory();
  }

  private renderLive() {
    const needle = this.filter.trim().toLowerCase();
    // Conversation matches answer the last History query, which lags the field while typing.
    const matches = this.historyQuery === this.filter.trim() ? this.liveMatches : {};
    const visible = needle
      ? this.live.filter((session) => session.id in matches || `${session.title} ${session.model}`.toLowerCase().includes(needle))
      : this.live;
    this.root.querySelector(".live-count")!.textContent = this.live.length ? String(this.live.length) : "";
    // While searching, a Live section with no match would only be noise above the results.
    this.root.querySelector<HTMLElement>(".live-title")!.hidden = needle !== "" && visible.length === 0;
    if (visible.length === 0) {
      this.liveList.innerHTML = needle ? "" : `<li class="list-empty">No live sessions</li>`;
      return;
    }
    this.liveList.replaceChildren(...visible.map((session) => this.liveRow(session, needle ? matches[session.id] : undefined)));
  }

  renderFooter(connection: Connection, instances: SiblingInstance[]) {
    const others = instances.filter((instance) => !instance.current);
    // Live counts running Tact instances, this one included even before the first poll lists it.
    // Running counts sessions: this instance's come from the live stream, the others' from polling.
    const live = others.length + 1;
    const running = others.reduce(
      (total, instance) => total + instance.running,
      this.live.filter((session) => session.state === "running").length,
    );
    const theme = this.host.themeChoice();
    const resolved = this.host.resolvedTheme();
    this.footer.innerHTML = `
      <button type="button" class="instance-button" aria-haspopup="menu" ${others.length ? "" : "disabled"}>
        <span class="connection-dot" data-connection="${connection}" aria-hidden="true"></span>
        <span class="instance-text"><strong></strong><small></small></span>
        ${others.length ? glyph("chevron-down") : ""}
      </button>
      <button type="button" class="icon-button theme-button" aria-haspopup="menu" aria-label="Theme: ${theme}" title="Theme: ${theme}">${glyph(resolved === "dark" ? "moon" : "sun")}</button>`;
    this.footer.querySelector(".instance-text strong")!.textContent = CONNECTION_LABELS[connection];
    this.footer.querySelector(".instance-text small")!.textContent = [
      `${live} live`,
      running ? `${running} running` : "",
    ].filter(Boolean).join(" · ");
    const themeButton = this.footer.querySelector<HTMLButtonElement>(".theme-button")!;
    themeButton.addEventListener("click", () => openMenu(themeButton, (["system", "light", "dark"] as const).map((choice) => ({
      label: choice[0]!.toUpperCase() + choice.slice(1),
      icon: glyph(choice === "dark" ? "moon" : choice === "light" ? "sun" : "monitor"),
      checked: choice === theme,
      run: () => {
        this.host.setTheme(choice);
        this.renderFooter(connection, instances);
      },
    })), "Theme"));
    const button = this.footer.querySelector<HTMLButtonElement>(".instance-button")!;
    button.addEventListener("click", () => openMenu(button, instances.map((instance) => ({
      label: instance.workspace.split("/").pop() || instance.workspace,
      busy: instance.running ? `${instance.running} running` : undefined,
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

  refreshHistory() {
    if (this.historyLoaded) return this.loadHistory(true);
  }

  /** Re-labels every row's workspace, e.g. once the default workspace is known. */
  refreshWorkspaces() {
    for (const row of this.root.querySelectorAll<HTMLElement>(".session-row[data-workspace]")) {
      this.renderWorkspace(row, row.dataset.workspace!);
    }
  }

  private liveRow(session: SessionSummary, snippet: string | undefined) {
    const row = document.createElement("li");
    row.className = "session-row";
    const pinned = this.pinned.has(session.id);
    row.classList.toggle("pinned", pinned);
    row.dataset.state = session.state;
    row.classList.toggle("active", session.id === this.active);
    row.classList.toggle("unread", session.unread);
    const label = this.host.catalog()?.models.find((model) => model.id === session.model)?.label ?? session.model;
    row.innerHTML = `
      <button type="button" class="session-main" ${session.id === this.active ? 'aria-current="true"' : ""}>
        <span class="session-marker" aria-hidden="true"></span>
        <span class="session-text"><span class="session-title"></span><span class="session-meta"><span class="model-dot"></span><span class="session-model"></span><span class="session-workspace"></span><span class="session-age"></span></span></span>
        ${pinned ? `<span class="pin-mark" title="Pinned">${glyph("pin")}</span>` : ""}
        ${session.has_draft ? `<span class="draft-mark" title="Unsent draft">${glyph("pencil")}</span>` : ""}
      </button>
      <button type="button" class="icon-button row-menu" aria-label="Session actions" aria-haspopup="menu">${glyph("more")}</button>`;
    // The globe's visible diameter matches the 8px steady-state dots (the canvas is slightly larger).
    if (session.state === "running") row.querySelector(".session-marker")!.append(createSphere(10, 14));
    this.titles.render(row.querySelector(".session-title")!, session.id, session.title || "New chat", session.title_pending);
    renderSnippet(row, snippet);
    row.querySelector(".session-model")!.textContent = label;
    this.renderWorkspace(row, session.workspace);
    row.querySelector(".session-age")!.textContent = formatAge(session.last_activity_unix_ms);
    row.querySelector<HTMLElement>(".model-dot")!.style.background = modelColor(session.model);
    row.querySelector<HTMLElement>(".session-main")!.setAttribute("aria-label", [
      session.title || "New chat", label, this.host.workspaceToken(session.workspace) ?? "",
      session.state === "running" ? "running" : session.state === "error" ? "error" : "",
      session.unread ? "unread" : "", session.has_draft ? "has draft" : "",
    ].filter(Boolean).join(", "));
    row.querySelector(".session-main")!.addEventListener("click", () => this.activate(session.id));
    const menu = row.querySelector<HTMLButtonElement>(".row-menu")!;
    menu.addEventListener("click", () => openMenu(menu, [
      { label: "Rename session…", icon: glyph("pencil"), run: () => this.host.renameSession(session.id) },
      { label: "Fork", detail: "", run: () => void this.open({ fork: { session: session.id } }) },
      { label: pinned ? "Unpin" : "Pin to top", run: () => this.togglePin(session.id) },
      { label: session.state === "running" ? "Stop and close…" : "Close", danger: true, run: () => void this.close(session) },
    ], "Session actions"));
    return row;
  }

  private renderWorkspace(row: HTMLElement, workspace: string) {
    row.dataset.workspace = workspace;
    const token = row.querySelector<HTMLElement>(".session-workspace")!;
    const name = this.host.workspaceToken(workspace);
    token.hidden = name === null;
    token.textContent = name ?? "";
    token.title = workspace;
  }

  private togglePin(id: string) {
    if (!this.pinned.delete(id)) this.pinned.add(id);
    localStorage.setItem(PINS_KEY, JSON.stringify([...this.pinned]));
    this.setLive(this.live, this.active);
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
      this.filter = this.search.value;
      this.renderLive();
      clearTimeout(this.searchTimer);
      this.searchTimer = window.setTimeout(() => void this.loadHistory(true), 160);
    });
    this.more.addEventListener("click", () => void this.loadHistory(false));
    this.scopeButton.addEventListener("click", () => openMenu(this.scopeButton, (["repository", "workspace"] as const).map((scope) => ({
      label: HISTORY_SCOPE_LABELS[scope],
      checked: scope === this.historyScope,
      run: () => this.setHistoryScope(scope),
    })), "History scope"));
    void this.loadHistory(true);
  }

  private setHistoryScope(scope: HistoryScope) {
    if (scope === this.historyScope) return;
    this.historyScope = scope;
    localStorage.setItem(HISTORY_SCOPE_KEY, scope);
    this.renderScope();
    void this.loadHistory(true);
  }

  private renderScope() {
    const label = HISTORY_SCOPE_LABELS[this.historyScope];
    this.scopeButton.querySelector(".history-scope-label")!.textContent = label;
    this.scopeButton.setAttribute("aria-label", `History scope: ${label}`);
    this.scopeButton.title = this.historyScope === "workspace"
      ? "Showing sessions from this workspace only"
      : "Showing sessions from every checkout of this repository";
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
      const page = await this.host.api.query("history", { query: this.historyQuery, cursor: this.historyCursor, scope: this.historyScope });
      if (this.historyRequest !== request) return;
      this.historyLoaded = true;
      this.history = reset ? page.sessions : [...this.history, ...page.sessions];
      if (reset) {
        this.liveMatches = page.live_matches;
        this.renderLive();
      }
      this.renderHistory();
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

  /** Live sessions already have a row above, so History lists only the others. */
  private renderHistory() {
    const live = new Set(this.live.map((session) => session.id));
    const rows = this.history.filter((session) => !live.has(session.session_id)).map((session) => this.historyRow(session));
    if (rows.length > 0) {
      this.historyList.replaceChildren(...rows);
    } else if (this.historyLoaded) {
      const empty = this.historyQuery
        ? "No matches"
        : this.historyScope === "workspace" ? "No saved sessions in this workspace" : "No saved sessions";
      this.historyList.innerHTML = `<li class="list-empty">${empty}</li>`;
    }
  }

  private historyRow(session: PersistedSession) {
    const row = document.createElement("li");
    row.className = "session-row history-row";
    row.innerHTML = `<button type="button" class="session-main"><span class="session-text"><span class="session-title"></span><span class="session-meta"><span class="model-dot"></span><span class="session-model"></span><span class="session-workspace"></span><span class="session-age"></span></span></span></button>`;
    this.titles.render(row.querySelector(".session-title")!, session.session_id, session.preview || session.session_id.slice(0, 8), session.title_pending ?? false);
    renderSnippet(row, session.snippet);
    row.querySelector(".session-model")!.textContent = this.host.catalog()?.models.find((model) => model.id === session.model)?.label ?? session.model;
    this.renderWorkspace(row, session.workspace);
    row.querySelector(".session-age")!.textContent = formatAge(session.started_at_unix_ms);
    row.querySelector<HTMLElement>(".model-dot")!.style.background = modelColor(session.model);
    row.querySelector<HTMLElement>(".session-main")!.title = "Resume";
    row.querySelector(".session-main")!.addEventListener("click", () => void this.open({ resume: { session: session.session_id } }));
    return row;
  }
}

/** Shows where a search matched a session's conversation, under the row's title. */
function renderSnippet(row: HTMLElement, snippet: string | undefined) {
  if (!snippet) return;
  const line = document.createElement("span");
  line.className = "session-snippet";
  line.textContent = snippet;
  row.querySelector(".session-title")!.after(line);
}
