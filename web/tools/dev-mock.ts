// A self-contained imitation of a running Tact for UI development: sessions that stream realistic
// turns with tool rows and subagents, the shared draft with origin echoes and images, queue,
// every query, history, instances, and the review endpoints.
// It follows docs/web.md; the dev server (dev.ts) only adapts it to HTTP.

import type {
  AgentMessage,
  Checkout,
  CommandName,
  Commands,
  ContextBudget,
  Draft,
  Effort,
  ListedMemory,
  ModelCatalog,
  PatchStats,
  PersistedSession,
  Queries,
  QueryName,
  QueuedPrompt,
  ReasoningMode,
  Significance,
  Speed,
  Subagent,
  SubagentRoster,
  SessionSnapshot,
  SessionSummary,
  StreamEventName,
  StreamEvents,
  ToolDetail,
  ToolOutcome,
  ToolState,
  TransientStatus,
  WireEntry,
  Workspaces,
} from "../src/core/wire";
import { parseApplyPatch, patchStats } from "../src/chat/patch";

type EntryBody = WireEntry extends infer Entry
  ? Entry extends WireEntry ? Omit<Entry, "id" | "revision" | "parent"> : never
  : never;

type ToolBody = Extract<EntryBody, { kind: "tool" }>;

/** Tools whose calls are landmarks whatever their outcome. */
const LANDMARK_TOOLS = new Set(["apply_patch", "update_plan", "spawn_agent", "send_agent_message", "close_agent", "interrupt_agent"]);

/**
 * The class of a call without Code Mode children. It mirrors `Significance::is_landmark` in
 * bin/tact/src/web/wire.rs and must change with it; memory writes are told apart by the operation
 * that starts their summary, since the mock's rows carry no arguments.
 */
function significanceOf(name: string, state: ToolState, summary: string): Significance {
  const landmark = state === "failed" || LANDMARK_TOOLS.has(name) || (name === "memory" && /^(store|replace|delete)\b/.test(summary));
  return landmark ? "landmark" : "routine";
}

/** A finished tool row; `fields` override the defaults. */
function tool(name: string, summary: string, fields: Partial<ToolBody> = {}): ToolBody {
  return {
    kind: "tool", name, summary, state: "succeeded", duration_ns: 4_000_000, substeps: [], child_count: 0,
    has_detail: false, outcome: null, stats: null, significance: significanceOf(name, fields.state ?? "succeeded", summary), ...fields,
  };
}

/** A command's outcome as the server derives it: the last output lines and a test summary. */
export function outcomeOf(result: unknown): ToolOutcome | null {
  const { exit_code, output } = (result ?? {}) as { exit_code?: number; output?: unknown };
  if (typeof output !== "string") return null;
  const tail = output.split("\n").map((line) => line.trimEnd()).filter(Boolean).slice(-8).map((line) => line.slice(0, 200));
  const counts = /(\d+) passed(?:[,;]\s*(\d+) failed)?/.exec(output);
  const summary = counts ? (counts[2] && counts[2] !== "0" ? `${counts[1]} passed, ${counts[2]} failed` : `${counts[1]} passed`) : null;
  return { exit_code: exit_code ?? null, tail, summary };
}

function statsOf(envelope: string): PatchStats {
  const files = parseApplyPatch(envelope);
  return { files: files.length, ...patchStats(files) };
}

export class MockRefusal extends Error {
  constructor(readonly code: string, message: string, readonly status = 409) {
    super(message);
  }
}

export type MockListener = <Name extends StreamEventName>(name: Name, data: StreamEvents[Name]) => void;

const codex = { provider: "openai" as const, reasoning_modes: ["standard", "pro"] as ReasoningMode[], effort_fixed_after_start: false };
const claude = { provider: "anthropic" as const, reasoning_modes: ["standard"] as ReasoningMode[], effort_fixed_after_start: true };
export const MOCK_CATALOG: ModelCatalog = {
  models: [
    { id: "sol", label: "Sol", ...codex, effective_speeds: ["standard", "fast", "ultrafast"] },
    { id: "luna", label: "Luna", ...codex, effective_speeds: ["standard", "fast", "fast"] },
    { id: "astra", label: "Astra", ...codex, effective_speeds: ["standard", "fast", "ultrafast"] },
    { id: "haiku-5.5", label: "Haiku 5.5", ...claude, effective_speeds: ["standard", "standard", "standard"] },
    { id: "opus-5.5", label: "Opus 5.5", ...claude, effective_speeds: ["standard", "fast", "fast"] },
    { id: "sonnet-5.5", label: "Sonnet 5.5", ...claude, effective_speeds: ["standard", "standard", "standard"] },
    { id: "fable-5.1", label: "Fable 5.1", ...claude, effective_speeds: ["standard", "standard", "standard"] },
  ],
  efforts: ["low", "medium", "high", "xhigh", "max"],
  speeds: ["standard", "fast", "ultrafast"],
};
const WINDOW_TOKENS = 272_000;
const MAX_LIVE = 8;

export const MOCK_DEFAULT_WORKSPACE = "/Users/dev/src/tact";
const WS2 = "/Users/dev/src/tact-ws2";
type MockCheckout = Omit<Checkout, "current" | "touched">;
/** The default workspace's repository: the main checkout, a git worktree, and a jj workspace. */
export const MOCK_CHECKOUTS: MockCheckout[] = [
  { path: MOCK_DEFAULT_WORKSPACE, name: "tact", label: "main", kind: "git", head: "9d3b7456e1", changed_files: 2, missing: false },
  { path: WS2, name: "tact-ws2", label: "ws2", kind: "git", head: "4be0c2d913", changed_files: 4, missing: false },
  { path: "/Users/dev/src/review-ui", name: "review-ui", label: "review-ui", kind: "jj", head: "77a1f0c5e2", changed_files: 0, missing: false },
];
const RECENT_WORKSPACES = [WS2, "/Users/dev/src/commonware", "/Users/dev/notes"];
/** How the synthetic review diff is relabelled for checkouts other than the main one. */
const CHECKOUT_FILES: Record<string, [string, string][]> = {
  [WS2]: [
    ["src/review/mod.rs", "src/web/checkout.rs"],
    ["src/tui/components/actions.rs", "src/web/diff.rs"],
    [".github/workflows/release.yml", "docs/workspaces.md"],
  ],
};

/**
 * A review page as `checkout` would show it: the main checkout's diff unchanged, the worktree's
 * with other files, and a checkout without changes empty.
 */
export function mockCheckoutPage<Page extends { patch: string; repository: string }>(page: Page, checkout: Checkout): Page {
  if (checkout.path === MOCK_DEFAULT_WORKSPACE) return page;
  const renames = CHECKOUT_FILES[checkout.path];
  const patch = renames ? renames.reduce((text, [from, to]) => text.replaceAll(from, to), page.patch) : "";
  return { ...page, repository: checkout.name, patch };
}

class MockSession {
  title: string;
  entries: WireEntry[] = [];
  details = new Map<number, ToolDetail>();
  status: TransientStatus | null = null;
  queue: QueuedPrompt[] = [];
  draft: Draft = { rev: 0, text: "", images: [] };
  reasoningMode: ReasoningMode = "standard";
  speed: Speed = "standard";
  context: ContextBudget = { active_tokens: 18_400, window_tokens: WINDOW_TOKENS };
  subagents: SubagentRoster = { max_subagents: 4, agents: [] };
  agentEntries = new Map<number, WireEntry[]>();
  nextImage = 1;
  unread = false;
  failed = false;
  lastActivity = Date.now();
  turn: AbortController | null = null;
  private nextEntry = 1;

  constructor(
    readonly id: string,
    public model: string,
    public effort: Effort = "medium",
    title = "New chat",
    readonly workspace = MOCK_DEFAULT_WORKSPACE,
  ) {
    this.title = title;
  }

  get running() {
    return this.turn !== null;
  }

  summary(): SessionSummary {
    return {
      id: this.id,
      title: this.title,
      model: this.model,
      state: this.running ? "running" : this.failed ? "error" : "idle",
      unread: this.unread,
      has_draft: this.draft.text.trim().length > 0,
      last_activity_unix_ms: this.lastActivity,
      workspace: this.workspace,
    };
  }

  snapshot(): SessionSnapshot {
    return {
      session: this.id,
      title: this.title,
      model: this.model,
      effort: this.effort,
      reasoning_mode: this.reasoningMode,
      speed: this.speed,
      entries: this.entries.map((entry) => ({ ...entry })),
      status: this.status,
      queue: this.queue.map((item) => ({ ...item })),
      draft: structuredClone(this.draft),
      running: this.running,
      context: { ...this.context },
      subagents: structuredClone(this.subagents),
    };
  }

  push(body: EntryBody, parent: number | null = null, at = Date.now()): WireEntry {
    const entry = { ...body, id: this.nextEntry++, revision: 1, parent, at_ms: at } as WireEntry;
    this.entries.push(entry);
    this.lastActivity = Date.now();
    return entry;
  }
}

/** Resolves after `ms`, or rejects when the turn is interrupted. */
function sleep(ms: number, signal: AbortSignal) {
  return new Promise<void>((resolve, reject) => {
    if (signal.aborted) return reject(signal.reason);
    const timer = setTimeout(resolve, ms);
    signal.addEventListener("abort", () => {
      clearTimeout(timer);
      reject(signal.reason);
    }, { once: true });
  });
}

const words = (text: string) => text.match(/\S+\s*/g) ?? [];

export class MockTact {
  readonly sessions = new Map<string, MockSession>();
  config = {
    path: "/Users/dev/.tact/config.toml",
    text: ["# Tact configuration", 'model = "sol"', 'effort = "high"', "", "[web]", "enabled = true", "port = 7878", ""].join("\n"),
    revision: "1",
  };
  memories: ListedMemory[] = memorySeed();
  active: string | null = null;
  workspaceVersion = "1";
  /** Multiplies every scripted delay; tests set it to 0. */
  pace = 1;
  private listeners = new Set<MockListener>();
  private nextSession = 1;
  private nextQueue = 1;

  constructor(seed = true) {
    if (seed) this.seed();
  }

  subscribe(listener: MockListener) {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  /** The events a fresh stream receives: hello, live, active, and the active session's snapshot. */
  greeting(): { name: StreamEventName; data: unknown }[] {
    const events: { name: StreamEventName; data: unknown }[] = [
      { name: "hello", data: { protocol_version: 9, client_hint: "tact-dev" } },
      { name: "live", data: this.live() },
    ];
    const session = this.active ? this.sessions.get(this.active) : undefined;
    if (session) {
      events.push({ name: "active", data: { session: session.id } });
      events.push({ name: "snapshot", data: session.snapshot() });
    }
    return events;
  }

  live(): StreamEvents["live"] {
    return {
      active: this.active,
      sessions: [...this.sessions.values()].map((session) => session.summary()),
    };
  }

  instance() {
    const sessions = [...this.sessions.values()];
    return {
      protocol_version: 9,
      workspace: "/Users/dev/src/tact",
      repository: "tact",
      live: sessions.length,
      running: sessions.filter((session) => session.running).length,
    };
  }

  toolDetail(sessionId: string, entry: number): ToolDetail {
    const detail = this.session(sessionId).details.get(entry);
    if (!detail) throw new MockRefusal("invalid_request", "That entry has no detail.", 404);
    return detail;
  }

  /** Announces that files changed in `checkout`, by default the default workspace. */
  touchWorkspace(checkout = MOCK_DEFAULT_WORKSPACE) {
    this.workspaceVersion = String(Number(this.workspaceVersion) + 1);
    this.emit("workspace", { version: this.workspaceVersion, checkout });
  }

  /**
   * The checkouts of `sessionId`'s repository. A session outside the default workspace's
   * repository has a family of one. While a session runs, its agent is working in the git worktree.
   */
  workspaces(sessionId?: string): Workspaces {
    const session = sessionId === undefined ? undefined : this.session(sessionId);
    const workspace = session?.workspace ?? MOCK_DEFAULT_WORKSPACE;
    const family: MockCheckout[] = MOCK_CHECKOUTS.some((checkout) => checkout.path === workspace)
      ? MOCK_CHECKOUTS
      : [{ path: workspace, name: workspace.split("/").pop()!, label: "main", kind: "git", head: null, changed_files: 0, missing: false }];
    return {
      default: MOCK_DEFAULT_WORKSPACE,
      checkouts: family.map((checkout) => ({
        ...checkout,
        current: checkout.path === workspace,
        touched: (session?.running ?? false) && checkout.path === WS2,
      })),
      recent: RECENT_WORKSPACES.filter((path) => path !== workspace),
    };
  }

  /** The checkout a review request addresses, refusing paths outside the session's repository. */
  reviewCheckout(sessionId: string | undefined, path: string | undefined): Checkout {
    const { checkouts } = this.workspaces(sessionId);
    const checkout = path === undefined
      ? checkouts.find((candidate) => candidate.current)
      : checkouts.find((candidate) => candidate.path === path);
    if (!checkout) throw new MockRefusal("unknown_checkout", "That checkout is not part of the session's repository.", 404);
    return checkout;
  }

  /** Simulates typing in the terminal's composer. */
  terminalDraft(text: string) {
    if (!this.active) return;
    this.setDraft(this.session(this.active), text, "terminal");
  }

  command(name: CommandName, args: unknown, client: number): unknown {
    const origin = `web:${client}`;
    const body = args as never;
    switch (name) {
      case "set_draft": {
        const { session, text } = body as Commands["set_draft"];
        this.setDraft(this.session(session), text, origin);
        return {};
      }
      case "submit": {
        const { session: id, rev, queue } = body as Commands["submit"];
        const session = this.session(id);
        if (session.draft.rev !== rev) {
          throw new MockRefusal("draft_changed", "The draft changed before it was sent.");
        }
        const text = session.draft.text.trim();
        if (!text) return {};
        if (/^\/(copy|editor|quit)\b/.test(text)) {
          throw new MockRefusal("not_available_remotely", `${text.split(/\s/)[0]} only works in the terminal.`);
        }
        this.setDraft(session, "", origin);
        if (session.running) {
          session.queue.push({ id: this.nextQueue++, text, steering: !queue });
          this.emit("queue", { session: session.id, items: session.queue });
        } else {
          void this.runTurn(session, text);
        }
        return {};
      }
      case "interrupt": {
        const session = this.session((body as Commands["interrupt"]).session);
        if (!session.turn) throw new MockRefusal("nothing_running", "Nothing is running.");
        session.turn.abort(new Error("interrupted"));
        return {};
      }
      case "steer":
      case "dequeue": {
        const { session: id, queue_id } = body as Commands["steer"];
        const session = this.session(id);
        const item = session.queue.find((candidate) => candidate.id === queue_id);
        if (!item) throw new MockRefusal("unknown_session", "That prompt was already sent.");
        if (name === "steer") {
          if (!session.running) throw new MockRefusal("nothing_running", "Nothing is running.");
          item.steering = true;
        } else {
          session.queue = session.queue.filter((candidate) => candidate !== item);
        }
        this.emit("queue", { session: id, items: session.queue });
        return {};
      }
      case "compact": {
        const session = this.session((body as Commands["compact"]).session);
        if (session.running) throw new MockRefusal("turn_running", "Wait for the turn to finish.");
        if (session.queue.length) throw new MockRefusal("queue_not_empty", "The queue is not empty.");
        void this.compact(session);
        return {};
      }
      case "set_model":
      case "set_effort":
      case "set_reasoning_mode":
      case "set_speed": {
        const update = body as { session: string; model?: string; effort?: Effort; mode?: ReasoningMode; speed?: Speed };
        const session = this.session(update.session);
        const started = session.entries.length > 0;
        if (update.model !== undefined) {
          if (started) throw new MockRefusal("invalid_request", "The model is fixed after the first turn.", 400);
          session.model = update.model;
        }
        if (update.effort !== undefined) {
          const fixed = MOCK_CATALOG.models.find((model) => model.id === session.model)?.effort_fixed_after_start;
          if (started && fixed) throw new MockRefusal("invalid_request", "This model's effort is fixed after the first turn.", 400);
          session.effort = update.effort;
          this.append(session, { kind: "effort_changed", to: update.effort });
        }
        if (update.mode !== undefined) session.reasoningMode = update.mode;
        if (update.speed !== undefined) session.speed = update.speed;
        this.emit("settings", {
          session: session.id, model: session.model, effort: session.effort,
          reasoning_mode: session.reasoningMode, speed: session.speed,
        });
        this.publishLive();
        return {};
      }
      case "edit_queued": {
        const { session: id, queue_id, text } = body as Commands["edit_queued"];
        const session = this.session(id);
        const item = session.queue.find((candidate) => candidate.id === queue_id);
        if (!item) throw new MockRefusal("unknown_session", "That prompt was already sent.");
        item.text = text;
        this.emit("queue", { session: id, items: session.queue });
        return {};
      }
      case "attach_image": {
        const { session: id, data_url } = body as Commands["attach_image"];
        if (!data_url.startsWith("data:image/")) throw new MockRefusal("invalid_request", "Only images can be attached.", 400);
        const session = this.session(id);
        const marker = `[Image #${session.nextImage++}]`;
        const text = session.draft.text ? `${session.draft.text.replace(/\s*$/, "")} ${marker}` : marker;
        session.draft.images.push({ marker, data_url });
        this.setDraft(session, text, origin);
        return {};
      }
      case "reflect":
      case "handoff": {
        const session = this.session((body as Commands["handoff"]).session);
        if (session.running) throw new MockRefusal("turn_running", "Wait for the turn to finish.");
        if (session.queue.length) throw new MockRefusal("queue_not_empty", "The queue is not empty.");
        if (name === "reflect") this.append(session, { kind: "reflection_started" });
        void this.runTurn(session, name === "reflect" ? "Reflect on this session." : "Prepare a handoff for the next session.", false);
        return {};
      }
      case "reload_config":
        return {};
      case "write_config": {
        const { text, revision } = body as Commands["write_config"];
        if (revision !== this.config.revision) throw new MockRefusal("stale", "The configuration changed since it was read.");
        if (/^\s*\[[^\]]*$/m.test(text)) throw new MockRefusal("invalid_request", "TOML parse error: unclosed table header.", 400);
        this.config = { ...this.config, text, revision: String(Number(this.config.revision) + 1) };
        return {};
      }
      case "delete_memory": {
        const { key } = body as Commands["delete_memory"];
        const record = this.memories.find((candidate) => candidate.key.id === key.id);
        if (!record?.deletable) throw new MockRefusal("not_available_remotely", "You cannot delete that memory.");
        if (record.key.version !== key.version) throw new MockRefusal("stale", "That memory changed.");
        this.memories = this.memories.filter((candidate) => candidate !== record);
        return {};
      }
      case "set_max_subagents": {
        const { limit } = body as Commands["set_max_subagents"];
        for (const session of this.sessions.values()) {
          session.subagents.max_subagents = limit;
          this.emit("subagents", { session: session.id, ...session.subagents });
        }
        return {};
      }
      case "activate": {
        this.activate(this.session((body as Commands["activate"]).session).id);
        return {};
      }
      case "open_session": {
        const spec = body as Commands["open_session"];
        let session: MockSession;
        if ("resume" in spec) {
          const existing = this.sessions.get(spec.resume.session);
          if (existing) {
            this.activate(existing.id);
            return { session: existing.id };
          }
          const record = history.find((candidate) => candidate.session_id === spec.resume.session);
          if (!record) throw new MockRefusal("unknown_session", "No such session.", 404);
          this.ensureCapacity();
          session = new MockSession(record.session_id, record.model, record.effort, record.preview, record.workspace);
          session.push({ kind: "user", text: record.preview });
          session.push({ kind: "assistant", text: "Resumed from history.", complete: true, commentary: false });
        } else if ("fork" in spec) {
          const source = this.session(spec.fork.session);
          if (source.running) throw new MockRefusal("turn_running", "Wait for the turn to finish before forking.");
          this.ensureCapacity();
          session = new MockSession(this.newId(), source.model, source.effort, `${source.title} (fork)`, source.workspace);
          for (const entry of source.entries) session.push({ ...entry });
          session.push({ kind: "forked_from", session: source.id });
        } else {
          this.ensureCapacity();
          const workspace = spec.new.workspace ?? MOCK_DEFAULT_WORKSPACE;
          if (!MOCK_CHECKOUTS.some((checkout) => checkout.path === workspace) && !RECENT_WORKSPACES.includes(workspace)) {
            throw new MockRefusal("invalid_request", `${workspace} is not a known workspace.`, 400);
          }
          session = new MockSession(this.newId(), spec.new.model ?? "sol", "medium", "New chat", workspace);
        }
        this.sessions.set(session.id, session);
        this.activate(session.id);
        return { session: session.id };
      }
      case "close_session": {
        const { session: id, force } = body as Commands["close_session"];
        const session = this.session(id);
        if (session.running && !force) throw new MockRefusal("turn_running", "A turn is running.");
        session.turn?.abort(new Error("closed"));
        this.sessions.delete(id);
        this.emit("closed", { session: id });
        if (this.active === id) {
          const next = [...this.sessions.values()].sort((a, b) => b.lastActivity - a.lastActivity)[0];
          this.active = null;
          if (next) this.activate(next.id);
          else this.publishLive();
        } else {
          this.publishLive();
        }
        return {};
      }
    }
    throw new MockRefusal("invalid_request", `Unknown command ${name}.`, 400);
  }

  query<Name extends QueryName>(name: Name, args: unknown): Queries[Name]["reply"] {
    const query = String((args as { query?: string } | undefined)?.query ?? "").toLowerCase();
    const reply = (): unknown => {
      switch (name) {
        case "models":
          return MOCK_CATALOG;
        case "history": {
          const cursor = (args as { cursor?: string | null } | undefined)?.cursor;
          const matches = history.filter((session) => `${session.preview} ${session.session_id}`.toLowerCase().includes(query));
          const start = cursor ? Number(cursor) : 0;
          return { sessions: matches.slice(start, start + 12), next_cursor: start + 12 < matches.length ? String(start + 12) : null };
        }
        case "files":
          return { paths: files.filter((path) => fuzzy(query, path)).slice(0, 50) };
        case "skills":
          return { skills: skills.filter((skill) => fuzzy(query, skill.name)) };
        case "recent_prompts":
          return { prompts: recentPrompts.filter((prompt) => prompt.text.toLowerCase().includes(query)) };
        case "context_diagnostics": {
          const session = this.session((args as { session: string }).session);
          return {
            model_window_tokens: WINDOW_TOKENS, auto_compact_token_limit: 244_800, active_tokens: session.context.active_tokens,
            usage: { input: session.context.active_tokens, cached_input: Math.round(session.context.active_tokens * .8), uncached_input: Math.round(session.context.active_tokens * .2), output: 1_840, total: session.context.active_tokens + 1_840 },
            continuation: "previous_response", prompt_cache: true, compactions_started: 1, compactions_completed: 1,
            last_compaction: { trigger: "automatic", started_at_unix_ms: Date.now() - 3_600_000, completed_at_unix_ms: Date.now() - 3_597_000, before_tokens: 241_000, after_tokens: 31_200 },
          };
        }
        case "memories":
          return { access: { source: "local", namespace: null, role: "writer" }, records: this.memories };
        case "config":
          return this.config;
        case "workspaces":
          return this.workspaces((args as Queries["workspaces"]["args"] | undefined)?.session);
      }
      throw new MockRefusal("invalid_request", `Unknown query ${name}.`, 400);
    };
    return structuredClone(reply()) as Queries[Name]["reply"];
  }

  agentTranscript(sessionId: string, agent: number) {
    return { entries: this.session(sessionId).agentEntries.get(agent) ?? [] };
  }

  /** Starts the background session that keeps working, so the sidebar always shows activity. */
  startAmbientWork() {
    this.startAgentActivity();
    const worker = [...this.sessions.values()].find((session) => session.title.startsWith("Benchmark"));
    if (!worker) return;
    const loop = async () => {
      while (this.sessions.has(worker.id)) {
        if (!worker.running) await this.runTurn(worker, "Re-run the journal benchmark and report regressions.");
        await new Promise((resolve) => setTimeout(resolve, 12_000 * this.pace));
      }
    };
    void loop();
  }

  /** The seeded session's running agents keep reporting activity, so their status lines move. */
  private startAgentActivity() {
    const steps: [string, string][] = [
      ["read", "bin/tact/src/web/wire.rs"], ["grep", "protocol_version"], ["read", "web/src/core/wire.ts:120-180"],
      ["exec_command", "rg -n subagent_entry docs/web.md"], ["read", "bin/tact/src/web/bridge.rs:300-420"], ["grep", "WireMessage"],
    ];
    let step = 0;
    setInterval(() => {
      const session = [...this.sessions.values()].find((candidate) => candidate.subagents.agents.some((agent) => agent.id === 3));
      if (!session) return;
      for (const agent of session.subagents.agents.filter((candidate) => candidate.status.state === "running")) {
        const entries = session.agentEntries.get(agent.id);
        if (!entries) continue;
        const [name, summary] = steps[(step + agent.id) % steps.length]!;
        const entry = { ...tool(name, summary), id: entries.length + 1, revision: 1, parent: null, at_ms: Date.now() } as WireEntry;
        entries.push(entry);
        this.emit("subagent_entry", { session: session.id, agent: agent.id, entry });
      }
      step += 1;
    }, 3500 * this.pace || 3500);
  }

  private session(id: string) {
    const session = this.sessions.get(id);
    if (!session) throw new MockRefusal("unknown_session", "That session is not live.", 404);
    return session;
  }

  private newId() {
    return `019a${(this.nextSession++).toString(16).padStart(4, "0")}-7c1e-7d55-9b1f-3e2a9c8d4f${String(Math.floor(Math.random() * 90) + 10)}`;
  }

  private ensureCapacity() {
    if (this.sessions.size >= MAX_LIVE) {
      throw new MockRefusal("too_many_sessions", `At most ${MAX_LIVE} sessions can be live.`);
    }
  }

  private activate(id: string) {
    const session = this.session(id);
    session.unread = false;
    this.active = id;
    this.publishLive();
    this.emit("active", { session: id });
    this.emit("snapshot", session.snapshot());
  }

  private setDraft(session: MockSession, text: string, origin: string) {
    const hadDraft = session.draft.text.trim().length > 0;
    const images = session.draft.images.filter((image) => text.includes(image.marker));
    session.draft = { rev: session.draft.rev + 1, text, images };
    this.emit("draft", { session: session.id, rev: session.draft.rev, text, images, origin });
    if (hadDraft !== text.trim().length > 0) this.publishLive();
  }

  private emit<Name extends StreamEventName>(name: Name, data: StreamEvents[Name]) {
    const session = (data as { session?: unknown }).session;
    const scoped = name !== "live" && name !== "workspace" && name !== "hello" && name !== "active"
      && name !== "closed" && name !== "snapshot";
    if (scoped && session !== this.active) return;
    for (const listener of this.listeners) listener(name, structuredClone(data));
  }

  private publishLive() {
    this.emit("live", this.live());
  }

  private append(session: MockSession, body: EntryBody, parent: number | null = null) {
    const entry = session.push(body, parent);
    this.emit("entry", { session: session.id, entry });
    return entry;
  }

  private update(session: MockSession, entry: WireEntry, change: Partial<EntryBody>) {
    Object.assign(entry, change);
    entry.revision += 1;
    session.lastActivity = Date.now();
    this.emit("entry", { session: session.id, entry });
  }

  private setStatus(session: MockSession, status: TransientStatus | null) {
    session.status = status;
    this.emit("status", { session: session.id, status });
  }

  private async stream(session: MockSession, entry: WireEntry, text: string, signal: AbortSignal) {
    let shown = "";
    const pieces = words(text);
    for (let index = 0; index < pieces.length;) {
      const take = 2 + Math.floor(Math.random() * 4);
      shown += pieces.slice(index, index + take).join("");
      index += take;
      this.update(session, entry, { text: shown } as Partial<EntryBody>);
      await sleep(45 * this.pace, signal);
    }
  }

  private async runTurn(session: MockSession, prompt: string, showPrompt = true) {
    const turn = new AbortController();
    session.turn = turn;
    session.failed = false;
    if (session.title === "New chat") session.title = prompt.slice(0, 48);
    const started = Date.now();
    if (showPrompt) this.append(session, { kind: "user", text: prompt });
    this.publishLive();
    const signal = turn.signal;
    const script = scriptFor(prompt);
    let interrupted = false;
    try {
      this.setStatus(session, { kind: "warming" });
      await sleep(500 * this.pace, signal);
      this.setStatus(session, { kind: "thinking" });
      const reasoning = this.append(session, { kind: "reasoning", text: "" });
      await this.stream(session, reasoning, script.reasoning, signal);
      const narration = this.append(session, { kind: "assistant", text: "", complete: false, commentary: false });
      await this.stream(session, narration, script.narration, signal);
      this.update(session, narration, { complete: true } as Partial<EntryBody>);
      for (const tool of script.tools) {
        this.setStatus(session, { kind: "tool", name: tool.name });
        const row = this.append(session, {
          kind: "tool", name: tool.name, summary: tool.summary, state: "running",
          duration_ns: null, elapsed_ns: 0, substeps: [], child_count: 0, has_detail: true,
          significance: significanceOf(tool.name, "running", tool.summary),
        });
        session.details.set(row.id, { arguments: tool.arguments, result: null, metadata: null });
        for (const step of tool.substeps ?? []) {
          await sleep(350 * this.pace, signal);
          const current = row as Extract<WireEntry, { kind: "tool" }>;
          this.update(session, row, { substeps: [...current.substeps, step] } as Partial<EntryBody>);
        }
        const agent = tool.name === "spawn_agent" ? await this.runSubagent(session, signal) : null;
        await sleep(tool.ms * this.pace, signal);
        // The agent also edits the git worktree, which the review offers as a touched checkout.
        if (tool.name === "apply_patch") this.touchWorkspace(WS2);
        this.growContext(session, 2_000 + Math.floor(Math.random() * 6_000));
        const result = agent === null ? tool.result : { agent_id: agent, status: "completed" };
        session.details.set(row.id, { arguments: tool.arguments, result, metadata: { exit_code: tool.failed ? 1 : 0 } });
        this.update(session, row, {
          state: tool.failed ? "failed" : "succeeded",
          duration_ns: tool.ms * 1_000_000,
          outcome: tool.name === "exec_command" ? outcomeOf(tool.result) : null,
          stats: tool.name === "apply_patch" && !tool.failed ? statsOf(String(tool.arguments)) : null,
          significance: significanceOf(tool.name, tool.failed ? "failed" : "succeeded", tool.summary),
        } as Partial<EntryBody>);
      }
      if (script.retry) {
        this.setStatus(session, { kind: "retrying", delay_ns: 2_000_000_000, next_attempt: 2, max_attempts: 5 });
        await sleep(2000 * this.pace, signal);
      }
      this.setStatus(session, { kind: "responding" });
      const answer = this.append(session, { kind: "assistant", text: "", complete: false, commentary: false });
      await this.stream(session, answer, script.answer, signal);
      this.update(session, answer, { complete: true } as Partial<EntryBody>);
    } catch {
      interrupted = true;
    }
    session.turn = null;
    this.setStatus(session, null);
    if (interrupted) {
      if (this.sessions.has(session.id)) this.append(session, { kind: "interrupted", count: 1 });
    } else {
      this.append(session, { kind: "turn_completed", duration_ns: (Date.now() - started) * 1_000_000 });
    }
    if (this.active !== session.id) session.unread = true;
    if (!this.sessions.has(session.id)) return;
    this.touchWorkspace(session.workspace);
    const next = session.queue.shift();
    if (next) {
      this.emit("queue", { session: session.id, items: session.queue });
      void this.runTurn(session, next.text);
    } else {
      this.publishLive();
    }
  }

  private growContext(session: MockSession, tokens: number) {
    session.context.active_tokens = Math.min(WINDOW_TOKENS, session.context.active_tokens + tokens);
    this.emit("context", { session: session.id, ...session.context });
  }

  /** Spawns a subagent whose transcript streams as `subagent_entry` events. */
  private async runSubagent(session: MockSession, signal: AbortSignal): Promise<number> {
    const id = session.subagents.agents.length + 1;
    const agent = {
      id, parent: null, session_id: this.newId(), role: "bridge reviewer", task: "Check that publications never block the loop.",
      model: "sol", thinking: "high" as Effort, reasoning_mode: "standard" as const, status: { state: "running" as const },
    };
    session.subagents.agents.push(agent);
    const entries: WireEntry[] = [];
    session.agentEntries.set(id, entries);
    const publishRoster = () => this.emit("subagents", { session: session.id, ...session.subagents });
    const add = (body: EntryBody) => {
      const entry = { ...body, id: entries.length + 1, revision: 1, parent: null, at_ms: Date.now() } as WireEntry;
      entries.push(entry);
      this.emit("subagent_entry", { session: session.id, agent: id, entry });
      return entry;
    };
    publishRoster();
    try {
      add({ kind: "user", text: agent.task });
      await sleep(400 * this.pace, signal);
      add(tool("read", "bin/tact/src/web/bridge.rs", { duration_ns: 6_000_000 }));
      await sleep(600 * this.pace, signal);
      const answer = add({ kind: "assistant", text: "", complete: false, commentary: false });
      let text = "";
      for (const word of words("Publisher::publish only sends on an unbounded channel and drops the error, so a stopped server cannot block or fail the loop.")) {
        text += word;
        Object.assign(answer, { text });
        answer.revision += 1;
        this.emit("subagent_entry", { session: session.id, agent: id, entry: answer });
        await sleep(40 * this.pace, signal);
      }
      Object.assign(answer, { complete: true });
      answer.revision += 1;
      this.emit("subagent_entry", { session: session.id, agent: id, entry: answer });
      Object.assign(agent, { status: { state: "completed", output: "No blocking path found." } });
    } catch (error) {
      Object.assign(agent, { status: { state: "interrupted" } });
      publishRoster();
      throw error;
    }
    publishRoster();
    return id;
  }

  private async compact(session: MockSession) {
    const turn = new AbortController();
    session.turn = turn;
    this.publishLive();
    this.setStatus(session, { kind: "compacting" });
    try {
      await sleep(2500 * this.pace, turn.signal);
      this.append(session, { kind: "context_compacted", duration_ns: 2_500_000_000 });
      session.context.active_tokens = 24_000;
      this.emit("context", { session: session.id, ...session.context });
    } catch {
      this.append(session, { kind: "interrupted", count: 1 });
    }
    session.turn = null;
    this.setStatus(session, null);
    this.publishLive();
  }

  private seed() {
    const main = new MockSession(this.newId(), "sol", "high", "Wire the web bridge into the TUI loop");
    // Three turns spread over the last few hours: an older one that left a failure, one that
    // explored, failed a test and recovered, and the latest, still waiting on its agents.
    let clock = Date.now() - 5 * 3_600_000 - 20 * 60_000;
    const add = (body: EntryBody, seconds = 20, parent: number | null = null) => main.push(body, parent, clock += seconds * 1000);
    const detail = (entry: WireEntry, value: ToolDetail) => main.details.set(entry.id, value);
    const shell = (cmd: string, exit_code: number, output: string, seconds: number) => {
      const row = add(tool("exec_command", cmd, { state: exit_code === 0 ? "succeeded" : "failed", duration_ns: seconds * 1e9, has_detail: true, outcome: outcomeOf({ exit_code, output }) }), seconds);
      detail(row, { arguments: { cmd }, result: { exit_code, wall_time_seconds: seconds, output }, metadata: null });
      return row;
    };
    const patch = (envelope: string, summary: string) => {
      const row = add(tool("apply_patch", summary, { duration_ns: 31_000_000, has_detail: true, stats: statsOf(envelope) }));
      detail(row, { arguments: envelope, result: { output: `Success. Updated the following files:\nM ${summary}` }, metadata: null });
      return row;
    };
    const BRIDGE_TEST = "cargo nextest run -p tact -E 'test(bridge)'";

    add({ kind: "user", text: "The bridge test is flaky on CI. Find out why and harden it." }, 0);
    add({ kind: "reasoning", text: "A flaky ordering test usually means two tasks race on a channel. Run it first, then read the bridge." }, 6);
    shell(BRIDGE_TEST, 0, "     Summary [  47.902s] 18 tests run: 18 passed, 0 skipped\n", 48);
    shell("sed -n 1,200p bin/tact/src/web/bridge.rs", 0, "//! Connects the web server to the terminal event loop.\n", 1);
    patch(PATCH_FLAKE, "bin/tact/src/web/bridge.rs");
    shell("cargo clippy -p tact -- -D warnings", 101, [
      "    Checking tact v0.7.0 (/Users/dev/src/tact/bin/tact)",
      "error: this `if` has identical blocks",
      "   --> bin/tact/src/web/bridge.rs:212:9",
      "    |",
      "212 |         if retry { self.flush() } else { self.flush() }",
      "    = help: for further information visit https://rust-lang.github.io/rust-clippy/master/index.html#if_same_then_else",
      "error: could not compile `tact` (lib) due to 1 previous error",
    ].join("\n"), 21);
    add({ kind: "assistant", text: "The flake came from the test's own timer: it raced the publisher's first frame. The test now waits for the frame instead of sleeping.\n\nClippy still flags identical branches in `bridge.rs`; I left them for you to decide.", complete: true, commentary: false }, 30);
    add({ kind: "turn_completed", duration_ns: 432_000_000_000 }, 2);

    clock += 70 * 60_000;
    add({ kind: "user", text: "Make draft echoes follow the acknowledgement.\n\n- Keep `Publisher::publish` non-blocking\n- **Don't** touch the keymap" }, 0);
    add({ kind: "reasoning", text: "Find where the echo is published and where the reply is sent." }, 5);
    const exploring: [string, string][] = [
      ["memory", "scan · local · draft echo ordering · 2 candidates"], ["find_sessions", "draft echo"],
      ["read_session", "019a00ff-7c1e-7d55-9b1f-3e2a9c8d4f01"], ["web__run", "tokio mpsc ordering guarantees"],
      ["web__run", "tokio broadcast lagged receivers"], ["view_image", "/tmp/draft-echo-race.png"],
    ];
    for (const [name, summary] of exploring) add(tool(name, summary, { duration_ns: 40_000_000 + summary.length * 1_000_000 }), 4);
    add({ kind: "assistant", text: "The echo is published before the reply is sent, so a second tab sees the draft first. I'll make the reply wait for the loop to apply the command.", complete: true, commentary: false }, 12);
    patch(PATCH_ENVELOPE, "bin/tact/src/tui/app.rs, bin/tact/src/web/bridge.rs");
    shell(BRIDGE_TEST, 101, [
      "        PASS [   0.388s] tact web::bridge::tests::commands_apply_in_order",
      "        FAIL [   0.412s] tact web::bridge::tests::draft_echo_follows_acknowledgement",
      "",
      "thread 'web::bridge::tests::draft_echo_follows_acknowledgement' panicked at bin/tact/src/web/bridge.rs:388:9:",
      "assertion failed: echo arrived before the acknowledgement",
      "     Summary [  48.201s] 18 tests run: 17 passed, 1 failed",
    ].join("\n"), 48);
    add({ kind: "assistant", text: "One test still sees the echo first: the acknowledgement is queued behind the publication. Sending it from the loop fixes that.", complete: true, commentary: false }, 15);
    patch(PATCH_ACK, "bin/tact/src/web/bridge.rs");
    shell(BRIDGE_TEST, 0, "        PASS [   0.401s] tact web::bridge::tests::draft_echo_follows_acknowledgement\n     Summary [  47.630s] 18 tests run: 18 passed, 0 skipped\n", 48);
    add({ kind: "assistant", text: "Draft echoes now follow the acknowledgement. The reply is sent from the loop after the command is applied, and `draft_echo_follows_acknowledgement` covers it.\n\nWith $n$ connected tabs, each command now costs one publication and one reply, so the loop does $O(n)$ work per command instead of $O(n^2)$. A tab sees its echo after\n\n$$\nt_{\\text{echo}} = t_{\\text{apply}} + \\sum_{i=1}^{n} \\delta_i \\le t_{\\text{apply}} + n\\,\\delta_{\\max}\n$$\n\nwhich stays under a frame (16 ms) for $n \\le 8$.", complete: true, commentary: false }, 20);
    add({ kind: "turn_completed", duration_ns: 402_000_000_000 }, 2);

    clock = Date.now() - 42 * 60_000;
    add({ kind: "user", text: "Wire the web bridge into the TUI loop. Publications must never block the loop, and every web command has to go through the same effect as its keypress.\n\n[Image #1]\nThe current flow is above; compare it with [Image #2] before you start.", images: 2 }, 0);
    add({ kind: "reasoning", text: "The loop already owns all session state, so the bridge only needs a non-blocking publisher and a request channel the select loop polls next to terminal input." }, 8);
    shell("sed -n 1,200p bin/tact/src/web/bridge.rs", 0, "//! Connects the web server to the terminal event loop.\n", 1);
    const rg = add(tool("exec_command", "rg -n \"enum Action\" bin/tact/src/tui", { duration_ns: 182_000_000, has_detail: true, outcome: { exit_code: 0, tail: ["bin/tact/src/tui/components/actions.rs:73:pub(super) enum Action {", "bin/tact/src/tui/components/app.rs:112:enum Action {"], summary: null } }), 6);
    detail(rg, {
      arguments: { cmd: 'rg -n "enum Action" bin/tact/src/tui' },
      result: { chunk_id: "a1", exit_code: 0, wall_time_seconds: 0.18, output: "bin/tact/src/tui/components/actions.rs:73:pub(super) enum Action {\nbin/tact/src/tui/components/app.rs:112:enum Action {\n" },
      metadata: null,
    });
    shell("jj st", 0, "Working copy changes:\nM bin/tact/src/tui/app.rs\n", 1);
    shell("cargo check -p tact", 0, "    Checking tact v0.7.0 (/Users/dev/src/tact/bin/tact)\n    Finished `dev` profile [unoptimized + debuginfo] target(s) in 9.84s\n", 10);
    // A Code Mode cell that only reads stays routine and folds with the commands around it.
    const reads = add(tool("exec", "2 tools", { duration_ns: 120_000_000, child_count: 2 }), 2);
    for (const path of ["bin/tact/src/tui/app.rs", "bin/tact/src/tui/event_loop.rs"]) add(tool("exec_command", `sed -n 1,160p ${path}`, { duration_ns: 30_000_000 }), 1, reads.id);
    patch(PATCH_ENVELOPE, "bin/tact/src/tui/app.rs, bin/tact/src/web/bridge.rs");
    shell("cargo nextest run -p tact", 101, "        FAIL [   0.412s] tact web::bridge::tests::draft_echo_follows_acknowledgement\n\nassertion failed: echo arrived before the acknowledgement\n     Summary [  48.201s] 312 tests run: 311 passed, 1 failed\n", 48);
    // A cell that spawns agents is a landmark through its children.
    const batch = add(tool("exec", "2 tools", { duration_ns: 11_000_000_000, child_count: 2, significance: "landmark" }), 12);
    for (const [agent, role] of [[2, "protocol auditor"], [5, "ordering prover"]] as const) {
      // An agent thread published between the batch's calls must not split the batch.
      if (agent === 5) add(directedThread(1, THREAD_DOCS), 2);
      const spawn = add(tool("spawn_agent", `${role} · ${agent === 2 ? "sol high" : "astra high"}`, { duration_ns: 900_000_000, has_detail: true }), 1, batch.id);
      detail(spawn, { arguments: { role, model: agent === 2 ? "sol" : "astra" }, result: JSON.stringify({ agent_id: agent, role, status: "running" }), metadata: null });
    }
    add(tool("wait_agent", "#2, #5", { duration_ns: 4_000_000_000 }), 4);
    add(tool("list_agents", "all agents", { duration_ns: 2_000_000 }), 1);
    const scan = add(tool("memory", "scan · local · web bridge ordering · 2 candidates", { duration_ns: 41_000_000, has_detail: true }), 2);
    const read = add(tool("memory", "read · local · 12@v2 · 1 memory", { duration_ns: 6_000_000, has_detail: true }), 1);
    const replace = add(tool("memory", "replace · local · 12@v3", { duration_ns: 9_000_000, has_detail: true }), 1);
    const remove = add(tool("memory", "delete · 9@v1", { state: "failed", duration_ns: 3_000_000, has_detail: true }), 1);
    add({ kind: "assistant", text: "One test failed: the draft echo arrived before the acknowledgement. I'll make the reply wait for the loop to apply the command.", complete: true, commentary: false }, 10);
    const send = add(tool("send_agent_message", "→ #2", { duration_ns: 3_000_000, has_detail: true }), 3);
    const plan = add(tool("update_plan", "2/4 done", { duration_ns: 1_000_000, has_detail: true }), 3);
    add(directedThread(2, THREAD_SCHEMA), 20);
    add(directedThread(3, THREAD_FAILED), 20);
    patch(PATCH_ACK, "bin/tact/src/web/bridge.rs");
    shell("cargo nextest run -p tact", 0, "     Summary [  51.004s] 312 tests run: 312 passed, 0 skipped\n", 51);
    const spawn = add(tool("spawn_agent", "review bridge ordering · sol xhigh", { duration_ns: 312_000_000_000, substeps: ["read bridge.rs", "trace Publisher::publish", "report"], child_count: 1, has_detail: true }), 312);
    detail(spawn, { arguments: { role: "ordering reviewer", model: "sol", thinking: "xhigh" }, result: { agent_id: 1, model: "sol", role: "ordering reviewer", status: "completed" }, metadata: null });
    add({ kind: "assistant", text: "## Done\n\nThe loop now polls `requests` next to terminal input:\n\n```rust\ntokio::select! {\n    Some(request) = web.requests.recv() => self.apply(request),\n    Some(event) = terminal.next() => self.handle(event),\n}\n```\n\n- Publications go through an unbounded channel, so a stalled browser cannot block a frame.\n- Every command is applied with the same effect function as its keypress.\n\n```mermaid\nsequenceDiagram\n    participant B as Browser\n    participant L as Event loop\n    participant T as Terminal\n    B->>L: command\n    L->>L: apply keypress effect\n    L-->>B: acknowledgement\n    L-->>T: draft echo\n    L-->>B: draft echo\n```\n\n| Command | Keypress |\n| :-- | :-- |\n| `submit` | Enter |\n| `interrupt` | Esc Esc |\n\nAll **312** tests pass.\n\n![screenshot](/Users/ben/Downloads/absolute-cinema.png)", complete: true, commentary: false }, 30);
    add({ kind: "turn_completed", duration_ns: 1_402_000_000_000 }, 2);
    add({ kind: "effort_changed", to: "high" }, 60);
    add({ kind: "context_compacted", duration_ns: 3_100_000_000 }, 30);
    detail(plan, {
      arguments: {
        explanation: "The bridge test fails, so the reply fix comes before the docs.",
        plan: [
          { step: "Poll web requests next to terminal input", status: "completed" },
          { step: "Apply each command with its keypress effect", status: "completed" },
          { step: "Make the reply wait for the loop to apply the command", status: "in_progress" },
          { step: "Document the stream events", status: "pending" },
        ],
      },
      result: { output: "Plan updated" },
      metadata: null,
    });
    const memoryRecord = (version: number, content: string) => ({
      key: { id: 12, version },
      content,
      created_at_ms: 1_790_000_000_000,
      updated_at_ms: 1_790_100_000_000,
      last_scanned_at_ms: null,
      scan_count: 3,
      last_used_at_ms: 1_790_200_000_000,
      use_count: 2,
      probation_until_ms: null,
    });
    const local = { source: "local", namespace: null, role: null };
    detail(scan, {
      arguments: { operation: "scan", query: "web bridge ordering" },
      result: {
        operation: "scan", backend: local, abstained: false,
        candidates: [
          { key: { id: 12, version: 2 }, preview: "The web bridge applies commands through the same effect function as a keypress.", score: 0.8123 },
          { key: { id: 4, version: 1 }, preview: "Publications are non-blocking; a stalled browser must never stall a frame.", score: 0.6 },
        ],
      },
      metadata: null,
    });
    detail(read, {
      arguments: { operation: "read", keys: [{ id: 12, version: 2 }] },
      result: { operation: "read", backend: local, memories: [memoryRecord(2, "The web bridge applies commands through the same effect function as a keypress.")] },
      metadata: null,
    });
    detail(replace, {
      arguments: { operation: "put", content: "The web bridge applies commands through the same effect function as a keypress.\nAcknowledgements follow the publication.", replace: { id: 12, version: 2 } },
      result: {
        operation: "put", backend: local, replaced: { id: 12, version: 2 },
        previous_content: "The web bridge applies commands through the same effect function as a keypress.",
        memory: memoryRecord(3, "The web bridge applies commands through the same effect function as a keypress.\nAcknowledgements follow the publication."),
      },
      metadata: null,
    });
    detail(remove, {
      arguments: { operation: "delete", key: { id: 9, version: 1 } },
      result: { error: "memory 9 changed since version 1" },
      metadata: null,
    });
    detail(send, {
      arguments: {
        agent_id: 2,
        purpose: "question",
        priority: "urgent",
        in_reply_to: null,
        message: "Does the version check also cover the `subagent_entry` event, or only `snapshot`? If only the snapshot, list the events a v1 client would misparse.",
      },
      result: JSON.stringify({ message_id: 16, thread_id: 4, disposition: "steered" }),
      metadata: null,
    });
    main.context.active_tokens = 142_600;
    main.speed = "fast";
    main.reasoningMode = "pro";
    main.subagents.agents.push({
      id: 1, parent: null, session_id: "019a00ff-7c1e-7d55-9b1f-3e2a9c8d4f01", role: "ordering reviewer",
      task: "Review bridge ordering between acknowledgements and draft echoes.", model: "sol", thinking: "xhigh", reasoning_mode: "pro",
      status: { state: "completed", output: "The acknowledgement must follow the publication." },
    });
    main.agentEntries.set(1, [
      { id: 1, revision: 1, parent: null, kind: "user", text: "Review bridge ordering between acknowledgements and draft echoes." },
      { id: 2, revision: 1, parent: null, ...tool("read", "bin/tact/src/web/bridge.rs", { duration_ns: 5_000_000 }) },
      { id: 3, revision: 1, parent: null, kind: "assistant", text: "The acknowledgement must follow the publication, otherwise a tab can submit a stale revision.", complete: true, commentary: false },
    ]);

    const extra: [number, number | null, string, string, string, Subagent["status"]][] = [
      [2, null, "protocol auditor", "sol", "Audit the web protocol for versioning gaps.", { state: "running" }],
      [3, 2, "schema checker", "luna", "Compare wire.ts with wire.rs field by field.", { state: "running" }],
      [4, 2, "docs verifier", "opus-5.5", "Verify docs/web.md against the implementation.", { state: "failed", error: "The documentation file could not be read: permission denied." }],
      [5, 1, "ordering prover", "astra", "Prove the acknowledgement ordering invariant.", { state: "pending" }],
    ];
    for (const [id, parent, role, model, task, status] of extra) {
      main.subagents.agents.push({ id, parent, session_id: "019a00ff-0000-7000-8000-00000000000" + id, role, task, model, thinking: "high", reasoning_mode: "standard", status });
      main.agentEntries.set(id, [
        { id: 1, revision: 1, parent: null, kind: "user", text: task },
        { id: 2, revision: 1, parent: null, kind: "reasoning", text: "Start with the entry points, then follow the data through each layer." },
        { id: 3, revision: 1, parent: null, kind: "assistant", text: "Working on it: " + role + " has read the relevant files.", complete: false, commentary: true },
      ]);
    }
    // The root turn has finished its own work and waits on the agents it started.
    main.turn = new AbortController();
    main.status = { kind: "waiting_for_background_work" };
    // Each agent's transcript shows the threads it takes part in.
    for (const [thread, messages] of [[1, THREAD_DOCS], [2, THREAD_SCHEMA], [3, THREAD_FAILED]] as const) {
      const parties = new Set(messages.flatMap((message) => [message.from, message.to]));
      for (const agent of parties) {
        const entries = agent === null ? undefined : main.agentEntries.get(agent);
        entries?.push({ id: entries.length + 1, revision: 1, parent: null, ...directedThread(thread, messages) });
      }
    }

    const ideas = new MockSession(this.newId(), "opus-5.5", "medium", "Sketch the overview prompt", WS2);
    ideas.push({ kind: "user", text: "Draft a better overview prompt." });
    ideas.push({ kind: "assistant", text: "Here is a tighter prompt that asks for a **narrative** first and a risk list second.", complete: true, commentary: false });
    ideas.push({ kind: "turn_completed", duration_ns: 21_000_000_000 });
    ideas.unread = true;
    ideas.draft = { rev: 3, text: "Also mention the sandboxed frame.", images: [] };
    ideas.lastActivity -= 600_000;

    const failing = new MockSession(this.newId(), "luna", "low", "Flaky release pipeline test");
    failing.push({ kind: "user", text: "Why does release_pipeline fail on CI only?" });
    failing.push({ kind: "error", message: "stream disconnected before completion: connection reset by peer" });
    failing.failed = true;
    failing.lastActivity -= 3_600_000;

    const bench = new MockSession(this.newId(), "astra", "max", "Benchmark journal compaction");
    for (const session of [main, bench, ideas, failing]) this.sessions.set(session.id, session);
    this.active = main.id;
  }
}

/** A conversation thread entry as the server publishes it: the thread plus its latest message. */
function directedThread(thread: number, messages: readonly AgentMessage[]): EntryBody {
  const latest = messages.at(-1)!;
  const label = (agent: number | null) => (agent === null ? "root" : `agent ${agent}`);
  return {
    kind: "directed_message",
    from: label(latest.from),
    to: label(latest.to),
    body: latest.body,
    delivery: latest.delivery,
    thread,
    messages: [...messages],
  };
}

const message = (fields: Pick<AgentMessage, "id" | "from" | "to" | "purpose" | "body"> & Partial<AgentMessage>): AgentMessage => ({
  priority: "deferred",
  in_reply_to: null,
  delivery: "delivered",
  detail: "queued",
  ...fields,
});

/** The docs verifier reports a finding to the protocol auditor, which replies; the follow-up is long. */
const THREAD_DOCS: AgentMessage[] = [
  message({
    id: 11, from: 4, to: 2, purpose: "finding",
    body: "`docs/web.md` still documents `directed_message` with four fields; the bridge now also sends `thread` and `messages`:\n\n```ts\n{ kind: \"directed_message\"; from; to; body; delivery; thread: number; messages: Message[] }\n```",
  }),
  message({
    id: 12, from: 2, to: 4, purpose: "reply", in_reply_to: 11, detail: "steered",
    body: "Confirmed against `wire.rs`. Keep the four summary fields documented as the latest message; I will flag the protocol version separately.",
  }),
  message({
    id: 13, from: 4, to: 2, purpose: "coordinate", in_reply_to: 12, detail: "started",
    body: [
      "Proposed wording for the section, so we do not both edit it:",
      "",
      "1. `thread` identifies the conversation; every message of one conversation shares it.",
      "2. `messages` is the retained thread in delivery order.",
      "3. Each message carries `id`, `from` (`null` for the root), `to`, `purpose`, and `priority`.",
      "4. `in_reply_to` names the message it answers, when any.",
      "5. `delivery` is `admitted`, `delivered`, `failed`, or `unknown`.",
      "6. `detail` says how the recipient took it, or why delivery failed.",
      "",
      "Open questions:",
      "",
      "- Does a client need the thread's participants, or are `from`/`to` enough?",
      "- Should the entry carry a revision per message?",
      "",
      "```rust",
      "pub(super) struct WireMessage {",
      "    pub(super) id: u64,",
      "    pub(super) from: Option<u64>,",
      "    pub(super) to: u64,",
      "}",
      "```",
    ].join("\n"),
  }),
];

/** The protocol auditor delegates urgently to the schema checker, which has not taken it yet. */
const THREAD_SCHEMA: AgentMessage[] = [
  message({
    id: 14, from: 2, to: 3, purpose: "delegate", priority: "urgent", delivery: "admitted", detail: "steered",
    body: "Stop the field-by-field pass and diff `WireMessage` against `AgentMessage` in `wire.ts` first; report any field whose nullability differs.",
  }),
];

/** The schema checker asks the docs verifier, which had already failed. */
const THREAD_FAILED: AgentMessage[] = [
  message({
    id: 15, from: 3, to: 4, purpose: "question", delivery: "failed", detail: "agent 4 has failed and cannot receive messages",
    body: "Which section of `docs/web.md` lists the stream events?",
  }),
];

const PATCH_FLAKE = [
  "*** Begin Patch",
  "*** Update File: bin/tact/src/web/bridge.rs",
  "@@ async fn publishes_without_blocking()",
  "-        tokio::time::sleep(Duration::from_millis(20)).await;",
  "-        let frame = frames.try_recv().unwrap();",
  "+        let frame = frames.recv().await.expect(\"the publisher sends a frame\");",
  "+        assert_eq!(frame.kind, FrameKind::Snapshot);",
  "@@ fn publisher()",
  "-    let (sender, receiver) = mpsc::channel(1);",
  "+    // Unbounded: a stalled browser must never block the loop.",
  "+    let (sender, receiver) = mpsc::unbounded_channel();",
  "*** End Patch",
].join("\n");

const PATCH_ACK = [
  "*** Begin Patch",
  "*** Update File: bin/tact/src/web/bridge.rs",
  "@@ fn apply(&mut self, request: Request)",
  "-        request.reply(Ok(()));",
  "         self.dispatch(request.effect());",
  "+        // The acknowledgement follows the publication the effect caused.",
  "+        self.publish_pending();",
  "+        request.reply(Ok(()));",
  "*** End Patch",
].join("\n");

const PATCH_ENVELOPE = [
  "*** Begin Patch",
  "*** Update File: bin/tact/src/tui/app.rs",
  "@@ async fn run(&mut self)",
  "     loop {",
  "-        let event = self.terminal.next().await;",
  "-        self.handle(event);",
  "+        tokio::select! {",
  "+            Some(request) = self.web.requests.recv() => self.apply(request),",
  "+            Some(event) = self.terminal.next() => self.handle(event),",
  "+        }",
  "     }",
  "@@ fn apply(&mut self, request: Request)",
  "+        // Web commands end in the same effects as their keypresses.",
  "         self.dispatch(request.into_effect());",
  "*** Add File: bin/tact/src/web/bridge.rs",
  "+//! The bridge between the event loop and the web interface.",
  "+",
  "+pub(crate) struct Bridge {",
  "+    requests: mpsc::UnboundedReceiver<Request>,",
  "+}",
  "+",
  "+impl Bridge {",
  "+    pub(crate) fn new() -> Self {",
  "+        todo!()",
  "+    }",
  "+}",
  "*** End Patch",
].join("\n");

type ScriptedTool = {
  name: string;
  summary: string;
  ms: number;
  arguments: unknown;
  result: unknown;
  substeps?: string[];
  failed?: boolean;
};

function scriptFor(prompt: string) {
  const subject = prompt.length > 60 ? `${prompt.slice(0, 57)}…` : prompt;
  const testCommand = "cargo nextest run -p tact -E 'test(stream)'";
  const tools: ScriptedTool[] = [
    {
      name: "exec_command", summary: "sed -n 1,180p bin/tact/src/web/server.rs", ms: 300,
      arguments: { cmd: "sed -n 1,180p bin/tact/src/web/server.rs" },
      result: { exit_code: 0, output: "//! The HTTP server for the web interface.\n…\n" },
    },
    {
      name: "exec_command", summary: "rg -n 'fn publish' bin/tact/src", ms: 1500,
      arguments: { cmd: "rg -n 'fn publish' bin/tact/src" },
      result: { exit_code: 0, output: "bin/tact/src/web/bridge.rs:104:    pub(crate) fn publish(&self, publication: Publication) {\n" },
    },
    {
      name: "exec_command", summary: "sed -n 1,200p bin/tact/src/web/stream.rs", ms: 300,
      arguments: { cmd: "sed -n 1,200p bin/tact/src/web/stream.rs" },
      result: { exit_code: 0, output: "//! Coalesces publications into frames.\n…\n" },
    },
    {
      name: "apply_patch", summary: "bin/tact/src/web/stream.rs", ms: 260,
      arguments: "*** Begin Patch\n*** Update File: bin/tact/src/web/stream.rs\n@@\n-    let interval = Duration::from_millis(16);\n+    let interval = FRAME_INTERVAL;\n*** End Patch",
      result: { output: "Success. Updated the following files:\nM bin/tact/src/web/stream.rs" },
    },
    {
      name: "exec_command", summary: testCommand, ms: 2400, failed: true,
      substeps: ["Compiling tact v0.7.0", "Running 18 tests"],
      arguments: { cmd: testCommand },
      result: { exit_code: 101, output: "        FAIL [   0.208s] tact web::stream::tests::coalesces_bursts\n\nassertion failed: `left == right` (left: 2, right: 1)\n     Summary [  2.311s] 18 tests run: 17 passed, 1 failed\n" },
    },
    {
      name: "apply_patch", summary: "bin/tact/src/web/stream.rs", ms: 240,
      arguments: "*** Begin Patch\n*** Update File: bin/tact/src/web/stream.rs\n@@ fn flush\n-        pending.clear();\n+        pending.drain_into(sink);\n*** End Patch",
      result: { output: "Success. Updated the following files:\nM bin/tact/src/web/stream.rs" },
    },
    {
      name: "exec_command", summary: testCommand, ms: 2600,
      substeps: ["Compiling tact v0.7.0", "Running 18 tests", "18 passed"],
      arguments: { cmd: testCommand },
      result: { exit_code: 0, output: "     Summary [  2.481s] 18 tests run: 18 passed, 0 skipped\n" },
    },
  ];
  return {
    retry: /retry/i.test(prompt),
    reasoning: `The request is: ${subject}. I should look at where publications leave the loop, check how entries are coalesced, then make the smallest change and run the focused tests.`,
    narration: "Let me look at how the stream coalesces publications before changing anything.",
    tools,
    answer: [
      `I looked into **${subject.replace(/[*_\`]/g, "")}**.`,
      "The stream coalesces entry events to the frame interval, so a burst of tokens becomes one event per frame:",
      [
        "```rust",
        "let mut ticker = tokio::time::interval(FRAME_INTERVAL);",
        "loop {",
        "    tokio::select! {",
        "        _ = ticker.tick() => flush(&mut pending, &sink).await?,",
        "        Some(publication) = publications.recv() => pending.absorb(publication),",
        "    }",
        "}",
        "```",
      ].join("\n"),
      "1. Entries are keyed by `(id, revision)`, so a late duplicate is dropped by the client.\n2. Draft events carry their origin, so each tab ignores its own echo.",
      "The focused tests pass. Want me to run the full suite?",
    ].join("\n\n"),
  };
}

const history: PersistedSession[] = [
  "Add Linux io_uring benchmark job",
  "Investigate flaky resume test",
  "Explain the journal pruning invariant",
  "Port review search to CSS highlights",
  "Speed up transcript rendering",
  "Model colour defaults from the terminal palette",
  "Fix soft wrap in user prompts",
  "Shared memory backend design",
  "Release 0.6.5 changelog",
  "Cloudflare tunnel example",
  "Pin prompts in the composer",
  "Fork action fixes",
  "Subagent split view",
  "Read session improvements",
  "CI cache tuning",
  "Transcript batch connection",
].map((preview, index) => ({
  session_id: `0199${index.toString(16).padStart(4, "0")}-41aa-7b0c-8e3d-5c6f7a8b9c0d`,
  started_at_unix_ms: Date.now() - (index + 1) * 5_400_000,
  model: MOCK_CATALOG.models[index % MOCK_CATALOG.models.length]!.id,
  effort: MOCK_CATALOG.efforts[index % MOCK_CATALOG.efforts.length]!,
  reasoning_mode: "standard" as const,
  preview,
  workspace: index % 5 === 1 ? WS2 : MOCK_DEFAULT_WORKSPACE,
}));

/** Subsequence match, enough for fixture search. */
function fuzzy(query: string, text: string) {
  let position = 0;
  const haystack = text.toLowerCase();
  for (const character of query) {
    position = haystack.indexOf(character, position) + 1;
    if (position === 0) return false;
  }
  return true;
}

const files = [
  "bin/", "bin/tact/", "bin/tact/src/", "bin/tact/src/web/", "bin/tact/src/web/bridge.rs", "bin/tact/src/web/server.rs",
  "bin/tact/src/web/mod.rs", "bin/tact/src/tui/", "bin/tact/src/tui/app.rs", "bin/tact/src/tui/theme.rs",
  "bin/tact/src/tui/context.rs", "bin/tact/src/app/config.rs", "bin/tact/src/app/model.rs", "docs/web.md",
  "web/app.ts", "web/chat.ts", "web/composer.ts", "web/store.ts", "README.md", "Cargo.toml",
];

const skills = [
  { name: "autofix", description: "Iteratively review and repair a branch until it is merge-ready." },
  { name: "humanizer", description: "Rewrite text that sounds AI-generated." },
  { name: "jujutsu", description: "Guide to the jj version control system." },
  { name: "linux-server", description: "Build and benchmark on the Linux server." },
  { name: "review-agent", description: "Read-only, defect-first review of a change." },
];

const recentPrompts = [
  "Run the focused tests and fix any failure.",
  "Explain why the draft echo arrives before the acknowledgement.",
  "Review the diff for missing error context.",
  "!cargo nextest run -p tact -E 'test(web)'",
  "Summarize what changed since main and propose a commit message.",
].map((text, index) => ({
  text, recorded_at_unix_ms: Date.now() - index * 2_700_000, session_id: "019a0001-7c1e-7d55-9b1f-3e2a9c8d4f10", workspace: "/Users/dev/src/tact",
}));

function memorySeed(): ListedMemory[] {
  return [
    "In the Tact repository, use jj instead of git for version control.",
    "Prefer typed errors with context; never include credentials in logs or errors.",
    "The web UI must mirror the TUI: drafts and the active session are shared state in both directions.",
    "Progress indicators stay animated even under prefers-reduced-motion.",
  ].map((content, index) => ({
    key: { id: index + 1, version: 1 }, content,
    created_at_ms: Date.now() - (index + 3) * 86_400_000, updated_at_ms: Date.now() - index * 7_200_000,
    last_scanned_at_ms: Date.now() - index * 600_000, scan_count: 12 - index * 2, last_used_at_ms: null,
    use_count: 6 - index, probation_until_ms: index === 3 ? Date.now() + 86_400_000 : null,
    // One shared memory the user may read but not delete.
    deletable: index !== 1,
  }));
}
