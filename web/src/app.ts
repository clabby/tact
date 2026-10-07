import "./shell/theme.css";
import "./shell/shell.css";
import { ApiClient, ApiError, describeError } from "./core/api-client";
import { Transcript } from "./chat/chat";
import { Composer } from "./chat/composer";
import { setAttentionBadge } from "./shell/favicon";
import { effortColor, modelColor } from "./core/format";
import { glyph } from "./ui/glyphs";
import { layoutReducer, loadLayout, type Layout, type LayoutAction, type View } from "./shell/layout";
import { openConfigEditor, openContextDiagnostics, openMemories, openPhoneLink, openReflect } from "./shell/features";
import { openSubagents, type SubagentsView } from "./chat/subagents";
import { formatAge } from "./core/format";
import { Palette, type PaletteCommand } from "./ui/palette";
import { PromptRail } from "./chat/prompt-rail-view";
import { mountReviewPanel } from "./review/review-panel";
import { Sidebar } from "./shell/sidebar";
import { effectiveSpeed, speedChoices } from "./core/speed";
import { Store, type Change } from "./core/store";
import { StreamClient } from "./core/stream";
import { ThemeController } from "./core/theme";
import { openSheet } from "./ui/sheet";
import { toast } from "./ui/toast";
import type { CommandName, Commands, ModelCatalog, SiblingInstance } from "./core/wire";

const root = document.getElementById("app")!;
const SIDEBAR_KEY = "tact.web.sidebar";
const isMac = /Mac|iPhone|iPad/.test(navigator.platform);

/** The full-window message shown when this browser has no valid login. */
function showLocked(reason = "Open the login link from Tact on this device to continue.") {
  document.title = "Tact · Signed out";
  root.innerHTML = `<div class="gate"><div class="gate-card" role="alert">
    <div class="gate-mark">${glyph("keyboard")}</div>
    <h1>Signed out</h1><p></p>
    <p class="gate-hint">In the terminal, choose <strong>Open in browser</strong> from Actions, or copy the web link. It signs this browser in.</p>
  </div></div>`;
  root.querySelector(".gate-card p")!.textContent = reason;
}

function showUnreachable(error: unknown, retry: () => void) {
  root.innerHTML = `<div class="gate"><div class="gate-card" role="alert">
    <div class="gate-mark">${glyph("alert")}</div>
    <h1>Tact is unreachable</h1><p></p>
    <button type="button" class="button primary">Try again</button>
  </div></div>`;
  root.querySelector(".gate-card p")!.textContent = describeError(error);
  root.querySelector("button")!.addEventListener("click", retry);
}

class App {
  private readonly store = new Store();
  private readonly theme = new ThemeController();
  private readonly palette = new Palette();
  private readonly transcript: Transcript;
  private readonly composer: Composer;
  private readonly rail: PromptRail;
  private readonly sidebar: Sidebar;
  private readonly stream: StreamClient;
  private readonly shell: HTMLElement;
  private layout: Layout;
  private catalog: ModelCatalog | null = null;
  private subagents: SubagentsView | null = null;
  private instances: SiblingInstance[] = [];
  private review: { dispose(): void; setVisible?(visible: boolean): void } | null = null;
  private readonly reviewListeners = {
    active: new Set<() => void>(),
    workspace: new Set<() => void>(),
    running: new Set<() => void>(),
    theme: new Set<() => void>(),
  };
  private shownSession: string | null = null;
  private wasRunning = false;

  constructor(private readonly api: ApiClient) {
    root.innerHTML = `
      <div class="app">
        <aside class="sidebar" aria-label="Sessions"></aside>
        <div class="scrim" aria-hidden="true"></div>
        <main class="main">
          <header class="chat-header">
            <button type="button" class="icon-button menu-button" aria-label="Toggle sidebar" title="Toggle sidebar">${glyph("sidebar", "glyph glyph-open")}${glyph("sidebar-expand", "glyph glyph-expand")}</button>
            <div class="chat-title"><h1>Tact</h1><div class="chat-sub"><span class="model-dot"></span><span class="chat-model"></span><span class="chat-effort"></span></div></div>
            <nav class="view-tabs" role="tablist" aria-label="View">
              <button type="button" class="view-tab" role="tab" id="tab-chat" aria-controls="view-chat" data-view="chat">Chat</button>
              <button type="button" class="view-tab" role="tab" id="tab-review" aria-controls="view-review" data-view="review">Review<span class="tab-count" hidden></span></button>
            </nav>
            <div class="header-actions">
              <button type="button" class="palette-button" aria-label="Command palette">${glyph("search")}<span>Search</span><kbd>${isMac ? "⌘K" : "Ctrl K"}</kbd></button>
            </div>
          </header>
          <section class="view chat" id="view-chat" role="tabpanel" aria-labelledby="tab-chat">
            <div class="connection-banner" role="status" hidden></div>
            <div class="transcript-scroller"></div>
            <button type="button" class="jump-latest" hidden>${glyph("arrow-down")}Latest</button>
            <div class="dock"></div>
          </section>
          <section class="view review-view" id="view-review" role="tabpanel" aria-labelledby="tab-review" hidden>
            <div class="panel-body"></div>
          </section>
        </main>
      </div>`;
    this.shell = root.querySelector(".app")!;
    this.layout = loadLayout(innerWidth, localStorage.getItem(SIDEBAR_KEY) === "collapsed");
    this.transcript = new Transcript(
      root.querySelector(".transcript-scroller")!,
      root.querySelector(".jump-latest")!,
      () => this.theme.current,
    );
    this.rail = new PromptRail(root.querySelector("#view-chat")!, root.querySelector(".transcript-scroller")!, () => this.store.state.session);
    this.composer = new Composer(root.querySelector(".dock")!, {
      api,
      catalog: () => this.catalog,
      running: () => this.activeRunning(),
      openRecentPrompts: () => this.openRecentPrompts(),
      openContext: () => this.withSession((id) => void openContextDiagnostics(api, id)),
      actions: (query) => this.palette.matching(query),
      openSubagents: () => this.openSubagents(),
    });
    this.sidebar = new Sidebar(root.querySelector(".sidebar")!, {
      api,
      catalog: () => this.catalog,
      navigated: () => this.dispatch({ type: "toggle-drawer", open: false }),
      setTheme: (choice) => this.theme.set(choice),
      themeChoice: () => this.theme.choice,
      resolvedTheme: () => this.theme.current,
    });
    this.stream = new StreamClient({
      url: "./api/stream",
      connect: (url) => new EventSource(url, { withCredentials: true }),
      onEvent: (event) => this.store.dispatch(event),
      onConnection: (connection) => this.store.setConnection(connection),
      authorized: async () => {
        try {
          await api.instance();
          return true;
        } catch (error) {
          if (error instanceof ApiError && error.status === 401) return false;
          throw error;
        }
      },
    });
    this.store.subscribe((changes) => this.apply(changes));
    this.theme.subscribe(() => {
      this.transcript.rerender();
      this.subagents?.themeChanged();
      this.sidebar.renderFooter(this.store.state.connection, this.instances);
      for (const listener of this.reviewListeners.theme) listener();
    });
    this.bindShell();
    this.registerCommands();
    this.applyLayout();
    this.transcript.show(null);
    this.composer.show(null);
  }

  start() {
    this.stream.start();
    // The review mounts up front so its diff is loaded and watched before the tab is first opened.
    this.ensureReview();
    void this.api.query("models").then((catalog) => {
      this.catalog = catalog;
      this.composer.settingsChanged();
      this.renderHeader();
      this.sidebar.setLive(this.store.state.live, this.store.state.active);
    }).catch((error) => toast(`Could not load models: ${describeError(error)}`, "warning"));
    void this.api.instance().then((instance) => this.sidebar.setWorkspace(instance.repository, instance.workspace)).catch(() => {});
    void this.api.instances().then(({ instances }) => {
      this.instances = instances;
      this.sidebar.renderFooter(this.store.state.connection, instances);
    }).catch(() => {});
    document.addEventListener("visibilitychange", () => {
      if (document.visibilityState === "visible") this.stream.reconnectNow();
    });
    addEventListener("online", () => this.stream.reconnectNow());
  }

  private apply(changes: readonly Change[]) {
    const state = this.store.state;
    for (const change of changes) {
      switch (change.type) {
        case "connection":
          if (state.connection === "locked") {
            this.stream.stop();
            showLocked("This browser's login is no longer valid.");
            return;
          }
          this.renderConnection();
          if (state.connection === "open") this.composer.connectionOpened();
          break;
        case "live": {
          this.sidebar.setLive(state.live, state.active);
          this.sidebar.renderFooter(state.connection, this.instances);
          this.composer.statusChanged();
          this.renderHeader();
          const running = this.store.anyRunning();
          if (running !== this.wasRunning) {
            this.wasRunning = running;
            for (const listener of this.reviewListeners.running) listener();
          }
          break;
        }
        case "session":
          this.transcript.show(state.session && this.sessionSource(state.session.id));
          this.rail.refresh();
          this.composer.show(state.session);
          this.renderHeader();
          if ((state.session?.id ?? null) !== this.shownSession) {
            this.shownSession = state.session?.id ?? null;
            for (const listener of this.reviewListeners.active) listener();
          }
          break;
        case "entry":
          this.transcript.entryChanged(change.id);
          this.rail.refresh();
          // The first entry fixes the model; settings controls depend on it.
          if (change.added && state.session?.order.length === 1) this.composer.settingsChanged();
          break;
        case "status":
          this.composer.statusChanged();
          break;
        case "queue":
          this.composer.queueChanged();
          break;
        case "draft":
          this.composer.draftChanged();
          break;
        case "settings":
          this.composer.settingsChanged();
          this.renderHeader();
          break;
        case "context":
          this.composer.contextChanged();
          break;
        case "subagents":
          this.subagents?.rosterChanged();
          this.composer.statusChanged();
          break;
        case "subagent_entry":
          this.subagents?.entryChanged(change.agent, change.id);
          break;
        case "workspace":
          for (const listener of this.reviewListeners.workspace) listener();
          break;
      }
    }
  }

  private activeRunning() {
    const state = this.store.state;
    const summary = state.live.find((session) => session.id === state.session?.id);
    return summary ? summary.state === "running" : state.session?.running ?? false;
  }

  private renderHeader() {
    const session = this.store.state.session;
    const title = session?.title || (session ? "New chat" : "Tact");
    root.querySelector(".chat-title h1")!.textContent = title;
    const label = this.catalog?.models.find((model) => model.id === session?.model)?.label ?? session?.model ?? "";
    root.querySelector(".chat-model")!.textContent = label;
    const effort = root.querySelector<HTMLElement>(".chat-effort")!;
    const speedTier = session && this.catalog ? effectiveSpeed(this.catalog.models.find((model) => model.id === session.model), this.catalog, session.speed) : session?.speed;
    effort.textContent = session
      ? [session.effort, session.reasoningMode === "pro" ? "pro" : "", speedTier === "standard" ? "" : speedTier].filter(Boolean).join(" · ")
      : "";
    effort.style.color = session ? effortColor(session.effort) : "";
    root.querySelector<HTMLElement>(".chat-sub .model-dot")!.style.background = session ? modelColor(session.model) : "transparent";
    const running = this.activeRunning();
    this.shell.classList.toggle("running", running);
    const unread = this.store.state.live.filter((summary) => summary.unread).length;
    document.title = `${unread ? `(${unread}) ` : ""}${title} · Tact`;
    setAttentionBadge(unread > 0);
  }

  private renderConnection() {
    const banner = root.querySelector<HTMLElement>(".connection-banner")!;
    const connection = this.store.state.connection;
    banner.hidden = connection === "open" || (connection === "connecting" && !this.store.state.session);
    banner.innerHTML = `<span class="spinner"></span>${connection === "reconnecting" ? "Reconnecting to Tact…" : "Connecting…"}`;
    this.sidebar.renderFooter(connection, this.instances);
  }

  private dispatch(action: LayoutAction) {
    const next = layoutReducer(this.layout, action);
    if (next === this.layout) return;
    const viewChanged = next.view !== this.layout.view;
    this.layout = next;
    this.applyLayout();
    if (!viewChanged) return;
    this.review?.setVisible?.(next.view === "review");
    if (next.view === "chat") this.composer.focus();
  }

  private applyLayout() {
    const { viewport, drawerOpen, sidebarCollapsed, view } = this.layout;
    this.shell.dataset.viewport = viewport;
    this.shell.dataset.view = view;
    this.shell.classList.toggle("drawer-open", drawerOpen);
    this.shell.classList.toggle("sidebar-collapsed", sidebarCollapsed);
    localStorage.setItem(SIDEBAR_KEY, sidebarCollapsed ? "collapsed" : "open");
    for (const tab of root.querySelectorAll<HTMLButtonElement>(".view-tab")) {
      const selected = tab.dataset.view === view;
      tab.setAttribute("aria-selected", String(selected));
      tab.tabIndex = selected ? 0 : -1;
    }
    root.querySelector<HTMLElement>("#view-chat")!.hidden = view !== "chat";
    root.querySelector<HTMLElement>("#view-review")!.hidden = view !== "review";
    root.querySelector<HTMLElement>(".sidebar")!.toggleAttribute("inert", viewport === "desktop" ? sidebarCollapsed : !drawerOpen);
  }

  private showView(view: View) {
    this.dispatch({ type: "view", view });
  }

  private ensureReview() {
    if (this.review) return;
    const body = root.querySelector<HTMLElement>(".panel-body")!;
    const subscribe = (set: Set<() => void>) => (listener: () => void) => {
      set.add(listener);
      return () => void set.delete(listener);
    };
    this.review = mountReviewPanel(body, {
      api: this.api,
      activeSession: () => this.store.state.session?.id ?? null,
      onActiveSessionChange: subscribe(this.reviewListeners.active),
      onWorkspaceChanged: subscribe(this.reviewListeners.workspace),
      anyRunning: () => this.store.anyRunning(),
      onRunningChange: subscribe(this.reviewListeners.running),
      sendToChat: (markdown) => {
        this.composer.append(markdown);
        this.showView("chat");
      },
      theme: () => this.theme.current,
      onThemeChange: subscribe(this.reviewListeners.theme),
    });
    this.review.setVisible?.(this.layout.view === "review");
    // The Review tab mirrors the added and removed line totals the panel displays. The observer
    // fires whenever the panel re-renders its statistics, so the badge follows live refreshes.
    const badge = root.querySelector<HTMLElement>(".tab-count")!;
    let frame = 0;
    new MutationObserver(() => {
      frame ||= requestAnimationFrame(() => {
        frame = 0;
        const additions = body.querySelector("#change-stats .add")?.textContent ?? "";
        const deletions = body.querySelector("#change-stats .del")?.textContent ?? "";
        badge.hidden = additions === "" || (additions === "+0" && deletions === "\u22120");
        badge.innerHTML = `<span class="add">${additions}</span><span class="del">${deletions}</span>`;
      });
    }).observe(body, { childList: true, characterData: true, subtree: true });
  }

  private bindShell() {
    root.querySelector(".menu-button")!.addEventListener("click", () => this.dispatch({ type: "toggle-sidebar" }));
    root.querySelector(".drawer-close")!.addEventListener("click", () => this.dispatch({ type: "toggle-sidebar" }));
    root.querySelector(".scrim")!.addEventListener("click", () => this.dispatch({ type: "escape" }));
    for (const tab of root.querySelectorAll<HTMLButtonElement>(".view-tab")) {
      tab.addEventListener("click", () => this.showView(tab.dataset.view as View));
    }
    root.querySelector(".palette-button")!.addEventListener("click", () => this.palette.open());
    addEventListener("resize", () => this.dispatch({ type: "resize", width: innerWidth }), { passive: true });
    // Any input other than the confirming Esc cancels a pending interrupt.
    document.addEventListener("keydown", (event) => {
      if (event.key !== "Escape" && !["Shift", "Control", "Alt", "Meta"].includes(event.key)) this.composer.disarmInterrupt();
    }, true);
    document.addEventListener("pointerdown", () => this.composer.disarmInterrupt(), true);
    document.addEventListener("keydown", (event) => {
      const mod = isMac ? event.metaKey : event.ctrlKey;
      if (mod && event.key.toLowerCase() === "b") {
        event.preventDefault();
        this.dispatch({ type: "toggle-sidebar" });
      } else if (mod && event.key.toLowerCase() === "k") {
        event.preventDefault();
        if (this.palette.isOpen) this.palette.close();
        else this.palette.open();
      } else if (event.key === "Escape" && !event.defaultPrevented && !document.querySelector("dialog[open]")) {
        this.escape();
      } else if (event.key === "/" && !isEditable(event.target)) {
        event.preventDefault();
        this.composer.focus();
      } else if (event.key === "?" && !isEditable(event.target)) {
        event.preventDefault();
        showShortcuts();
      }
    });
  }

  /** Esc closes the drawer; otherwise it interrupts a running turn, but only when pressed twice. */
  private escape() {
    if (this.layout.drawerOpen) {
      this.dispatch({ type: "escape" });
    } else if (!this.activeRunning()) {
      this.composer.disarmInterrupt();
    } else if (this.composer.interruptArmed) {
      this.composer.interrupt();
    } else {
      this.composer.armInterrupt();
    }
  }

  private sessionSource(id: string) {
    return {
      key: id,
      data: this.store.state.session!,
      detail: (entry: number) => this.api.toolDetail(id, entry),
      image: (entry: number, index: number) => this.api.imageUrl(id, entry, index),
    };
  }

  private withSession(action: (id: string) => void) {
    const id = this.store.state.session?.id;
    if (id) action(id);
    else toast("Open a session first.", "info");
  }

  private openRecentPrompts() {
    this.withSession((session) => this.palette.pick({
      placeholder: "Search recent prompts",
      empty: "No recent prompts",
      load: async (query) => (await this.api.query("recent_prompts", { session, query })).prompts.map((prompt, index): PaletteCommand => ({
        id: `prompt:${index}`,
        title: prompt.text.replace(/\s+/g, " ").trim(),
        group: "Recent prompts",
        icon: "history",
        hint: formatAge(prompt.recorded_at_unix_ms),
        run: () => this.composer.replaceText(prompt.text),
      })),
    }));
  }

  private openSubagents() {
    if (this.subagents) return;
    this.subagents = openSubagents(this.api, () => this.store.state.session, () => this.theme.current, () => {
      this.subagents = null;
    });
  }

  /** Sends a command, reporting a refusal as a toast. */
  private command<Name extends CommandName>(name: Name, args: Commands[Name]) {
    void (this.api.command as (name: CommandName, args?: unknown) => Promise<unknown>)(name, args)
      .catch((error) => toast(describeError(error), "warning"));
  }

  private registerCommands() {
    const palette = this.palette;
    palette.register(() => this.store.state.live
      .filter((summary) => summary.id !== this.store.state.active)
      .map((summary): PaletteCommand => ({
        id: `activate:${summary.id}`,
        title: summary.title || "New chat",
        group: "Live sessions",
        icon: summary.state === "running" ? "circle" : "message",
        hint: summary.state === "running" ? "running" : summary.unread ? "unread" : "",
        run: () => this.command("activate", { session: summary.id }),
      })));
    palette.register(() => [
      { id: "new", title: "New chat", group: "Sessions", icon: "plus", run: () => void this.sidebar.newChat() },
      ...(this.catalog?.models ?? []).map((model): PaletteCommand => ({
        id: `new:${model.id}`, title: `New chat with ${model.label}`, group: "Sessions", icon: "plus",
        run: () => void this.sidebar.newChat(model.id),
      })),
      { id: "resume", title: "Resume a session…", group: "Sessions", icon: "history", keywords: "history", run: () => this.openHistory() },
    ]);
    palette.register(() => {
      const session = this.store.state.session;
      if (!session) return [];
      const id = session.id;
      const running = this.activeRunning();
      const started = session.order.length > 0;
      const model = this.catalog?.models.find((candidate) => candidate.id === session.model);
      // As in the terminal, work that rewrites the conversation waits for an idle session.
      const rewriteBlock = !started ? "Available after the first turn"
        : running ? "Wait for the current turn to finish"
        : session.queue.length > 0 ? "Wait for the queued messages to send"
        : undefined;
      const fixed = "Fixed after the first prompt";
      const commands: PaletteCommand[] = [];
      commands.push(
        { id: "stop", title: "Stop", group: "Session", icon: "stop", hint: "esc", keywords: "interrupt cancel", unavailable: running ? undefined : "Nothing is running", run: () => this.command("interrupt", { session: id }) },
        { id: "recent", title: "Recent prompts…", group: "Session", icon: "history", hint: "↑", run: () => this.openRecentPrompts() },
        { id: "compact", title: "Compact context", group: "Session", icon: "compact", unavailable: rewriteBlock, run: () => this.command("compact", { session: id }) },
        { id: "reflect", title: "Reflect…", group: "Session", icon: "reflect", keywords: "reflection learn", unavailable: rewriteBlock, run: () => openReflect(this.api, id) },
        { id: "handoff", title: "Prepare handoff", group: "Session", icon: "handoff", unavailable: rewriteBlock, run: () => this.command("handoff", { session: id }) },
        { id: "subagents", title: "Subagents", group: "Session", icon: "agents", hint: String(session.subagents.agents.length || ""), keywords: "agents tree", run: () => this.openSubagents() },
        { id: "context", title: "Context diagnostics", group: "Session", icon: "gauge", keywords: "tokens debug", run: () => void openContextDiagnostics(this.api, id) },
        { id: "attach", title: "Attach image…", group: "Session", icon: "image", unavailable: model?.provider === "anthropic" ? "Claude sessions are text-only" : undefined, run: () => root.querySelector<HTMLButtonElement>(".attach-chip")?.click() },
        { id: "fork", title: "Fork session", group: "Session", icon: "fork", unavailable: started ? undefined : "Available after the first turn", run: () => this.command("open_session", { fork: { session: id } }) },
        { id: "close", title: "Close session", group: "Session", icon: "trash", run: () => {
          if (running && !confirm("A turn is running. Stop it and close the session?")) return;
          this.command("close_session", { session: id, force: running });
        } },
      );
      if (started) {
        commands.push({ id: "model", title: "Change model", group: "Settings", icon: "sparkles", unavailable: fixed, run() {} });
      } else {
        commands.push(...(this.catalog?.models ?? []).filter((candidate) => candidate.id !== session.model).map((candidate): PaletteCommand => ({
          id: `model:${candidate.id}`, title: `Model: ${candidate.label}`, group: "Settings", icon: "sparkles",
          run: () => this.command("set_model", { session: id, model: candidate.id }),
        })));
      }
      if (started && model?.effort_fixed_after_start) {
        commands.push({ id: "effort", title: "Change effort", group: "Settings", icon: "brain", unavailable: fixed, run() {} });
      } else {
        commands.push(...(this.catalog?.efforts ?? []).filter((effort) => effort !== session.effort).map((effort): PaletteCommand => ({
          id: `effort:${effort}`, title: `Effort: ${effort}`, group: "Settings", icon: "brain",
          run: () => this.command("set_effort", { session: id, effort }),
        })));
      }
      if (started && model?.reasoning_modes.includes("pro")) {
        commands.push({ id: "mode", title: "Pro reasoning", group: "Settings", icon: "sparkles", unavailable: fixed, run() {} });
      } else if (model?.reasoning_modes.includes("pro")) {
        const pro = session.reasoningMode === "pro";
        commands.push({ id: "mode", title: pro ? "Turn pro reasoning off" : "Turn pro reasoning on", group: "Settings", icon: "sparkles",
          run: () => this.command("set_reasoning_mode", { session: id, mode: pro ? "standard" : "pro" }) });
      }
      const catalog = this.catalog;
      if (catalog) {
        const current = effectiveSpeed(model, catalog, session.speed);
        commands.push(...speedChoices(model, catalog).filter(({ tier }) => tier !== current).map(({ preference, tier }): PaletteCommand => ({
          id: `speed:${tier}`, title: `Speed: ${tier}`, group: "Settings", icon: "bolt",
          run: () => this.command("set_speed", { session: id, speed: preference }),
        })));
      }
      return commands;
    });
    palette.register(() => [
      { id: "memory", title: "Memory", group: "Tact", icon: "database", keywords: "memories", run: () => void openMemories(this.api) },
      { id: "config", title: "Edit configuration", group: "Tact", icon: "settings", keywords: "config settings", run: () => void openConfigEditor(this.api) },
      { id: "phone", title: "Open on your phone (QR code)", group: "Tact", icon: "monitor", keywords: "qr scan mobile link", run: () => void openPhoneLink(this.api) },
      { id: "reload", title: "Reload configuration", group: "Tact", icon: "refresh", keywords: "config", run: () => {
        void this.api.command("reload_config").then(() => toast("Configuration reloaded."), (error) => toast(describeError(error), "danger"));
      } },
      { id: "view", title: this.layout.view === "review" ? "Show chat" : "Show review", group: "View", icon: this.layout.view === "review" ? "message" : "panel", keywords: "diff changes overview", run: () => this.showView(this.layout.view === "review" ? "chat" : "review") },
      { id: "sidebar", title: "Toggle sidebar", group: "View", icon: "sidebar", hint: isMac ? "⌘B" : "Ctrl B", run: () => this.dispatch({ type: "toggle-sidebar" }) },
      { id: "composer", title: "Focus composer", group: "View", icon: "pencil", hint: "/", run: () => this.composer.focus() },
      { id: "shortcuts", title: "Keyboard shortcuts", group: "View", icon: "keyboard", hint: "?", run: () => showShortcuts() },
      ...(["system", "light", "dark"] as const).filter((choice) => choice !== this.theme.choice).map((choice): PaletteCommand => ({
        id: `theme:${choice}`, title: `Theme: ${choice}`, group: "View", icon: choice === "dark" ? "moon" : choice === "light" ? "sun" : "monitor",
        hint: this.theme.choice === choice ? "current" : "",
        run: () => {
          this.theme.set(choice);
          this.sidebar.renderFooter(this.store.state.connection, this.instances);
        },
      })),
    ]);
  }

  private openHistory() {
    this.palette.pick({
      placeholder: "Search saved sessions",
      empty: "No saved sessions",
      load: async (query) => (await this.api.query("history", { query })).sessions.map((session): PaletteCommand => ({
        id: `resume:${session.session_id}`,
        title: session.preview || session.session_id,
        group: "History",
        icon: "history",
        hint: formatAge(session.started_at_unix_ms),
        run: () => this.command("open_session", { resume: { session: session.session_id } }),
      })),
    });
  }
}

const SHORTCUTS: [string, string][] = [
  [isMac ? "⌘K" : "Ctrl K", "Command palette"],
  [isMac ? "⌘B" : "Ctrl B", "Show or hide the sidebar"],
  ["/", "Focus the composer"],
  ["Enter", "Send, or queue while a turn runs"],
  ["Shift Enter", "New line"],
  ["↑ in an empty composer", "Recent prompts"],
  ["@  @@  $", "Mention a file, a session, a skill"],
  ["! at the start", "Run a shell command"],
  ["Esc", "Stop the turn (empty composer), close sheets"],
  ["?", "This list"],
];

function showShortcuts() {
  const sheet = openSheet("Keyboard shortcuts");
  const list = document.createElement("dl");
  list.className = "facts shortcuts";
  for (const [keys, action] of SHORTCUTS) {
    const term = document.createElement("dt");
    term.innerHTML = keys.split("  ").map((key) => `<kbd></kbd>`).join(" ");
    term.querySelectorAll("kbd").forEach((kbd, index) => { kbd.textContent = keys.split("  ")[index]!; });
    const description = document.createElement("dd");
    description.textContent = action;
    list.append(term, description);
  }
  sheet.body.append(list);
}

function isEditable(target: EventTarget | null) {
  return target instanceof HTMLElement && (target.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(target.tagName));
}

async function main() {
  const api = new ApiClient();
  const token = new URLSearchParams(location.hash.slice(1)).get("k");
  if (token) {
    // The fragment is a credential: drop it from the address bar and history before anything else.
    history.replaceState(null, "", location.pathname + location.search);
    try {
      await api.login(token);
    } catch (error) {
      if (error instanceof ApiError && error.status === 401) return showLocked("This login link is invalid or has expired.");
      return showUnreachable(error, () => location.reload());
    }
  }
  try {
    await api.instance();
  } catch (error) {
    if (error instanceof ApiError && error.status === 401) return showLocked();
    return showUnreachable(error, () => void main());
  }
  new App(api).start();
}

void main();
