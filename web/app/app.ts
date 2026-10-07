import "./theme.css";
import "./shell.css";
import { ApiClient, ApiError, describeError } from "./api-client";
import { Transcript } from "./chat";
import { Composer } from "./composer";
import { effortColor, modelColor } from "./format";
import { glyph } from "./glyphs";
import { layoutReducer, loadLayout, saveLayout, type Layout, type LayoutAction } from "./layout";
import { openConfigEditor, openContextDiagnostics, openMemories, openReflect, openSubagents, type SubagentsView } from "./features";
import { formatAge } from "./format";
import { Palette, type PaletteCommand } from "./palette";
import { mountReviewPanel } from "./review-panel";
import { Sidebar } from "./sidebar";
import { Store, type Change } from "./store";
import { StreamClient } from "./stream";
import { ThemeController } from "./theme";
import { openSheet } from "./sheet";
import { toast } from "./toast";
import type { CommandName, Commands, ModelCatalog, SiblingInstance } from "./wire";

const root = document.getElementById("app")!;
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
        <main class="chat">
          <header class="chat-header">
            <button type="button" class="icon-button menu-button" aria-label="Open sessions">${glyph("sidebar")}</button>
            <div class="chat-title"><h1>Tact</h1><div class="chat-sub"><span class="model-dot"></span><span class="chat-model"></span><span class="chat-effort"></span></div></div>
            <div class="header-actions">
              <button type="button" class="palette-button" aria-label="Command palette">${glyph("search")}<span>Search</span><kbd>${isMac ? "⌘K" : "Ctrl K"}</kbd></button>
              <button type="button" class="icon-button panel-button" aria-label="Changes and overview" aria-pressed="false" title="Changes">${glyph("panel")}</button>
            </div>
          </header>
          <div class="connection-banner" role="status" hidden></div>
          <div class="transcript-scroller"></div>
          <button type="button" class="jump-latest" hidden>${glyph("arrow-down")}Latest</button>
          <div class="dock"></div>
        </main>
        <div class="panel-resizer" role="separator" aria-orientation="vertical" aria-label="Resize panel" tabindex="0"></div>
        <section class="side-panel" aria-label="Changes and overview">
          <header class="panel-head"><strong>Changes</strong><button type="button" class="icon-button panel-close" aria-label="Close panel">${glyph("close")}</button></header>
          <div class="panel-body"></div>
        </section>
      </div>`;
    this.shell = root.querySelector(".app")!;
    this.layout = loadLayout(localStorage, innerWidth);
    this.transcript = new Transcript(
      root.querySelector(".transcript-scroller")!,
      root.querySelector(".jump-latest")!,
      () => this.theme.current,
    );
    this.composer = new Composer(root.querySelector(".dock")!, {
      api,
      catalog: () => this.catalog,
      running: () => this.activeRunning(),
      openRecentPrompts: () => this.openRecentPrompts(),
      openContext: () => this.withSession((id) => void openContextDiagnostics(api, id)),
    });
    this.sidebar = new Sidebar(root.querySelector(".sidebar")!, {
      api,
      catalog: () => this.catalog,
      navigated: () => this.dispatch({ type: "toggle-drawer", open: false }),
      toggleTheme: () => this.theme.cycle(),
      themeChoice: () => this.theme.choice,
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
          this.composer.show(state.session);
          this.renderHeader();
          if ((state.session?.id ?? null) !== this.shownSession) {
            this.shownSession = state.session?.id ?? null;
            for (const listener of this.reviewListeners.active) listener();
          }
          break;
        case "entry":
          this.transcript.entryChanged(change.id);
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
    effort.textContent = session
      ? [session.effort, session.reasoningMode === "pro" ? "pro" : "", session.speed === "standard" ? "" : session.speed].filter(Boolean).join(" · ")
      : "";
    effort.style.color = session ? effortColor(session.effort) : "";
    root.querySelector<HTMLElement>(".chat-sub .model-dot")!.style.background = session ? modelColor(session.model) : "transparent";
    const running = this.activeRunning();
    this.shell.classList.toggle("running", running);
    const unread = this.store.state.live.filter((summary) => summary.unread).length;
    document.title = `${running ? "● " : unread ? `(${unread}) ` : ""}${title} · Tact`;
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
    const panelChanged = next.panelOpen !== this.layout.panelOpen;
    this.layout = next;
    saveLayout(localStorage, next);
    this.applyLayout();
    if (panelChanged && next.panelOpen) this.ensureReview();
    this.review?.setVisible?.(next.panelOpen);
  }

  private applyLayout() {
    const { viewport, drawerOpen, panelOpen, panelWidth } = this.layout;
    this.shell.dataset.viewport = viewport;
    this.shell.classList.toggle("drawer-open", drawerOpen);
    this.shell.classList.toggle("panel-open", panelOpen);
    this.shell.style.setProperty("--panel-width", `${panelWidth}px`);
    root.querySelector(".panel-button")!.setAttribute("aria-pressed", String(panelOpen));
    const sidebar = root.querySelector<HTMLElement>(".sidebar")!;
    const overlaySidebar = viewport !== "desktop";
    sidebar.toggleAttribute("inert", overlaySidebar && !drawerOpen);
    const panel = root.querySelector<HTMLElement>(".side-panel")!;
    panel.toggleAttribute("inert", !panelOpen);
    if (panelOpen && viewport === "desktop") this.ensureReview();
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
        if (this.layout.viewport !== "desktop") this.dispatch({ type: "toggle-panel", open: false });
      },
      theme: () => this.theme.current,
      onThemeChange: subscribe(this.reviewListeners.theme),
    });
    this.review.setVisible?.(this.layout.panelOpen);
  }

  private bindShell() {
    root.querySelector(".menu-button")!.addEventListener("click", () => this.dispatch({ type: "toggle-drawer" }));
    root.querySelector(".drawer-close")!.addEventListener("click", () => this.dispatch({ type: "toggle-drawer", open: false }));
    root.querySelector(".scrim")!.addEventListener("click", () => this.dispatch({ type: "escape" }));
    root.querySelector(".panel-button")!.addEventListener("click", () => this.dispatch({ type: "toggle-panel" }));
    root.querySelector(".panel-close")!.addEventListener("click", () => this.dispatch({ type: "toggle-panel", open: false }));
    root.querySelector(".palette-button")!.addEventListener("click", () => this.palette.open());
    addEventListener("resize", () => this.dispatch({ type: "resize", width: innerWidth }), { passive: true });
    document.addEventListener("keydown", (event) => {
      const mod = isMac ? event.metaKey : event.ctrlKey;
      if (mod && event.key.toLowerCase() === "k") {
        event.preventDefault();
        if (this.palette.isOpen) this.palette.close();
        else this.palette.open();
      } else if (event.key === "Escape" && !event.defaultPrevented && !this.palette.isOpen) {
        this.dispatch({ type: "escape" });
      } else if (event.key === "/" && !isEditable(event.target)) {
        event.preventDefault();
        this.composer.focus();
      } else if (event.key === "?" && !isEditable(event.target)) {
        event.preventDefault();
        showShortcuts();
      }
    });
    this.bindResizer(root.querySelector(".panel-resizer")!);
  }

  private bindResizer(handle: HTMLElement) {
    handle.addEventListener("pointerdown", (event) => {
      event.preventDefault();
      handle.setPointerCapture(event.pointerId);
      this.shell.classList.add("resizing");
      const move = (moveEvent: PointerEvent) => {
        this.dispatch({ type: "panel-width", width: innerWidth - moveEvent.clientX, windowWidth: innerWidth });
      };
      const up = () => {
        this.shell.classList.remove("resizing");
        handle.removeEventListener("pointermove", move);
        handle.removeEventListener("pointerup", up);
        handle.removeEventListener("pointercancel", up);
      };
      handle.addEventListener("pointermove", move);
      handle.addEventListener("pointerup", up);
      handle.addEventListener("pointercancel", up);
    });
    handle.addEventListener("keydown", (event) => {
      if (event.key !== "ArrowLeft" && event.key !== "ArrowRight") return;
      event.preventDefault();
      const step = event.shiftKey ? 64 : 16;
      const width = this.layout.panelWidth + (event.key === "ArrowLeft" ? step : -step);
      this.dispatch({ type: "panel-width", width, windowWidth: innerWidth });
    });
  }

  private sessionSource(id: string) {
    return {
      key: id,
      data: this.store.state.session!,
      detail: (entry: number) => this.api.toolDetail(id, entry),
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
      const commands: PaletteCommand[] = [];
      if (running) commands.push({ id: "stop", title: "Stop", group: "Session", icon: "stop", hint: "esc", keywords: "interrupt cancel", run: () => this.command("interrupt", { session: id }) });
      commands.push(
        { id: "recent", title: "Recent prompts…", group: "Session", icon: "history", hint: "↑", run: () => this.openRecentPrompts() },
        { id: "compact", title: "Compact context", group: "Session", icon: "compact", run: () => this.command("compact", { session: id }) },
        { id: "reflect", title: "Reflect…", group: "Session", icon: "reflect", keywords: "reflection learn", run: () => openReflect(this.api, id) },
        { id: "handoff", title: "Prepare handoff", group: "Session", icon: "handoff", run: () => this.command("handoff", { session: id }) },
        { id: "subagents", title: "Subagents", group: "Session", icon: "agents", hint: String(session.subagents.agents.length || ""), keywords: "agents tree", run: () => this.openSubagents() },
        { id: "context", title: "Context diagnostics", group: "Session", icon: "gauge", keywords: "tokens debug", run: () => void openContextDiagnostics(this.api, id) },
        { id: "attach", title: "Attach image…", group: "Session", icon: "image", run: () => root.querySelector<HTMLButtonElement>(".attach-chip")?.click() },
        { id: "fork", title: "Fork session", group: "Session", icon: "fork", run: () => this.command("open_session", { fork: { session: id } }) },
        { id: "close", title: "Close session", group: "Session", icon: "trash", run: () => {
          if (running && !confirm("A turn is running. Stop it and close the session?")) return;
          this.command("close_session", { session: id, force: running });
        } },
      );
      if (!started) {
        commands.push(...(this.catalog?.models ?? []).map((candidate): PaletteCommand => ({
          id: `model:${candidate.id}`, title: `Model: ${candidate.label}`, group: "Settings", icon: "sparkles",
          hint: candidate.id === session.model ? "current" : "",
          run: () => this.command("set_model", { session: id, model: candidate.id }),
        })));
      }
      if (!(started && model?.effort_fixed_after_start)) {
        commands.push(...(this.catalog?.efforts ?? []).map((effort): PaletteCommand => ({
          id: `effort:${effort}`, title: `Effort: ${effort}`, group: "Settings", icon: "brain",
          hint: effort === session.effort ? "current" : "",
          run: () => this.command("set_effort", { session: id, effort }),
        })));
      }
      if (model?.reasoning_modes.includes("pro")) {
        const pro = session.reasoningMode === "pro";
        commands.push({ id: "mode", title: pro ? "Turn pro reasoning off" : "Turn pro reasoning on", group: "Settings", icon: "sparkles",
          run: () => this.command("set_reasoning_mode", { session: id, mode: pro ? "standard" : "pro" }) });
      }
      commands.push(...(this.catalog?.speeds ?? []).map((speed): PaletteCommand => ({
        id: `speed:${speed}`, title: `Speed: ${speed}`, group: "Settings", icon: "bolt",
        hint: speed === session.speed ? "current" : "",
        run: () => this.command("set_speed", { session: id, speed }),
      })));
      return commands;
    });
    palette.register(() => [
      { id: "memory", title: "Memory", group: "Tact", icon: "database", keywords: "memories", run: () => void openMemories(this.api) },
      { id: "config", title: "Edit configuration", group: "Tact", icon: "settings", keywords: "config settings", run: () => void openConfigEditor(this.api) },
      { id: "reload", title: "Reload configuration", group: "Tact", icon: "refresh", keywords: "config", run: () => {
        void this.api.command("reload_config").then(() => toast("Configuration reloaded."), (error) => toast(describeError(error), "danger"));
      } },
      { id: "panel", title: this.layout.panelOpen ? "Hide changes" : "Show changes", group: "View", icon: "panel", keywords: "diff review overview", run: () => this.dispatch({ type: "toggle-panel" }) },
      { id: "composer", title: "Focus composer", group: "View", icon: "pencil", hint: "/", run: () => this.composer.focus() },
      { id: "shortcuts", title: "Keyboard shortcuts", group: "View", icon: "keyboard", hint: "?", run: () => showShortcuts() },
      ...(["system", "light", "dark"] as const).map((choice): PaletteCommand => ({
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
