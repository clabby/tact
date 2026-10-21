# Tact web interface

Every running Tact serves a web interface in the background. It is a second front-end onto the
sessions the process is running, not a separate client: the terminal and the browser share the active
session, each session's draft, queue, settings, and transcript. The TUI must stay open for the web
interface to be served (there is no headless mode). Reachability needs a tunnel: Tact can publish
itself to your tailnet (`web.tailscale`), or you run a tunnel (Tailscale, an SSH forward, or
similar) and give Tact its address (`web.public_url`). See "Remote access".

## Ownership

- The TUI event loop owns every live session (a "pane": agent, journal, composer, queue).
- The web server (`bin/tact/src/web`) holds projections and a command port. It never mutates
  session state. The two halves talk only through the channels in `web::bridge`, which carry the
  typed messages of `core::protocol`.
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
  ports, then ephemeral), `public_url = ""` (the address of a tunnel you run; used for sign-in links), `tailscale = false` (publish the server to your tailnet when a sign-in link for
  another device is first wanted),
  `max_live_sessions = 8`. `public_url` and `tailscale` are mutually exclusive: setting both is a
  configuration error. `--web=false` (`TACT_WEB`) disables the server.
- Config `[web.monitor]`: `enabled = true` (the monitor model checks the sessions while a browser
  shows the Monitor and at least two sessions are live), `model = ""` and `thinking = ""`, which work like `[agent] model` and
  `[agent] thinking`: any model id and effort those accept, and empty means Tact's default model
  (`[agent] model` after command-line overrides) with `[agent] thinking`. An explicit model
  without an explicit thinking runs at that model's catalog default effort. `interval_seconds = 120`
  is the shortest time between automatic checks, at least 10. An unknown model or effort, or an
  interval under 10, is a configuration error. A model whose credentials are missing, or an
  access-gated model such as Mythos, leaves the monitor unavailable at runtime instead, with a
  sentence naming the model. Reloading the configuration applies the whole section to the next
  check. Checks cost what the chosen model costs, so set a smaller model here to keep them cheap,
  or a fast one with a short interval to check more often. See "Monitor".
- Machine token: `$TACT_HOME/web/token` (0600, 32 CSPRNG bytes, base64url). Registry:
  `$TACT_HOME/web/instances/<pid>.json` (pid, port, workspace, started_at), removed on exit; readers
  tolerate stale files. Assets: `$TACT_HOME/web/assets/v<version>` (bundle id
  `tact-web-v<version>.tar.gz`, override `TACT_WEB_ASSETS`); a build from source uses
  `$TACT_HOME/web/assets/development` instead, which `just install-dev` links.
- The bundle is looked up on every request while it is missing, so one installed after startup is
  served without a restart; until then `/` shows a built-in page explaining the installation. A
  release build also downloads and verifies the matching bundle in the background on first start.
- The login URL is `http://127.0.0.1:<port>/#k=<token>`, or the `public_url` or Tailscale address
  plus the same fragment.

## Authentication

Static assets are public. Every `/api/*` route except `POST /api/login` requires the cookie.

- `POST /api/login` `{ "token": string }` -> 204 and `Set-Cookie: tact=<token>; HttpOnly;
  SameSite=Strict; Path=/`. Otherwise 401.
- Mutating requests (`POST`) must send header `X-Tact: 1`. Any request with an `Origin` header
  must have it match `Host`. The SSE request is checked the same way. No CORS headers.
- Errors are JSON `{ "code": string, "message": string }`. Codes: `unauthorized` (401),
  `invalid_request` (400), `turn_running`, `queue_not_empty`, `nothing_running`, `draft_changed`,
  `session_locked`, `unknown_session`, `unknown_queue_item`, `too_many_sessions`,
  `not_available_remotely`, `stale`, `disabled` (409/404), `failed` (500). Commands and queries
  share one mapping (`protocol::CommandError::code`). The review routes use their own error body;
  see "Review errors".

## Reads

| Endpoint | Response |
| :-- | :-- |
| `GET /api/instance` | `{ pid, protocol_version, workspace, repository, live, running }` (`running` counts sessions with a turn or shell in flight) |
| `GET /api/instances` | `{ instances: [{ pid, port, workspace, live, running, current }] }` (siblings that answer) |
| `GET /api/sessions/{id}/entries/{n}` | `ToolDetail` of one entry (full arguments, result, metadata, each string truncated at 256 KiB) |
| `GET /api/sessions/{id}/agents/{agent}/entries` | `{ entries: WireEntry[] }`, the projected transcript of one subagent |
| `GET /api/sessions/{id}/entries/{n}/images/{i}` | The i-th image attached to a user entry, decoded; only PNG, JPEG, GIF, and WebP are served |
| `GET /api/file?path=&session=` | A local image that Markdown refers to (absolute, `file://`, or relative as "Local files" describes), as the terminal transcript shows such images. Only files whose bytes are PNG, JPEG, GIF, or WebP and at most 25 MiB are served. Anything else is 404 |
| `GET /api/document?path=&session=` | `{ path, text }`, a local text file that Markdown links to, for the file viewer. `path` is the file's canonical absolute path. Refusals: `not_found` (404), `not_a_file` (422, directories and special files), `too_large` (413, over 2 MiB), `binary` (415, not UTF-8 or containing NUL), `unreadable` (403) |
| `GET /api/sessions/{id}/agents/{agent}/entries/{n}` | `ToolDetail` of one subagent entry |
| `GET /api/stream` | Server-Sent Events, below |

### Local files

Both file routes take a decoded filesystem path in the query value. A literal `file://` prefix
is also accepted and stripped. Markdown file URLs are normalized by the client before the request.
A relative path resolves against the workspace of `session`, or the default workspace when the
session is not live. The viewer resolves its document's relative links and images against the
returned canonical directory before requesting them. Files anywhere on this machine are served,
as the token already grants that reach. Both routes check the opened file's type and
size before reading it. Reads also stop at the size limit if a file grows during the read.

## Stream

One SSE stream per tab, following the shared active session. On connect it emits `hello`, `live`,
`activity`, `active`, and a `snapshot` of the active session; reconnect repeats this (no replay).
Each event is `event: <name>` with a JSON `data`, serialized once and shared by every stream.
Keep-alive comment every 15 s. Entry events are coalesced to the TUI's frame interval. A tab that
connects between two coalesced deliveries can receive an `entry` its snapshot already holds; the
upsert rule makes that harmless. Likewise, a tab that connects while the other tabs hold an older
activity list receives the list again at the next delivery.

Draft text in snapshots and `draft` events is sent in full because clients use it to synchronize
saved edits.

| Event | Data |
| :-- | :-- |
| `hello` | `{ protocol_version, client_hint }` |
| `live` | `{ active: string \| null, sessions: SessionSummary[] }` on connect and whenever any summary changes |
| `activity` | `{ sessions: SessionActivity[] }`, every live session in the order of `live`, on connect and whenever any session's activity changes; closed sessions leave the list |
| `active` | `{ session }`, always followed by a `snapshot` |
| `snapshot` | `SessionSnapshot` |
| `entry` | `{ session, entry: WireEntry }` upsert by `(id, revision)`; clients drop anything older than what they hold |
| `status` | `{ session, status: TransientStatus \| null }` |
| `turn_timer` | `{ session, elapsed_ns: number \| null }` when the oldest running turn changes: how long the new one has run, or `null` once none runs. Clients count on with a monotonic clock, as the composer's turn timer |
| `queue` | `{ session, items: QueuedPrompt[] }` |
| `draft` | `{ session, rev, text, origin }`, origin is `"terminal"` or `"web:<client>"` |
| `settings` | `{ session, model, effort, reasoning_mode, speed }` |
| `context` | `{ session, active_tokens, window_tokens }`, the composer's context budget |
| `subagents` | `{ session } & SubagentRoster`, coalesced to the frame interval |
| `subagent_entry` | `{ session, agent, entry: WireEntry }` for the active session; same upsert rule as `entry` |
| `closed` | `{ session }` |
| `workspace` | `{ version, checkout }` the working tree of `checkout` (an absolute path) changed; marks that checkout's diff stale |
| `monitor` | `MonitorState` whenever it changes (a check started, finished, or failed, or the availability changed), in the order the states were taken; not part of the connect sequence. See "Monitor" |

```ts
type SessionSummary = {
  id: string; title: string; model: string;
  title_pending: boolean;     // title is the first prompt's fallback while a generated title is expected
  workspace: string;          // the directory the session's agent runs in
  state: "idle" | "running" | "error";
  unread: boolean;            // finished or errored while not active
  has_draft: boolean;
  last_activity_unix_ms: number;
};
// What a session is doing now, for the Monitor. Computed from the projection the server already
// holds for every live session; recomputed only when a publication touched that session.
type SessionActivity = {
  session: string;
  status: TransientStatus | null;  // as in the snapshot
  turn_elapsed_ns: number | null;  // how long the oldest running turn has run, as of this event;
                                   // count on with a monotonic clock. Time passing alone sends no event
  prompt: { text: string; at_ms: number | null } | null;  // newest visible prompt, at most 600 characters
  // Newest complete visible assistant message; a message still streaming is not reported. Over
  // 1,800 characters it is cut to its first 1,200 and last 500, joined by "\n…\n".
  message: { text: string; commentary: boolean; at_ms: number | null } | null;
  // Newest visible top-level tool call (a Code Mode cell with several calls counts as one).
  tool: { name: string; summary: string; state: "running" | "succeeded" | "failed";
          at_ms: number | null; outcome: ToolOutcome | null } | null;  // summary and outcome as in WireEntry
  turn: { tools: number; failed: number; patches: number };  // top-level calls since the newest prompt
                                   // (the whole transcript without one); patches: succeeded apply_patch calls
  plan: { done: number; total: number; current: string | null } | null;  // newest update_plan call
  error: string | null;            // the transient error, else the newest visible entry when it is an error;
                                   // at most 600 characters
  agents: { running: number; total: number };  // running counts pending and running subagents
  queued: number;                  // prompts waiting in the queue
  context: { active_tokens: number; window_tokens: number } | null;
};
type SessionSnapshot = {
  session: string; title: string; title_pending: boolean; model: string; workspace: string; effort: Effort;
  reasoning_mode: "standard" | "pro"; speed: Speed;
  entries: WireEntry[]; status: TransientStatus | null; queue: QueuedPrompt[];
  draft: Draft; running: boolean;
  turn_elapsed_ns: number | null; // how long the oldest running turn has run, as of this frame
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
// `at_ms` is the Unix time in milliseconds of the record that created the entry, or null when
// no record created it (agent message threads).
type WireEntry = { id: number; revision: number; parent: number | null; at_ms: number | null } & (
  | { kind: "user"; text: string; images: number } // the i-th image replaces the i-th "[Image #N]" marker
  | { kind: "assistant"; text: string; complete: boolean; commentary: boolean }
  | { kind: "reasoning"; text: string }
  | { kind: "tool"; name: string; summary: string; state: "running" | "succeeded" | "failed";
      duration_ns: number | null; elapsed_ns: number | null; // elapsed so far, while running
      substeps: string[]; child_count: number; has_detail: boolean;
      outcome: ToolOutcome | null; stats: PatchStats | null;
      significance: "routine" | "landmark" }
  | { kind: "directed_message"; from: string; to: string; body: string; delivery: string; // the latest message
      thread: number; messages: DirectedMessage[] }
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
type ToolOutcome = { exit_code: number | null; tail: string[]; summary: string | null };
type PatchStats = { files: number; additions: number; deletions: number };
```

A tool entry's `outcome`, `stats`, and `significance` let a client show how a call ended without
fetching its `ToolDetail`. The server computes them each time it projects a new revision of the
entry.

- `outcome` is set once a tool has finished and its result carries command output: an `output`
  string or an `exit_code` field. Shell commands (`exec_command`, `write_stdin`, and `!` shells
  from the terminal) report this way. It is null while the tool runs and for tools without
  command output.
  - `exit_code` is the process exit code, or null when the process has not exited or was killed.
  - `tail` holds the last 8 output lines that contain visible text. ANSI escapes and control
    characters are removed, a line rewritten with carriage returns keeps only its final text, and
    each line is cut to 200 characters (ending in `…` when cut).
  - `summary` is a short test or build result when the output contains one, such as
    `"17 passed, 1 failed, 2 skipped"`. The server recognises cargo test (summed over every test
    binary), nextest, pytest, Jest, bun test, and go test (`"1 packages passed, 1 failed"` when
    only package lines are present). Without test results, cargo and tsc compiler errors and
    warnings give `"3 errors, 2 warnings"`. Otherwise it is null. Failed counts include errors,
    and skipped counts include ignored and todo tests.
- `stats` is set for `apply_patch` calls that have not failed. `files` counts the Add, Update, and
  Delete headers of the patch envelope; `additions` and `deletions` count its `+` and `-` lines.
  The envelope does not contain a deleted file's lines, so they are not counted.
- `significance` says whether a call may fold into a run of routine work (`"routine"`) or should
  keep a row of its own (`"landmark"`). A call is a landmark when it failed, when it is
  `apply_patch`, `update_plan`, `spawn_agent`, `send_agent_message`, `close_agent`, or
  `interrupt_agent`, when it is a `memory` call that puts or deletes, or when it is a Code Mode
  `exec` cell with a landmark among its calls. Everything else is routine: shell commands, reads,
  searches, web lookups, agent waits and listings, memory scans and reads, Code Mode cells that
  only made routine calls, and unknown tools. A running call is routine unless its tool alone makes
  it a landmark, and it becomes a landmark if it fails. A change to the name, arguments, or state
  of one of a cell's calls is a new revision of the cell, so a cell's class follows its calls.

Clients must render unknown entry kinds as a muted generic row.

## Commands

`POST /api/cmd` with `X-Tact: 1` and the body `{ client, cmd: <name>, args: { ...body } }`
(`client` is a random per-tab integer; commands without a body omit `args`). The body deserializes
straight into `protocol::Command`. There is no per-command route. Success is 200 `{}`, except
`open_session` returns `{ session }` and `set_draft` returns `{ rev }`. The command is acknowledged
only after the loop applied it and published its changes. The draft receipt identifies the saved
text even when no text changed or its stream echo was missed.
`open_session` takes its variant as `args`, for example `{ cmd: "open_session", args: { new: {} } }`.

| Command | Body | Keyboard equivalent | Refused |
| :-- | :-- | :-- | :-- |
| `set_draft` | `session, text` | typing | never (last writer wins) |
| `submit` | `session, rev, queue?` | Enter in the composer, or Shift+Tab or the Queue button with `queue: true` | `draft_changed` if the draft moved; `not_available_remotely` for terminal-local slash commands such as `/copy`. While a turn runs the prompt steers it; with `queue: true`, or while a steer is still being applied, it waits in the queue instead. |
| `interrupt` | `session` | cancel-all | `nothing_running` |
| `steer` / `dequeue` | `session, queue_id` | queue panel | `nothing_running` / `unknown_queue_item` (409) for a consumed item |
| `compact` | `session` | Actions: Compact | `turn_running`, `queue_not_empty` |
| `set_model` / `set_effort` / `set_reasoning_mode` / `set_speed` | `session, model\|effort\|mode\|speed` | pickers | see Feature parity |
| `activate` | `session` | focus / Sessions action | `unknown_session` |
| `open_session` | `{ new: { model?, workspace? } }` \| `{ resume: { session } }` \| `{ fork: { session } }` | Sessions action | `too_many_sessions`, `unknown_session`, `session_locked`, `turn_running` (fork) |
| `close_session` | `session, force?` | close pane | `turn_running` without `force` |

Opening or activating a session from either window makes it the shared active session.

## Feature parity

Everything the terminal can do is reachable from the web through three generic paths, so a new
feature is an enum variant plus its loop-side handler and never a new route:

- **Commands** (`protocol::Command`, `POST /api/cmd`) change state and obey the same preconditions
  as the keypress, evaluated by the loop on the target pane.
- **Queries** (`protocol::Query`, `POST /api/query`) read data on demand. The body is
  `{ query: <name>, args?: { ... } }` with `X-Tact: 1`; the reply is the bare payload below.
  The server forwards every query to the loop, which answers from state it owns or by calling,
  off the loop thread, the same UI-agnostic function the terminal picker uses. Refusals use the
  common error codes.
- **Publications** (`protocol::Publication`, stream events) carry state that changes over time.

Data is computed once, in modules both front-ends call: `search` (fuzzy ranking, workspace
paths), `core::extensions::SkillMatches`, `core::session` (history pages, recent prompts),
`core::context` (diagnostics), `core::subagent_roster` (the subagent tree), `app::model` (the model
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
| `set_reasoning_mode` | `session, mode: "standard" \| "pro"` | `invalid_request` if the model does not list the mode, or after the first prompt: a session's mode is fixed when it is created, so a new thread is recreated to apply the choice |
| `set_speed` | `session, speed: Speed` | never; the model may run a lower tier |
| `edit_queued` | `session, queue_id, text` | `unknown_queue_item` (409) for a consumed item |
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
| `history` | `query?, cursor?` | `{ sessions: PersistedSession[], next_cursor: string \| null }` (resumable persisted sessions of the focused session's repository, any of its checkouts, newest first, pages of 50) |
| `files` | `query?` | `{ paths: string[] }` (at most 50; directories end in `/`) |
| `skills` | `query?` | `{ skills: { name, description }[] }` |
| `recent_prompts` | `session, scope?: "global" \| "current_session", query?` | `{ prompts: { text, recorded_at_unix_ms, session_id, workspace }[] }` |
| `workspaces` | `session?` | `{ default: string, checkouts: Checkout[], recent: string[] }`: the checkouts of the session's repository (the default workspace's without a session), main checkout first, and other workspaces live sessions run in. Answered by the server without the terminal loop |
| `context_diagnostics` | `session` | `ContextDiagnostics`: window, auto-compact limit, latest usage, compactions, an estimated per-category `breakdown` of the latest call's input (`null` until a call reports usage and for models that report none), and the per-call `history`. Counts, sizes, and tool names only, never content |
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
// One message of a conversation between agents. `from` is null for the root session.
type DirectedMessage = {
  id: number; from: number | null; to: number;
  purpose: "delegate" | "coordinate" | "finding" | "question" | "reply";
  priority: "deferred" | "urgent"; in_reply_to: number | null; body: string;
  delivery: "admitted" | "delivered" | "failed" | "unknown";
  detail: string | null; // "started" | "queued" | "steered", or the failure
};
type Checkout = {
  path: string; name: string; label: string; kind: "git" | "jj"; head: string | null;
  changed_files: number | null; current: boolean; missing: boolean; touched: boolean;
};
type PersistedSession = {
  session_id: string; started_at_unix_ms: number; model: string; effort: Effort;
  reasoning_mode: "standard" | "pro"; workspace: string; preview: string;
  title_pending?: true;       // preview is a fallback while a claimed title is being generated
};
type ContextDiagnostics = {
  model_window_tokens: number; auto_compact_token_limit: number | null;
  active_tokens: number | null;
  usage: { input: number; cached_input: number; uncached_input: number; output: number; total: number } | null;
  continuation: "full_context" | "previous_response" | null; prompt_cache: boolean | null;
  compactions_started: number; compactions_completed: number;
  last_compaction: { trigger: "automatic" | "manual"; started_at_unix_ms: number;
    completed_at_unix_ms: number | null; before_tokens: number | null; after_tokens: number | null } | null;
  breakdown: {                          // null until a call reports usage, and for models that report none
    input_tokens: number;               // exact: the latest call's input; categories sum to it
    categories: { kind: ContextCategory; tokens: number; items: number }[]; // every kind, in display order;
                                        // shares are estimates (each growth step is split by size)
    tools: { name: string; calls: number; call_tokens: number; output_tokens: number }[];
                                        // by descending output_tokens, at most 12, rest merged as "(other tools)"
    largest: { kind: ContextCategory; tool: string | null; turn: number; tokens: number }[]; // top 6
  } | null;
  history: { call: number; input: number; cached: number; output: number; after_compaction: boolean }[];
                                        // oldest first, at most the 120 newest calls
};
type ContextCategory = "prefix" | "user" | "assistant" | "reasoning" | "tool_calls" | "tool_output" | "compacted" | "other";
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
    model: string; thinking: Effort; reasoning_mode: "standard" | "pro";
    status: { state: "pending" | "running" | "interrupted" | "closing" | "closed" }
      | { state: "completed"; output: unknown } | { state: "failed"; error: string };
  }[];
};
```

## Review

The review routes (`bin/tact/src/web/review`) diff a checkout and run agent operations on the
selected range. The browser types live in `web/src/review/protocol.ts`.

A review is kept per checkout. `GET /api/review` prepares it on first use. Every capture of the
checkout is a **generation**; `POST /api/refresh` captures it again as the next generation, which
cancels work started for the old one and makes requests that name the old generation fail with
`stale_snapshot`. A **range** is `{ from, to }`, indices into `range_targets` (trunk merge base,
each commit, then the working tree), with `from < to`.

Every request may name a `checkout` (an absolute path); without one the target is the session's
workspace, or the default workspace without a session. See [workspaces.md](workspaces.md) for which
checkouts are accepted. Overviews, AI reviews, and question threads belong to a live `session`.
They run on that session's worker as clean-context prompts, are refused while any session has a
turn running (`turn_running`), and are cancelled when the session closes.

| Endpoint | Body | Response |
| :-- | :-- | :-- |
| `GET /api/review?session=&checkout=` | none | `Review` |
| `POST /api/refresh` | `generation, session?, checkout?` | `Review` for the next generation |
| `POST /api/range` | `generation, range, session?, checkout?` | `ReviewPage` |
| `POST /api/overview` | `session, generation, range, instructions?, checkout?` | `{ generation, selected_range, overview_mdx, instructions }` |
| `POST /api/ai-review` | `session, generation, range, checkout?` | `{ generation, selected_range, comments: AiComment[] }` |
| `POST /api/question` | `QuestionRequest` | `{ generation, selected_range, answer }` |
| `POST /api/questions` | `generation, session, checkout?` | `{ generation, questions: Question[] }` |
| `POST /api/question/cancel` | `operation_id, generation, range, session?` | 204 |
| `POST /api/review/compose` | `generation, range, decision, summary?, comments?, session?, checkout?` | `{ markdown }` |

`Review` is `{ protocol_version, generation, title, repository, checkout: { path, name, label,
kind }, trunk, range_targets, default_range (the full branch), page: ReviewPage, overview, questions, turn_running }`.
A `ReviewPage` is `{ generation, selected_range, full_context, patch, repository, scope, base }`;
`patch` is a git patch with full file context. `overview` and `questions` are the named session's
selected overview and question threads.

Requests for the same overview (session, generation, range, and instructions) share one agent run,
and a finished overview is cached. Instructions are trimmed and limited to 8 KiB. AI review
comments are `{ path, side, start_line, end_line, body }`, where `side` is `additions` or
`deletions` and `body` starts with a severity label `[P0]` through `[P3]`; a reply with an
unanchored comment fails as `ai_review_failed`. A question names its thread and operation ids and
an anchor (`path`, `side`, `start_line`, `end_line`), and carries the whole thread, alternating
`reviewer` and `agent` messages and ending with the reviewer.

`compose` turns a decision (`approve` or `request_changes`) into the Markdown the browser writes
into the session's draft (**Send to chat**). Every comment must lie inside a hunk of the reviewed
patch. When the reviewed checkout is not the session's workspace the text names it in a
`**Checkout:**` line.

The `workspace` stream event reports that a checkout's content changed. The server polls each
kept checkout while any stream is connected.

### Review errors

Review failures are JSON `{ code, error, retryable, snapshot_valid }`. `retryable` says whether
repeating the request can succeed; `snapshot_valid` says whether the client's current generation
and range are still usable.

| Code | Status | Retryable | Snapshot valid | Meaning |
| :-- | :-- | :-- | :-- | :-- |
| `stale_snapshot` | 409 | yes | no | The generation or range is gone, or the checkout changed; reload |
| `workspace_changed` | 422 | yes | no | The checkout could not be captured |
| `workspace_changed` | 422 | no | no | The directory is not a git or jj checkout |
| `workspace_changed` | 503 | yes | no | The checkout's version could not be read |
| `invalid_range` | 422 | no | yes | The range cannot be captured |
| `invalid_overview_instructions` | 400 | yes | yes | Instructions exceed 8 KiB |
| `overview_failed` | 422 | yes | yes | The agent failed or returned an empty or oversized overview |
| `ai_review_failed` | 422 | yes | yes | The agent failed or returned invalid or unanchored comments |
| `question_failed` | 422 | yes | yes | The agent failed or returned an empty or oversized answer |
| `invalid_thread` | 422 | no | yes | Malformed, unanchored, or out-of-order question thread |
| `invalid_comment_anchor` | 422 | no | yes | A composed comment is malformed or outside the patch |
| `turn_running` | 409 | yes | yes | A session has a turn running |
| `operation_cancelled` | 409 | yes | yes | The operation was cancelled or superseded |
| `session_cancelled` | 409 | no | yes | The operation's session closed |
| `unknown_session` | 404 | no | yes | The session is not live |
| `invalid_checkout` | 400 | no | yes | The checkout is not in an accepted repository family |
| `failed` | 500 | yes | yes | The server failed independently of the request |

## Monitor

The Monitor shows every live session of this process at once, to help the developer manage open
tasks. It has two layers:

- The `activity` stream event, which the server derives from the projections it already holds
  (see "Stream"). It costs no model calls.
- The **monitor** model (`bin/tact/src/web/monitor`), which periodically asks the model set by
  `[web.monitor]` (Tact's default model unless set) to **check** all live sessions. For each
  session it writes a note: the goal the agent is pursuing, up to four status blocks it composes
  from a typed vocabulary to show status and progress in the form that fits the work, an attention
  level with its evidence, and an optional message to steer or unblock the agent. It calls the
  model directly, without tools, an agent, or a session worker, through the same helper as the
  title generator (`core::direct`).

| Endpoint | Body | Response |
| :-- | :-- | :-- |
| `POST /api/monitor` | `{ refresh?: boolean }` | `MonitorState`; declares interest and may start a check |

The route sits behind the usual guard (cookie, and `X-Tact: 1`). Malformed bodies are
`invalid_request`. The `monitor` stream event carries the same state whenever it changes. POST
replies and stream events are not versioned together. The browser drops a reply when a stream event
replaced the state while the request was in flight, but a stream event delayed behind a newer reply
can briefly show older state until the next event or renewal.

```ts
type MonitorState = {
  availability: "ready" | "disabled" | "unavailable";
  detail: string | null;          // why it is disabled or unavailable, as a sentence; null when ready
  running: boolean;               // the model is checking the sessions
  error: string | null;           // the last check's failure, cleared by the next success
  interval_seconds: number;
  check: MonitorCheck | null;     // the last successful check, or null before the first
};
type MonitorCheck = {
  as_of_ms: number;               // when the sessions were read
  generated_at_ms: number;        // when the model's reply arrived
  model: string;                  // model id, labelled through the models catalog
  sessions: SessionNote[];        // notes for sessions that are still live, in input order
};
type Attention = "needs_input" | "stuck" | "off_track" | "ready" | "ok";
type SessionNote = {
  session: string; attention: Attention;
  goal: string | null;            // the task the agent is pursuing, at most 100 characters;
                                  // null only when the session has no task yet
  health: number | null;          // 0-100 integer: how well the work is going toward the goal;
                                  // null when there is no task yet
  previous_health: number | null; // this session's health in the previous successful check,
                                  // filled in by the server (null when there is none)
  blocks: StatusBlock[];          // composed by the model, most important first, at most 4
  reason: string | null;          // the evidence, at most 300 characters; null when attention is "ok"
  suggestion: string | null;      // at most 600 characters, written to the agent
};
type Tone = "neutral" | "success" | "warning" | "danger";
type StatusBlock =
  | { type: "text"; text: string; tone: Tone }   // a finding, blocker, risk, or result; text <= 240
  | { type: "progress"; label: string; done: number; total: number; unit: string | null }
                                  // integers, 0 <= done <= total, 0 < total <= 1,000,000; label <= 60, unit <= 20
  | { type: "metric"; label: string; value: string; previous: string | null; tone: Tone }
                                  // label <= 60; value and previous are short figures ("0", "42 ms",
                                  // "3/3") of at most 24 characters
  | { type: "checklist"; title: string | null;   // a planned sequence in planned order; title <= 60
      items: { label: string; state: "done" | "active" | "pending" | "failed" | "skipped" }[] } // 1-8 items, label <= 100
  | { type: "history";            // recent steps, newest first, 1-4 items
      items: { action: string; outcome: "worked" | "failed" | "running" | "unclear"; detail: string | null }[] }
                                  // action <= 100, detail <= 120
  | { type: "question"; text: string; options: string[] }  // text <= 300, at most 5 options of <= 80
  | { type: "code"; label: string | null; text: string }   // label <= 40, text <= 240 and at most 4 lines
  | { type: "live"; source: "message" | "plan" | "tool" | "prompt" | "agents" | "error" };
                                  // the browser fills it from the session's live data
```

Scheduling. The browser calls `POST /api/monitor` when the Monitor becomes visible and every 20 s
while it stays visible; each call holds a 60 s lease. While a lease is live, a check starts when
the model is available, none is running, at least two sessions are live (the Monitor compares
sessions, so one live session is never checked, even on refresh), and either:

- the call asked for `refresh` and the last check started at least 10 s ago, or
- the sessions changed since the last check read them and the last check started at least
  `interval_seconds` ago (or none ever ran).

"Changed" compares the live set and, per session, a transcript revision that grows with every
record applied to the session's transcript (so a change to any entry counts, not only to the newest
one), the running state, the queue length, the unread flag, and the subagents' statuses, so time
passing alone never counts. A check counts as having read the sessions only once it asks the model
about them: one skipped because sessions closed while it started leaves them unread. The server
applies the automatic rule every 5 s between calls as well. Only one check runs at a time, and a
running one, whether it is resolving its model or awaiting the reply, is abandoned when the server
stops. When no browser shows the Monitor, the model is never called. Availability is checked on
every call and, while a lease is live, on every 5 s tick, so a reload that enables the monitor or
makes its model usable applies without waiting for the browser: `disabled` when
`[web.monitor] enabled = false`, `unavailable` when the chosen model has no usable credentials
or is access-gated (the detail names the model and asks to set `[web.monitor] model`). A change of
availability is broadcast to every browser even when no check starts; a renewal that changes
nothing is not. Because availability is refreshed only on browser requests and on these periodic
checks, right after a reload a lookup that started under the previous configuration can briefly
show its status until a lookup of the current configuration completes.

Each check reads the configuration in force when it starts: the terminal loop hands every reloaded
configuration to the web server, so `enabled`, `model`, `thinking`, and `interval_seconds`
apply from the next scheduling decision and check without a restart. The request uses the chosen
effort as sessions do: the reasoning effort for OpenAI models, and adaptive thinking at that effort
for Claude models. A model that does not support the effort runs at its catalog default.

The model's input is a JSON document: the time, the previous check's notes (session, attention,
goal, and health) when the same model made that check (a different model judges afresh), and per session its title, model, workspace, state, unread and draft flags (never the
draft's text), queue length, turn time, status, plan, subagents, and a transcript tail. The tail
holds the newest visible entries, including the calls a Code Mode cell made (no reasoning, tool
output only as an exit code, a test summary, or one failure line). Everything about a session
shares a budget of about 10,000 bytes per session and 80,000 overall, measured as the JSON the
model receives. The session's two newest prompts are always included and get their share first,
shortened to at most half of what its fixed fields leave; plan steps (in planned order) and
subagents may then take up to half of the rest, dropping the ones that do not fit; and the other
transcript entries fill what remains. With more than 16 live sessions, the 16 most
recently active are read. Every string has a length limit. Transcripts and subagent lists are newest
first; plan steps keep their planned order. Each transcript item carries a sequence number `n`,
from 1 for the oldest item shown to the item count for the newest.

The reply must be one JSON object, `{ sessions: [...] }`. The instructions describe every block
type with examples by kind of work (a metric of failing tests, progress through a migration, a
question with the agent's options, a live tool for a long command, a checklist or live plan, a
warning text for a risk) and require every input session exactly once, even with little to say
(attention "ok", no blocks, the goal from the prompt). Blocks must not restate the title, goal, or
reason, every figure must come from the input, and each history step cites as `at` the `n` of the
newest transcript item it covers. A history step's outcome is whether that step's own result
succeeded (a test run that failed stays "failed" even when a later step fixed it), and a question
keeps the agent's options in the order and wording it offered them, each only the choice itself
without the agent's explanation of it.

Health is separate from attention: a session can be going well and still wait on an answer. Health
measures progress toward the goal, not activity and not whether a run finished. The instructions
use these bands: 90-100 when the goal is achieved or verified; 70-89 for normal progress without
unresolved problems; 40-69 for slow or uncertain work, minor failures being handled, retries, or a
finished run that honestly reports the goal blocked or impossible (lower in that band when nothing
useful came of it); 20-39 for repeated failures, a loop, scope drift, or a risky approach; and 0-19
for work that is still flailing, broken, or off track with no path forward. A report that the goal
was not achieved is never 90-100, and such a run is `needs_input` rather than `ready`. A session
waiting on the developer is scored on its progress so far: sound work with a sensible question at
the right point is 70-89. Each check scores health independently from the current evidence; the
previous score is context for continuity only, kept when the new score is within 10 points of it
and the evidence has not changed. The middle of a band is preferred when unsure, and only a session
with no task, or one that has just started with no evidence of progress yet, has no score. A note
must be consistent: a warning or danger block, or a concern in the reason, shows in its attention
and health, and when attention is `stuck` or `off_track` the first block explains the concern
rather than a success-looking metric or text. Changing production code or expected values only to
make a test pass (special-casing the test, weakening assertions, editing tests the developer said
not to touch, or behavior that contradicts the task) is `off_track` with a health of 20-39 even
when the suite passes, and the reason says what to check. The server keeps a health only when it is
a number that rounds to an integer from 0 to 100 (otherwise null) and sets `previous_health`
itself from the previous successful check, whichever model made it.

The server reads the reply leniently and validates each block on its own, so one bad block never
fails the check:

- The reply object is the first complete JSON value starting at the first brace, so a fence or
  commentary around it is ignored, and so are stray characters after it (Luna sometimes repeats the
  closing brackets of an otherwise complete reply). Line breaks and other control characters
  written raw inside strings, as in a multi-line code block, are read as if escaped; one right after
  a backslash keeps that backslash as a literal one. Notes for
  sessions the input did not describe
  are dropped, a session the reply leaves out simply has no note, and notes are kept in input order.
  Unknown attention levels read as `"ok"`, and an "ok" note carries no reason.
- Unknown or malformed blocks are dropped and the first 4 valid ones kept; a missing or malformed
  block list is empty. Strings are trimmed and cut to the limits above. An unknown tone reads as
  `"neutral"`, an unknown checklist state as `"pending"`, and an unknown history outcome as
  `"unclear"`. Required text that is empty, a progress block outside its bounds or with
  non-integers, a checklist or history without items, and an unknown live source drop the block.
  Empty question options are dropped, and the rest are ordered by where each first appears in the
  agent's newest message (options it does not contain follow in the model's order). Code is cut to
  its first 4 lines. A metric value or previous value longer than 24 characters, or neither text
  nor a number, drops the block rather than being cut.
- A progress or metric block is dropped when any run of digits in its `done`, `total`,
  `value`, or `previous` is not a run of digits in what the model read about that session
  (excluding the transcript items' sequence numbers), so it cannot present an invented figure.
- Models do not reliably keep the requested order, so history steps are sorted newest first by
  `at` (ties keep the model's order); steps whose `at` is missing or not one of the session's
  sequence numbers follow in the model's order, and the first 4 are kept. `at` is not sent to
  browsers.

Anything else fails the check with "The monitor's reply could not be read: <reason>." The reason
names the failure without repeating the reply, which may quote session content: the reply was
empty, it held no JSON object, its JSON ends early, its JSON is invalid at a given line and column,
or its object has no sessions list; each with the reply's length in characters where it helps.

Data handling. The monitor sends excerpts of every live session's transcript (prompts, messages,
tool summaries, errors, plans) to its own model, which may belong to a different provider than a
session's model, as session titles already do. Turn it off with `[web.monitor] enabled = false`,
or choose its model with `model`. Its instructions tell the model that all session content is data
to assess, never instructions to follow.


## UI

The web UI is chat first. Its styling takes the palette and rhythm of the TUI theme and is
mobile-first.

- **Sidebar.** **+ New chat**, the **Monitor** item, **Live** sessions with state markers (pulsing
  running, idle, accent unread, pencil draft), **History** with search, and an instance switcher.
  Live sessions are ordered by recent activity; **Pin to top** in a row's menu keeps a session
  above the rest (remembered per browser). On desktops the sidebar folds away (header button or
  Cmd/Ctrl B, remembered per browser); on narrower windows it is a drawer.
- **Tabs.** **Chat** holds the transcript and the shared composer. **Review** holds the Pierre
  diffs, live while the agent edits, and an **Overview** sub-tab. Cmd/Ctrl . switches between them,
  and from the Monitor it returns to Chat.
- **Monitor.** While at least two sessions are live, the sidebar item (or the **Monitor** palette
  command) replaces the chat with a dashboard of every live session of this process: one grid of
  compact tiles that fills the window, with the sessions that need you first, then those working,
  then idle ones, each group newest first; the toolbar's **Order** switches to **Health**, which
  puts the sessions going worst first. The item sits above the session list, outside its scrolling,
  and counts the sessions that need you and those running; when fewer than two sessions remain live,
  it disappears and the view returns to the chat. A toolbar filters the tiles (**All**, **Needs
  you**, **Working**, **Idle**, each with its count) and shows when the monitor model last checked
  the sessions and which model it is, with a refresh button. A session needs you when it failed, the
  monitor reads it as waiting on you, stuck, or off track, it has a reply nobody has read (**New
  reply**), it is retrying, or its running turn has shown no activity for 10 minutes with no tool
  call or subagent in flight. Every tile has the same frame: its title, one badge, and a gauge of
  the monitor's health score (0 to 100, how well the work is going, with an arrow when it moved by
  10 or more since the previous check), what the session is doing now with the turn's duration or
  the time since its last activity, and its actions. Between them, the monitor chooses how to show
  the session's status from a set of blocks: a sentence, a progress bar, a measurement with its
  previous value, a checklist, its latest steps newest first with whether each worked, a question
  with the agent's options, a short literal such as a command, or live data that updates in real
  time (the last message, the plan, the call in flight, the prompt, subagents, or the error). A
  model block written before the session's latest activity is marked **earlier**. A session's error
  always takes the tile's body, and without a current note the tile shows one piece of evidence
  chosen by its state: the end of its last message when it is idle (where a question to you sits),
  or, while it runs, the monitor's goal or the prompt. A question's options put that answer in the
  reply box when the session is idle and the question is current; they never send it. **Details**
  holds every block in full with the monitor's goal, reason, and suggested message, then the prompt,
  the latest reply as the Monitor received it (its opening and ending when it is long; the chat has
  the whole message), the plan, counters, and the model and workspace. Clicking a tile opens the
  session's chat. **Reply** (**Steer** while a turn runs) sends a prompt from the tile, and its box
  can insert the monitor's suggestion for editing. The **…** menu opens the session, stops a running
  turn after confirmation, or closes a session that is not running, which moves it to History. A
  session with an unsent draft offers **Open** instead of Reply, so a draft the Monitor knows about
  is never overwritten; a draft started elsewhere after the tile was drawn follows the usual rule
  that the latest write wins. The active session's reply goes to the composer. The Monitor asks for
  checks and ticks its clocks only while it is on screen.
- **Transcript.** Prompts and messages render as Markdown with highlighted code and KaTeX math
  (`$…$`, `$$…$$`, `\(…\)`, `\[…\]`, or a `math` fence). A `mermaid` fence renders as a
  diagram; Mermaid loads with the first one, a diagram opens enlarged on click, and one that
  cannot be drawn shows its source under a note. GitHub alerts (`> [!NOTE]` and the like),
  footnotes, and task lists render as on GitHub. Code blocks, display math, and diagrams have a
  copy control, and code blocks over 40 lines start folded. A code span naming a changed file
  (`src/app.ts:12`) opens it in **Review** at that line. Headings in messages have a control that
  copies a link to the section. Apply-patch calls render as truncated Pierre diffs;
  shell and code calls render as terminal blocks. Routine work that succeeded is folded; failures,
  edits, plans, and agents always get their own row. Two or more consecutive routine calls (see
  `significance`) fold into one row, with the thoughts between them. The row reads **Ran** when the
  run includes a shell command or Code Mode cell and **Explored** otherwise, and counts what the
  run covered ("4 commands, 1 code cell"). While one of its calls runs, the row names that call, as
  **Running** for a command or cell and **Exploring** otherwise, and the row is open so the live
  call stays in sight. Toggling a row inverts that default, and the choice stays with the row as
  the run grows.
- **File viewer.** A Markdown link to a local file (`[report](/Users/me/scratch/report.md)`,
  `[guide](docs/guide.md#setup)`, or `[main](src/main.rs:42)`) opens the file in a new tab at
  `./?file=<path>&session=<id>&line=<n>#<heading>` instead of a path on the server. Relative paths
  are the session's workspace's. Percent-encoding is decoded, and a trailing `:line` or `:line:column`
  is the line to show. Use `./README:12` for a bare filename without an extension.
  Markdown files render as in the transcript, with a **Source** view of numbered
  lines, and a fragment scrolls to the heading it names. Other text files show as numbered lines.
  Source views use pages of 10,000 lines to keep large files responsive. A line link opens its page.
  Links and images inside a viewed file resolve against its directory and open further viewer
  tabs. Fragment-only links in any rendering scroll to the heading they name. A file that is missing,
  not a regular file, larger than 2 MiB, binary, or unreadable shows why instead.
- **Prompt minimap.** One tick per prompt on the chat's right edge; longer prompts draw longer
  ticks and the current one is highlighted. Pointing at it lists prompt previews, choosing one
  scrolls there, and sessions with more than 12 prompts page through them. Under the current
  prompt, the list also shows its answer's headings.
- **Images.** Clicking an image opens it over the page with every image of the transcript in
  order: the arrow buttons or the Left and Right keys step through them, the wheel, the zoom
  buttons, + and -, or a double click zoom in about the pointer, dragging pans, and 0 fits the image
  again. On touch screens, a swipe at fit size steps to the next image.
- **Subagents.** A hierarchy graph (Active or All, Active by default) fills a popup. Selecting an
  agent opens its transcript beside the graph with the chat's renderer; the back button returns to
  the full graph. A chip above the composer reports running subagents and opens the popup.
- **Composer.** `/` at the start of the draft lists the actions that apply to the session's current
  state: compaction and handoff need an idle, started session; the model and Pro mode are offered
  only before the first prompt. Esc interrupts a running turn only when pressed twice; the first
  press asks for confirmation and any other input cancels it, as in the terminal.

The terminal's Actions menu has **Show QR code**, which draws the sign-in link as a Unicode QR code
(black on white, whatever the theme) for a phone to scan. Like the web version it refuses an
address that only this computer can reach, so set `web.tailscale` or `web.public_url` first. The
credential is only inside the code and is never shown as text.

## Remote access

The server listens on `127.0.0.1` by default and speaks plain HTTP, so reaching it from a phone or
another machine takes a tunnel. The token in the login URL is the only credential. Tact can run the
tunnel for you on Tailscale; for anything else you run it and tell Tact its address.

### Tailscale (managed)

```toml
[web]
tailscale = true
```

The web interface starts and works locally exactly as without the setting; Tailscale is not touched
at startup, so using the browser on this machine never involves it. It is only used when you ask
for a sign-in link for another device: the terminal's **Show QR code** action, or the web
interface's "Open on your phone". Then Tact runs `tailscale status --json` to find this machine's
HTTPS name and, if it is not already running, `tailscale serve --https=443` against its own
loopback port. The QR code carries that address (`https://<machine>.<tailnet>.ts.net`) and the
token, so scanning it signs a phone in.

Tailscale is checked again every time a code is requested. If it is off or not usable, no code is
shown; the message says what is wrong, and asking again after fixing it (turning Tailscale on, for
example) picks up from there. While Tailscale is off, an earlier publication is withdrawn.

The `serve` command runs without `--bg`, so it publishes only while that command runs, and Tact
stops it when it exits. If Tact is killed without a chance to clean up (for example with
`kill -9`), the `serve` command may keep running; `tailscale serve status` shows what is
published and `tailscale serve reset` clears it.

Requirements:

- The `tailscale` command is on the `PATH` (on macOS the app bundle's copy is also tried), and the
  client is connected.
- HTTPS certificates are enabled for the tailnet, and Tailscale Serve is enabled for it. If either is
  missing, the code is refused with the reason; the local web interface is unaffected.
- Only one interface can be shared at a time. The address is the machine's HTTPS port 443, so if
  anything is already served there (another Tact, or a `tailscale serve` you ran), the request is
  refused with a message saying so. Sharing several Tact instances is not supported. Use a
  separate tunnel and `public_url` for each if you need it; Tact cannot tell whether a tunnel you
  run yourself is shared between instances.

The publication is tailnet-only (`tailscale serve`, never `tailscale funnel`).

### Other tunnels

Set `public_url` to the address your tunnel exposes, for example `https://tact.example.net`, and
run the tunnel yourself. Tact then uses that address for the QR code and the copyable link, and does
nothing else. This is also how to publish with Tailscale by hand:

```sh
tailscale serve --bg 7878
```

An SSH forward (`ssh -L 7878:127.0.0.1:7878 host`) works the same way, with the login URL pointing
at the local end. In every case a request's `Origin` must match its `Host`, which holds when the
tunnel forwards the original host name.

Share the port only with your own devices. Every connected device sees and edits the same drafts,
and the token grants the same access as a shell.

## Security notes

The token is equivalent to a shell as the user. Drafts are visible to every connected device. All
transcript and draft text is rendered as text or through the sanitizing Markdown renderer, math is
typeset from the sanitized source, and Mermaid renders diagrams in its strict mode; agent MDX runs
only in the opaque-origin sandboxed overview frame.
The file viewer receives a file's text as JSON from an authenticated route and shows it through the
same renderer or as plain text, so a viewed HTML or SVG file is never parsed as a page.
