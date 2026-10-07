# Tact web interface

Every running Tact serves a web interface in the background. It is a second front-end onto the
sessions the process is running, not a separate client: the terminal and the browser share the active
session, each session's draft, queue, settings, and transcript. The TUI must stay open for the web
interface to be served (there is no headless mode). Reachability is external: put the port behind
Tailscale, an SSH forward, or similar. Tact implements no transport.

## Ownership

- The TUI event loop owns every live session (a "pane": agent, journal, composer, queue).
- The web server (`bin/tact/src/web`) holds projections and a command port. It never mutates
  session state. The two halves talk only through `web::bridge`.
- **Shared state** has one writer, the loop, and every change is published to every front-end: the
  active session, the live-session set, each session's draft, queue, settings, transcript, and busy
  state. **View state** (scroll, expanded rows, caret, open pickers, theme, review tabs) is local to
  a window and never synchronized.
- A web command ends in the same effect as the equivalent keypress and obeys the same
  preconditions, evaluated on the target session's own pane.
- A session is live in at most one pane of one process. Parallel panes share one working tree.

## Process

- Started on launch in the background; failure to start only sets a status string.
- Config `[web]`: `enabled = true`, `bind = "127.0.0.1"`, `port = 7878` (first port tried; scans 21
  ports, then ephemeral), `public_url = ""` (only used to build copyable links),
  `max_live_sessions = 8`. `--web=false` (`TACT_WEB`) disables it.
- Machine token: `$TACT_HOME/web/token` (0600, 32 CSPRNG bytes, base64url). Registry:
  `$TACT_HOME/web/instances/<pid>.json` (pid, port, workspace, started_at), removed on exit; readers
  tolerate stale files. Assets: `$TACT_HOME/web/assets/v<version>` (bundle id
  `tact-web-v<version>.tar.gz`, override `TACT_WEB_ASSETS`).
- The bundle is looked up on every request while it is missing, so one installed after startup is
  served without a restart; until then `/` shows a built-in page explaining the installation. A
  release build also downloads and verifies the matching bundle in the background on first start.
- The login URL is `http://127.0.0.1:<port>/#k=<token>` (or `public_url` + fragment).

## Authentication

Static assets are public. Every `/api/*` route except `POST /api/login` requires the cookie.

- `POST /api/login` `{ "token": string }` -> 204 and `Set-Cookie: tact=<token>; HttpOnly;
  SameSite=Strict; Path=/`. Otherwise 401.
- Mutating requests (`POST`) must send header `X-Tact: 1`. Any request with an `Origin` header
  must have it match `Host`. The SSE request is checked the same way. No CORS headers.
- Errors are JSON `{ "code": string, "message": string }`. Codes: `unauthorized` (401),
  `invalid_request` (400), `turn_running`, `queue_not_empty`, `nothing_running`, `draft_changed`,
  `session_locked`, `unknown_session`, `too_many_sessions`, `not_available_remotely`, `stale`,
  `disabled` (409/404), `failed` (500), plus the review codes in `web/app/protocol.ts`. Commands
  and queries share one mapping (`bridge::CommandError::code`).

## Reads

| Endpoint | Response |
| :-- | :-- |
| `GET /api/instance` | `{ protocol_version, workspace, repository, live, running }` |
| `GET /api/instances` | `{ instances: [{ pid, port, workspace, live, running, current }] }` (siblings that answer) |
| `GET /api/sessions/{id}/entries/{n}` | `ToolDetail` of one entry (full arguments, result, metadata, each string truncated at 256 KiB) |
| `GET /api/sessions/{id}/agents/{agent}/entries` | `{ entries: WireEntry[] }`, the projected transcript of one subagent |
| `GET /api/sessions/{id}/agents/{agent}/entries/{n}` | `ToolDetail` of one subagent entry |
| `GET /api/stream` | Server-Sent Events, below |

## Stream

One SSE stream per tab, following the shared active session. On connect it emits `hello`, `live`,
`active`, and a `snapshot` of the active session; reconnect repeats this (no replay). Each event is
`event: <name>` with a JSON `data`. Keep-alive comment every 15 s. Entry events are coalesced to
the TUI's frame interval.

| Event | Data |
| :-- | :-- |
| `hello` | `{ protocol_version, client_hint }` |
| `live` | `{ active: string \| null, sessions: SessionSummary[] }` on connect and whenever any summary changes |
| `active` | `{ session }`, always followed by a `snapshot` |
| `snapshot` | `SessionSnapshot` |
| `entry` | `{ session, entry: WireEntry }` upsert by `(id, revision)`; clients drop anything older than what they hold |
| `status` | `{ session, status: TransientStatus \| null }` |
| `queue` | `{ session, items: QueuedPrompt[] }` |
| `draft` | `{ session, rev, text, origin }`, origin is `"terminal"` or `"web:<client>"` |
| `settings` | `{ session, model, effort, reasoning_mode, speed }` |
| `context` | `{ session, active_tokens, window_tokens }`, the composer's context budget |
| `subagents` | `{ session } & SubagentRoster`, coalesced to the frame interval |
| `subagent_entry` | `{ session, agent, entry: WireEntry }` for the active session; same upsert rule as `entry` |
| `closed` | `{ session }` |
| `workspace` | `{ version }` the working tree changed; marks the diff stale |

```ts
type SessionSummary = {
  id: string; title: string; model: string;
  state: "idle" | "running" | "error";
  unread: boolean;            // finished or errored while not active
  has_draft: boolean;
  last_activity_unix_ms: number;
};
type SessionSnapshot = {
  session: string; title: string; model: string; effort: Effort;
  reasoning_mode: "standard" | "pro"; speed: Speed;
  entries: WireEntry[]; status: TransientStatus | null; queue: QueuedPrompt[];
  draft: Draft; running: boolean;
  context: { active_tokens: number; window_tokens: number } | null;
  subagents: SubagentRoster;
};
type Effort = "low" | "medium" | "high" | "xhigh" | "max";
type Speed = "standard" | "fast" | "ultrafast";
// An image belongs to the draft while its marker (e.g. "[Image #2]") occurs in `text`.
type Draft = { rev: number; text: string; images: { marker: string; data_url: string }[] };
type QueuedPrompt = { id: number; text: string; steering: boolean };
type TransientStatus =
  | { kind: "thinking" | "responding" | "warming" | "waiting_for_background_work" | "compacting" | "connecting" | "reconnecting" }
  | { kind: "tool"; name: string }
  | { kind: "retrying"; delay_ns: number; next_attempt: number; max_attempts: number }
  | { kind: "error"; message: string };
// Hidden entries are omitted. `id` is stable; `revision` increases on each change.
type WireEntry = { id: number; revision: number; parent: number | null } & (
  | { kind: "user"; text: string }
  | { kind: "assistant"; text: string; complete: boolean; commentary: boolean }
  | { kind: "reasoning"; text: string }
  | { kind: "tool"; name: string; summary: string; state: "running" | "succeeded" | "failed";
      duration_ns: number | null; substeps: string[]; child_count: number; has_detail: boolean }
  | { kind: "directed_message"; from: string; to: string; body: string; delivery: string }
  | { kind: "forked_from"; session: string }
  | { kind: "effort_changed"; to: string }
  | { kind: "fast_mode_changed"; enabled: boolean }
  | { kind: "reflection_started" }
  | { kind: "interrupted"; count: number }
  | { kind: "context_compacted"; duration_ns: number }
  | { kind: "turn_completed"; duration_ns: number }
  | { kind: "compaction_failed"; message: string }
  | { kind: "error"; message: string });
type ToolDetail = { arguments: unknown; result: unknown | null; metadata: unknown | null };
```

Clients must render unknown entry kinds as a muted generic row.

## Commands

`POST /api/cmd` with `X-Tact: 1` and the body `{ client, cmd: <name>, args: { ...body } }`
(`client` is a random per-tab integer; commands without a body omit `args`). The body deserializes
straight into `bridge::Command`; there is no per-command route. Success is 200 `{}`
(`open_session` returns `{ session }`). The command is acknowledged only after the loop applied it.
`open_session` takes its variant as `args`, for example `{ cmd: "open_session", args: { new: {} } }`.

| Command | Body | Keyboard equivalent | Refused |
| :-- | :-- | :-- | :-- |
| `set_draft` | `session, text` | typing | never (last writer wins) |
| `submit` | `session, rev` | Enter in the composer | `draft_changed` if the draft moved; `not_available_remotely` for terminal-local slash commands such as `/copy`. Queues if a turn is running or a steer is pending. |
| `interrupt` | `session` | cancel-all | `nothing_running` |
| `steer` / `dequeue` | `session, queue_id` | queue panel | `nothing_running` / `unknown_session` for a consumed item |
| `compact` | `session` | Actions: Compact | `turn_running`, `queue_not_empty` |
| `set_model` / `set_effort` / `set_reasoning_mode` / `set_speed` | `session, model\|effort\|mode\|speed` | pickers | see Feature parity |
| `activate` | `session` | focus / Sessions action | `unknown_session` |
| `open_session` | `{ new: { model? } }` \| `{ resume: { session } }` \| `{ fork: { session } }` | Sessions action | `too_many_sessions`, `unknown_session`, `session_locked`, `turn_running` (fork) |
| `close_session` | `session, force?` | close pane | `turn_running` without `force` |

Opening or activating a session from either window makes it the shared active session.

## Feature parity

Everything the terminal can do is reachable from the web through three generic paths, so a new
feature is an enum variant plus its loop-side handler and never a new route:

- **Commands** (`bridge::Command`, `POST /api/cmd`) change state and obey the same preconditions
  as the keypress, evaluated by the loop on the target pane.
- **Queries** (`bridge::Query`, `POST /api/query`) read data on demand. The body is
  `{ query: <name>, args?: { ... } }` with `X-Tact: 1`; the reply is the bare payload below.
  The server forwards every query to the loop, which answers from state it owns or by calling,
  off the loop thread, the same UI-agnostic function the terminal picker uses. Refusals use the
  common error codes.
- **Publications** (`bridge::Publication`, stream events) carry state that changes over time.

Data is computed once, in modules both front-ends call: `search` (fuzzy ranking, workspace
paths), `core::extensions::SkillMatches`, `tui::session` (history pages, recent prompts),
`tui::context` (diagnostics), `core::subagent_roster` (the subagent tree), `app::model` (the model
catalog and its couplings), and `app::config::ConfigDocument`. Ranking is identical in both
front-ends: best match first, ties in source order.

### Inventory

| Terminal feature | Web |
| :-- | :-- |
| Change effort | `set_effort` command; choices from the `models` query |
| Pro reasoning (`p` in the effort dial) | `set_reasoning_mode` |
| Speed tiers | `set_speed`; `effective_speeds` in `models` shows the tier a model actually runs |
| Select model | `set_model`, before the session's first turn |
| New / resume / fork / switch / close session | `open_session`, `activate`, `close_session`; `history` query |
| Compact | `compact` |
| Reflection | `reflect` |
| Prepare handoff | `handoff` |
| Reload config | `reload_config` |
| Edit config ($EDITOR) | `config` query, then `write_config` from an in-browser editor |
| Memory browser | `memories` query, `delete_memory`; sort and filter are view state |
| Subagents overlay | `subagents` event and snapshot field; transcripts via `GET .../agents/{agent}/entries` and `subagent_entry` events |
| Max subagents (tree overlay) | `set_max_subagents` |
| Continue after a subagent completes | automatic in the loop; nothing to send |
| Debug context | `context_diagnostics` query |
| Context budget in the composer | `context` event and snapshot field |
| `@` file mention | `files` query; inserting the path is a local draft edit |
| `@@` session mention | `history` query; inserting `@@<id>` is a local draft edit |
| `$` skill mention | `skills` query |
| Recent prompts picker | `recent_prompts` query |
| Paste image | `attach_image`; images are part of the shared draft |
| `!` shell command | `submit` of a draft starting with `!`, exactly like Enter |
| Queue: steer / edit / delete | `steer`, `edit_queued`, `dequeue` |
| Interrupt (cancel-all) | `interrupt`; also cancels a handoff being prepared (Esc in the terminal). A compaction cannot be interrupted (`turn_running`) |
| Theme | web-local view state (light/dark), never synchronized |
| Keybindings | the web's own shortcut help |
| Copy | browser clipboard from the rendered transcript |
| Open draft in $EDITOR | not offered (`not_available_remotely` if attempted through `submit`) |
| Open in browser / Copy web link | terminal-only by nature |

### Commands

In addition to the commands above:

| Command | Args | Refused |
| :-- | :-- | :-- |
| `set_reasoning_mode` | `session, mode: "standard" \| "pro"` | `invalid_request` if the model does not list the mode |
| `set_speed` | `session, speed: Speed` | never; the model may run a lower tier |
| `edit_queued` | `session, queue_id, text` | `unknown_session` for a consumed item |
| `attach_image` | `session, data_url` (a `data:image/...;base64` URL) | `invalid_request` for a non-image |
| `reflect` | `session, instructions?` | `turn_running`, `queue_not_empty` |
| `handoff` | `session` | `turn_running`, `queue_not_empty` |
| `reload_config` | none | `failed` with the load error |
| `write_config` | `text, revision` | `stale` if the file changed since `config`; `invalid_request` if `text` does not load; `not_available_remotely` if `text` adds credentials. The file is unchanged on refusal and reloaded on success |
| `delete_memory` | `key: { id, version, namespace? }` | `disabled`; `not_available_remotely` unless the backend lets this user delete the key (`deletable` in `memories`); `stale` if the memory changed; `failed` |
| `set_max_subagents` | `limit` | never |

Settings couplings come from the `models` catalog and are enforced by the loop: the model is
selectable only before the first turn; `effort_fixed_after_start` models refuse `set_effort`
once a turn started (`invalid_request`). A session has started when its snapshot has entries.
`attach_image` appends a new marker to the draft text and publishes a `draft` event. A
`set_draft` whose text no longer contains a marker drops that image.

### Queries

| Query | Args | Reply |
| :-- | :-- | :-- |
| `models` | none | `ModelCatalog` |
| `history` | `query?, cursor?` | `{ sessions: PersistedSession[], next_cursor: string \| null }` (resumable persisted sessions of this workspace, newest first, pages of 50) |
| `files` | `query?` | `{ paths: string[] }` (at most 50; directories end in `/`) |
| `skills` | `query?` | `{ skills: { name, description }[] }` |
| `recent_prompts` | `session, scope?: "global" \| "current_session", query?` | `{ prompts: { text, recorded_at_unix_ms, session_id, workspace }[] }` |
| `context_diagnostics` | `session` | `ContextDiagnostics` |
| `memories` | none | `{ access: { source, namespace, role }, records: (MemoryRecord & { deletable: boolean })[] }`; `disabled` when memory is off |
| `config` | none | `{ path, text, revision }`; `not_available_remotely` when the file holds credentials (they never leave the terminal), `invalid_request` when it does not parse |

```ts
type ModelCatalog = {
  models: {
    id: string; label: string; provider: "openai" | "anthropic";
    reasoning_modes: ("standard" | "pro")[];
    effective_speeds: Speed[];          // indexed like `speeds`
    effort_fixed_after_start: boolean;
  }[];
  efforts: Effort[];
  speeds: Speed[];                      // increasing
};
type PersistedSession = {
  session_id: string; started_at_unix_ms: number; model: string; effort: Effort;
  reasoning_mode: "standard" | "pro"; workspace: string; preview: string;
};
type ContextDiagnostics = {
  model_window_tokens: number; auto_compact_token_limit: number | null;
  active_tokens: number | null;
  usage: { input: number; cached_input: number; uncached_input: number; output: number; total: number } | null;
  continuation: "full_context" | "previous_response" | null; prompt_cache: boolean | null;
  compactions_started: number; compactions_completed: number;
  last_compaction: { trigger: "automatic" | "manual"; started_at_unix_ms: number;
    completed_at_unix_ms: number | null; before_tokens: number | null; after_tokens: number | null } | null;
};
type MemoryRecord = {
  key: { id: number; version: number; namespace?: string }; content: string;
  created_at_ms: number; updated_at_ms: number; last_scanned_at_ms: number | null;
  scan_count: number; last_used_at_ms: number | null; use_count: number;
  probation_until_ms: number | null;
};
type SubagentRoster = {
  max_subagents: number;
  agents: {                             // arrival order; an unlisted parent makes a root
    id: number; parent: number | null; session_id: string; role: string; task: string;
    model: string; thinking: Effort;
    status: { state: "pending" | "running" | "interrupted" | "closing" | "closed" }
      | { state: "completed"; output: unknown } | { state: "failed"; error: string };
  }[];
};
```

## Review (diff and overview)

The review engine keeps its existing payloads (`web/app/protocol.ts`) with these changes:

- The diff context is per workspace and prepared lazily by `GET /api/review`, then cached by
  generation. `POST /api/refresh`, `/api/range` are unchanged.
- Overviews, AI reviews, and inline question threads belong to a session. Their requests gain
  `session`; they run through that session's worker as a clean-context auxiliary prompt, and are
  cancelled when the session closes.
  `GET /api/review?session=<id>` and `POST /api/refresh` (optional `session`) include that session's
  selected overview and question threads; `POST /api/questions` requires `session`.
- `/api/status` polling is replaced by the `workspace` stream event. The server watches the
  workspace version while any stream is connected and emits it when it changes. "Agent running" for
  staleness means any live session is busy.
- `/api/decision` and `/api/cancel` are gone. `POST /api/review/compose` with the former decision
  body returns `{ markdown }`, the canonical review text; the client writes it into the active
  session's draft (**Send to chat**).

## UI

Chat first. Left sidebar: **+ New chat**, **Live** sessions with state markers (pulsing running,
idle, accent unread, pencil draft), **History** with search; an instance switcher. The main pane is
the transcript and the shared composer. A secondary panel (a drawer on phones) has **Changes**
(Pierre diffs, live while the agent edits) and **Overview** tabs. The styling takes its palette and
rhythm from the TUI theme and is mobile-first.

## Security notes

The token is equivalent to a shell as the user. Drafts are visible to every connected device. All
transcript and draft text is rendered as text or through the sanitizing Markdown renderer; agent
MDX runs only in the opaque-origin sandboxed overview frame.
