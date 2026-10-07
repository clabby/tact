import type {
  ContextBudget,
  DraftImage,
  DraftOrigin,
  Effort,
  QueuedPrompt,
  ReasoningMode,
  SessionSnapshot,
  SessionSummary,
  Speed,
  StreamEvent,
  SubagentRoster,
  TransientStatus,
  WireEntry,
} from "./wire";

export type Connection = "connecting" | "open" | "reconnecting" | "locked";

/** Entries keyed by id with their transcript order. */
export type TranscriptData = {
  entries: Map<number, WireEntry>;
  /** Entry ids in transcript order. */
  order: number[];
};

/** The projection of the active session that the stream keeps current. */
export type SessionView = TranscriptData & {
  id: string;
  title: string;
  model: string;
  effort: Effort;
  reasoningMode: ReasoningMode;
  speed: Speed;
  status: TransientStatus | null;
  queue: QueuedPrompt[];
  draft: { rev: number; text: string; images: DraftImage[]; origin: DraftOrigin | null };
  running: boolean;
  context: ContextBudget | null;
  subagents: SubagentRoster;
  /** Subagent transcripts, filled by `subagent_entry` events and on-demand loads. */
  agents: Map<number, TranscriptData>;
};

export type AppState = {
  connection: Connection;
  protocolVersion: number | null;
  live: SessionSummary[];
  /** The shared active session as last announced; `session` follows once its snapshot arrives. */
  active: string | null;
  session: SessionView | null;
  workspace: string | null;
};

/**
 * What one event changed, so views can update exactly the affected DOM. `session` means the
 * active session's view was replaced wholesale (a snapshot or a close).
 */
export type Change =
  | {
    type: "connection" | "live" | "session" | "status" | "queue" | "draft" | "settings" | "context"
      | "subagents" | "workspace";
  }
  | { type: "entry"; id: number; added: boolean }
  | { type: "subagent_entry"; agent: number; id: number };

export type Listener = (changes: readonly Change[], state: AppState) => void;

export function initialState(): AppState {
  return { connection: "connecting", protocolVersion: null, live: [], active: null, session: null, workspace: null };
}

export function transcriptData(entries: readonly WireEntry[]): TranscriptData {
  const data: TranscriptData = { entries: new Map(), order: [] };
  for (const entry of entries) upsert(data, entry);
  return data;
}

/**
 * Stores `entry` unless the same or a newer revision is already held. Returns null when the entry
 * was ignored, and otherwise whether it is new to the transcript.
 */
export function upsert(data: TranscriptData, entry: WireEntry): { added: boolean } | null {
  const held = data.entries.get(entry.id);
  if (held && held.revision >= entry.revision) return null;
  data.entries.set(entry.id, entry);
  if (!held) insertOrdered(data.order, entry.id);
  return { added: !held };
}

function sessionView(snapshot: SessionSnapshot): SessionView {
  return {
    ...transcriptData(snapshot.entries),
    id: snapshot.session,
    title: snapshot.title,
    model: snapshot.model,
    effort: snapshot.effort,
    reasoningMode: snapshot.reasoning_mode,
    speed: snapshot.speed,
    status: snapshot.status,
    queue: snapshot.queue,
    draft: { rev: snapshot.draft.rev, text: snapshot.draft.text, images: snapshot.draft.images ?? [], origin: null },
    running: snapshot.running,
    context: snapshot.context,
    subagents: snapshot.subagents ?? { max_subagents: 0, agents: [] },
    agents: new Map(),
  };
}

/**
 * Applies one stream event to `state` in place and reports what changed. Session-scoped events
 * for any session other than the displayed one are ignored: the stream follows the active session
 * and a stale event must not leak into another transcript.
 */
export function reduce(state: AppState, event: StreamEvent): Change[] {
  switch (event.type) {
    case "hello":
      state.protocolVersion = event.data.protocol_version;
      state.connection = "open";
      return [{ type: "connection" }];
    case "live": {
      state.live = event.data.sessions;
      state.active = event.data.active;
      const title = state.session && event.data.sessions.find((summary) => summary.id === state.session!.id)?.title;
      if (title && state.session && title !== state.session.title) {
        state.session.title = title;
        return [{ type: "live" }, { type: "settings" }];
      }
      return [{ type: "live" }];
    }
    case "active":
      state.active = event.data.session;
      return [{ type: "live" }];
    case "snapshot":
      state.session = sessionView(event.data);
      state.active = event.data.session;
      return [{ type: "session" }];
    case "closed": {
      state.live = state.live.filter((summary) => summary.id !== event.data.session);
      if (state.active === event.data.session) state.active = null;
      const changes: Change[] = [{ type: "live" }];
      if (state.session?.id === event.data.session) {
        state.session = null;
        changes.push({ type: "session" });
      }
      return changes;
    }
    case "workspace":
      state.workspace = event.data.version;
      return [{ type: "workspace" }];
  }

  const session = state.session;
  if (!session || session.id !== event.data.session) return [];
  switch (event.type) {
    case "entry": {
      const result = upsert(session, event.data.entry);
      return result ? [{ type: "entry", id: event.data.entry.id, added: result.added }] : [];
    }
    case "subagent_entry": {
      const { agent, entry } = event.data;
      let transcript = session.agents.get(agent);
      if (!transcript) session.agents.set(agent, transcript = transcriptData([]));
      return upsert(transcript, entry) ? [{ type: "subagent_entry", agent, id: entry.id }] : [];
    }
    case "context":
      session.context = { active_tokens: event.data.active_tokens, window_tokens: event.data.window_tokens };
      return [{ type: "context" }];
    case "subagents":
      session.subagents = { max_subagents: event.data.max_subagents, agents: event.data.agents };
      return [{ type: "subagents" }];
    case "status":
      session.status = event.data.status;
      return [{ type: "status" }];
    case "queue":
      session.queue = event.data.items;
      return [{ type: "queue" }];
    case "draft":
      if (event.data.rev <= session.draft.rev) return [];
      session.draft = {
        rev: event.data.rev,
        text: event.data.text,
        images: event.data.images ?? session.draft.images,
        origin: event.data.origin,
      };
      return [{ type: "draft" }];
    case "settings":
      session.model = event.data.model;
      session.effort = event.data.effort;
      session.reasoningMode = event.data.reasoning_mode;
      session.speed = event.data.speed;
      return [{ type: "settings" }];
  }
}

function insertOrdered(order: number[], id: number) {
  if (order.length === 0 || order[order.length - 1]! < id) {
    order.push(id);
    return;
  }
  let low = 0;
  let high = order.length;
  while (low < high) {
    const middle = (low + high) >> 1;
    if (order[middle]! < id) low = middle + 1;
    else high = middle;
  }
  order.splice(low, 0, id);
}

/** Owns the app state and notifies views of each batch of changes. */
export class Store {
  readonly state = initialState();
  private listeners = new Set<Listener>();

  subscribe(listener: Listener) {
    this.listeners.add(listener);
    return () => void this.listeners.delete(listener);
  }

  dispatch(event: StreamEvent) {
    this.notify(reduce(this.state, event));
  }

  setConnection(connection: Connection) {
    if (this.state.connection === connection) return;
    this.state.connection = connection;
    this.notify([{ type: "connection" }]);
  }

  /** Whether any live session is busy; the review panel treats that as "agent running". */
  anyRunning() {
    return this.state.live.some((summary) => summary.state === "running");
  }

  private notify(changes: Change[]) {
    if (changes.length === 0) return;
    for (const listener of this.listeners) listener(changes, this.state);
  }
}
