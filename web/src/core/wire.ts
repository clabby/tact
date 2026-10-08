// Wire types of the Tact web protocol. `docs/web.md` (and `web::bridge`) is the source of truth.

export type Effort = "low" | "medium" | "high" | "xhigh" | "max";
export type Speed = "standard" | "fast" | "ultrafast";
export type ReasoningMode = "standard" | "pro";
export type SessionState = "idle" | "running" | "error";

export type SessionSummary = {
  id: string;
  title: string;
  model: string;
  state: SessionState;
  /** Finished or errored while not active. */
  unread: boolean;
  has_draft: boolean;
  last_activity_unix_ms: number;
  /** The directory the session's agent works in, fixed when the session is created. */
  workspace: string;
};

export type QueuedPrompt = { id: number; text: string; steering: boolean };

/** An image belongs to the draft while its marker (e.g. "[Image #2]") occurs in the text. */
export type DraftImage = { marker: string; data_url: string };
export type Draft = { rev: number; text: string; images: DraftImage[] };

/** `"terminal"` or `"web:<client>"`. */
export type DraftOrigin = string;

export type ContextBudget = { active_tokens: number; window_tokens: number };

export type TransientStatus =
  | {
    kind:
      | "thinking"
      | "responding"
      | "warming"
      | "waiting_for_background_work"
      | "compacting"
      | "connecting"
      | "reconnecting";
  }
  | { kind: "tool"; name: string }
  | { kind: "retrying"; delay_ns: number; next_attempt: number; max_attempts: number }
  | { kind: "error"; message: string };

export type ToolState = "running" | "succeeded" | "failed";

/**
 * How a finished shell-like call ended: its exit code, the last non-empty output lines (at most 8,
 * each at most 200 characters, ANSI escapes removed), and a test or build summary such as
 * "17 passed, 1 failed" when the output has a recognisable one.
 */
export type ToolOutcome = { exit_code: number | null; tail: string[]; summary: string | null };

/** The size of an applied patch, computed from its envelope. */
export type PatchStats = { files: number; additions: number; deletions: number };

/**
 * A landmark keeps a row of its own: a failure, a patch, a plan update, agent coordination, a
 * memory write, or a Code Mode cell that made one of those calls. Everything else is routine.
 */
export type Significance = "routine" | "landmark";

export type MessagePurpose = "delegate" | "coordinate" | "finding" | "question" | "reply";
export type MessageDelivery = "admitted" | "delivered" | "failed" | "unknown";

/** One message of a conversation between agents. */
export type AgentMessage = {
  id: number;
  /** The sending agent, or `null` for the root session. */
  from: number | null;
  /** The recipient agent. */
  to: number;
  purpose: MessagePurpose;
  priority: "deferred" | "urgent";
  in_reply_to: number | null;
  body: string;
  /** `unknown` until the first delivery state arrives. */
  delivery: MessageDelivery;
  /** How the recipient took the message (`started`, `queued` or `steered`), or the failure. */
  detail: string | null;
};

export type EntryBody =
  /** `images` counts the attachments; the i-th replaces the i-th "[Image #N]" marker in the text. */
  | { kind: "user"; text: string; images?: number }
  | { kind: "assistant"; text: string; complete: boolean; commentary: boolean }
  | { kind: "reasoning"; text: string }
  | {
    kind: "tool";
    name: string;
    summary: string;
    state: ToolState;
    duration_ns: number | null;
    /** How long a running call had run when the server sent this; absent once it finishes. */
    elapsed_ns?: number | null;
    substeps: string[];
    child_count: number;
    has_detail: boolean;
    /** Set once a shell-like call finishes; absent from older servers. */
    outcome?: ToolOutcome | null;
    /** Set for an applied `apply_patch`; absent from older servers and for failed patches. */
    stats?: PatchStats | null;
    /** Whether the call may fold into a run of routine work; absent from older servers, whose calls never fold. */
    significance?: Significance;
  }
  /**
   * One conversation thread between agents, updated in place as messages arrive and delivery
   * states change. `messages` is the retained thread in delivery order; `from`, `to`, `body`
   * and `delivery` summarize its latest message.
   */
  | {
    kind: "directed_message";
    from: string;
    to: string;
    body: string;
    delivery: string;
    thread: number;
    messages: AgentMessage[];
  }
  | { kind: "forked_from"; session: string }
  | { kind: "effort_changed"; to: string }
  | { kind: "fast_mode_changed"; enabled: boolean }
  | { kind: "reflection_started" }
  | { kind: "interrupted"; count: number }
  | { kind: "context_compacted"; duration_ns: number }
  | { kind: "turn_completed"; duration_ns: number }
  | { kind: "compaction_failed"; message: string }
  | { kind: "error"; message: string };

/**
 * `id` is stable; `revision` increases on every change to the entry. A newer server may send kinds
 * missing from `EntryBody`; renderers must treat any other kind as a generic row.
 * `at_ms` is the unix-millisecond time the entry's first record was recorded, when known.
 */
export type WireEntry = { id: number; revision: number; parent: number | null; at_ms?: number | null } & EntryBody;

export type SubagentStatus =
  | { state: "pending" | "running" | "interrupted" | "closing" | "closed" }
  | { state: "completed"; output: unknown }
  | { state: "failed"; error: string };

export type Subagent = {
  id: number;
  /** An id missing from the roster makes the agent a root. */
  parent: number | null;
  session_id: string;
  role: string;
  task: string;
  model: string;
  thinking: Effort;
  reasoning_mode: ReasoningMode;
  status: SubagentStatus;
};

export type SubagentRoster = { max_subagents: number; agents: Subagent[] };

export type SessionSnapshot = {
  session: string;
  title: string;
  model: string;
  effort: Effort;
  reasoning_mode: ReasoningMode;
  speed: Speed;
  entries: WireEntry[];
  status: TransientStatus | null;
  queue: QueuedPrompt[];
  draft: Draft;
  running: boolean;
  context: ContextBudget | null;
  subagents: SubagentRoster;
};

export type ToolDetail = { arguments: unknown; result: unknown | null; metadata: unknown | null };

export type StreamEvents = {
  hello: { protocol_version: number; client_hint: string };
  live: { active: string | null; sessions: SessionSummary[] };
  active: { session: string };
  snapshot: SessionSnapshot;
  entry: { session: string; entry: WireEntry };
  status: { session: string; status: TransientStatus | null };
  queue: { session: string; items: QueuedPrompt[] };
  draft: { session: string; rev: number; text: string; images?: DraftImage[]; origin: DraftOrigin };
  settings: { session: string; model: string; effort: Effort; reasoning_mode: ReasoningMode; speed: Speed };
  context: { session: string } & ContextBudget;
  subagents: { session: string } & SubagentRoster;
  subagent_entry: { session: string; agent: number; entry: WireEntry };
  closed: { session: string };
  /** `checkout` is the directory whose files changed; without it, the session's workspace. */
  workspace: { version: string; checkout?: string };
};

export type StreamEventName = keyof StreamEvents;

export type StreamEvent = {
  [Name in StreamEventName]: { type: Name; data: StreamEvents[Name] };
}[StreamEventName];

export const STREAM_EVENTS = [
  "hello", "live", "active", "snapshot", "entry", "status", "queue", "draft", "settings", "context",
  "subagents", "subagent_entry", "closed", "workspace",
] as const satisfies readonly StreamEventName[];

export type OpenSpec =
  /** `workspace` is an absolute path; without it the default workspace is used. */
  | { new: { model?: string; workspace?: string } }
  | { resume: { session: string } }
  | { fork: { session: string } };

export type MemoryKey = { id: number; version: number; namespace?: string };

/** Command arguments (`args`); `undefined` marks a command without arguments. */
export type Commands = {
  set_draft: { session: string; text: string };
  /** While a turn runs the prompt steers it, or waits in the queue when `queue` is set. */
  submit: { session: string; rev: number; queue?: boolean };
  interrupt: { session: string };
  steer: { session: string; queue_id: number };
  dequeue: { session: string; queue_id: number };
  edit_queued: { session: string; queue_id: number; text: string };
  compact: { session: string };
  set_model: { session: string; model: string };
  set_effort: { session: string; effort: Effort };
  set_reasoning_mode: { session: string; mode: ReasoningMode };
  set_speed: { session: string; speed: Speed };
  activate: { session: string };
  open_session: OpenSpec;
  close_session: { session: string; force?: boolean };
  attach_image: { session: string; data_url: string };
  reflect: { session: string; instructions?: string };
  handoff: { session: string };
  reload_config: undefined;
  write_config: { text: string; revision: string };
  delete_memory: { key: MemoryKey };
  set_max_subagents: { limit: number };
};

export type CommandName = keyof Commands;

export type CommandReplies = {
  [Name in CommandName]: Name extends "open_session" ? { session: string } : Record<string, never>;
};

export type ModelInfo = {
  id: string;
  label: string;
  provider: "openai" | "anthropic";
  reasoning_modes: ReasoningMode[];
  /** The tier the model actually runs for each entry of `ModelCatalog.speeds`. */
  effective_speeds: Speed[];
  effort_fixed_after_start: boolean;
};

export type ModelCatalog = { models: ModelInfo[]; efforts: Effort[]; speeds: Speed[] };

export type PersistedSession = {
  session_id: string;
  started_at_unix_ms: number;
  model: string;
  effort: Effort;
  reasoning_mode: ReasoningMode;
  workspace: string;
  preview: string;
};

export type RecentPrompt = { text: string; recorded_at_unix_ms: number; session_id: string; workspace: string };

export type ContextDiagnostics = {
  model_window_tokens: number;
  auto_compact_token_limit: number | null;
  active_tokens: number | null;
  usage: { input: number; cached_input: number; uncached_input: number; output: number; total: number } | null;
  continuation: "full_context" | "previous_response" | null;
  prompt_cache: boolean | null;
  compactions_started: number;
  compactions_completed: number;
  last_compaction: {
    trigger: "automatic" | "manual";
    started_at_unix_ms: number;
    completed_at_unix_ms: number | null;
    before_tokens: number | null;
    after_tokens: number | null;
  } | null;
};

export type MemoryRecord = {
  key: MemoryKey;
  content: string;
  created_at_ms: number;
  updated_at_ms: number;
  last_scanned_at_ms: number | null;
  scan_count: number;
  last_used_at_ms: number | null;
  use_count: number;
  probation_until_ms: number | null;
};

/** A memory as listed for this user; `deletable` says whether the backend lets them delete it. */
export type ListedMemory = MemoryRecord & { deletable: boolean };

/**
 * A working copy of a repository: its main checkout, a git worktree, or a jj workspace. `label` is
 * the branch (git) or workspace name (jj). `current` marks the session's workspace (the default
 * workspace without a session); `touched` says the session's recent tool calls referred to a path
 * inside it, as a hint.
 */
export type Checkout = {
  path: string;
  name: string;
  label: string;
  kind: "git" | "jj";
  head: string | null;
  changed_files: number | null;
  current: boolean;
  missing: boolean;
  touched: boolean;
};

/** The checkouts of a session's repository (main checkout first) and other recent workspaces, newest first. */
export type Workspaces = { default: string; checkouts: Checkout[]; recent: string[] };

/** Query arguments and replies; `undefined` arguments mark a query without `args`. */
export type Queries = {
  models: { args: undefined; reply: ModelCatalog };
  history: { args: { query?: string; cursor?: string | null }; reply: { sessions: PersistedSession[]; next_cursor: string | null } };
  files: { args: { query?: string }; reply: { paths: string[] } };
  skills: { args: { query?: string }; reply: { skills: { name: string; description: string }[] } };
  recent_prompts: {
    args: { session: string; scope?: "global" | "current_session"; query?: string };
    reply: { prompts: RecentPrompt[] };
  };
  context_diagnostics: { args: { session: string }; reply: ContextDiagnostics };
  memories: {
    args: undefined;
    reply: { access: { source: string; namespace: string | null; role: string | null }; records: ListedMemory[] };
  };
  config: { args: undefined; reply: { path: string; text: string; revision: string } };
  workspaces: { args: { session?: string }; reply: Workspaces };
};

export type QueryName = keyof Queries;

export type Instance = {
  protocol_version: number;
  workspace: string;
  repository: string;
  live: number;
  running: number;
};

export type SiblingInstance = {
  pid: number;
  port: number;
  workspace: string;
  live: number;
  running: number;
  current: boolean;
};
