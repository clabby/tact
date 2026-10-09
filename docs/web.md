# Tact web interface

Every running Tact serves a web interface in the background. It is a second front-end onto the
sessions the process is running, not a separate client: the terminal and the browser share the active
session, each session's draft, queue, settings, and transcript. The TUI must stay open for the web
interface to be served, or you run `tact serve`, which runs the same sessions without a terminal.
Reachability needs a tunnel: Tact can publish itself to your tailnet (`web.tailscale`), or you run
a tunnel (Tailscale, an SSH forward, or similar) and give Tact its address (`web.public_url`). See
"Remote access". One web interface can also run sessions on other machines; see "Other machines".

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
  `not_available_remotely`, `stale`, `disabled` (409/404), `failed` (500), and for other
  machines `unknown_machine` (404), `unknown_route` (404/405), `machine_unauthorized` (409),
  `machine_unreachable` and `machine_protocol_error` (502). Commands and queries
  share one mapping (`protocol::CommandError::code`). The review routes use their own error body;
  see "Review errors".

## Reads

| Endpoint | Response |
| :-- | :-- |
| `GET /api/instance` | `{ protocol_version, workspace, repository, live, running }` |
| `GET /api/instances` | `{ instances: [{ pid, port, workspace, live, running, current }] }` (siblings that answer) |
| `GET /api/sessions/{id}/entries/{n}` | `ToolDetail` of one entry (full arguments, result, metadata, each string truncated at 256 KiB) |
| `GET /api/sessions/{id}/agents/{agent}/entries` | `{ entries: WireEntry[] }`, the projected transcript of one subagent |
| `GET /api/sessions/{id}/entries/{n}/images/{i}` | The i-th image attached to a user entry, decoded; only PNG, JPEG, GIF, and WebP are served |
| `GET /api/file?path=&session=` | A local image that Markdown refers to (absolute, `file://`, or a path relative to the session's workspace, else the default workspace), as the terminal transcript shows such images. Only files whose bytes are PNG, JPEG, GIF, or WebP and at most 25 MiB are served; anything else is 404 |
| `GET /api/sessions/{id}/agents/{agent}/entries/{n}` | `ToolDetail` of one subagent entry |
| `GET /api/stream` | Server-Sent Events, below |

## Stream

One SSE stream per tab, following the shared active session. On connect it emits `hello`, `live`,
`active`, and a `snapshot` of the active session; reconnect repeats this (no replay). Each event is
`event: <name>` with a JSON `data`, serialized once and shared by every stream. Keep-alive comment
every 15 s. Entry events are coalesced to the TUI's frame interval. A tab that connects between
two coalesced deliveries can receive an `entry` its snapshot already holds; the upsert rule makes
that harmless.

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
| `workspace` | `{ version, checkout }` the working tree of `checkout` (an absolute path) changed; marks that checkout's diff stale |

```ts
type SessionSummary = {
  id: string; title: string; model: string;
  workspace: string;          // the directory the session's agent runs in
  state: "idle" | "running" | "error";
  unread: boolean;            // finished or errored while not active
  has_draft: boolean;
  last_activity_unix_ms: number;
};
type SessionSnapshot = {
  session: string; title: string; model: string; workspace: string; effort: Effort;
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
straight into `protocol::Command`; there is no per-command route. Success is 200 `{}`
(`open_session` returns `{ session }`). The command is acknowledged only after the loop applied it.
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
kind }, trunk, range_targets, default_range, page: ReviewPage, overview, questions, turn_running }`.
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

## UI

The web UI is chat first. Its styling takes the palette and rhythm of the TUI theme and is
mobile-first.

- **Sidebar.** **+ New chat**, **Live** sessions with state markers (pulsing running, idle, accent
  unread, pencil draft), **History** with search, and an instance switcher. Live sessions are
  ordered by recent activity; **Pin to top** in a row's menu keeps a session above the rest
  (remembered per browser). On desktops the sidebar folds away (header button or Cmd/Ctrl B,
  remembered per browser); on narrower windows it is a drawer.
- **Tabs.** **Chat** holds the transcript and the shared composer. **Review** holds the Pierre
  diffs, live while the agent edits, and an **Overview** sub-tab. Cmd/Ctrl . switches between them.
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

## Other machines

One web interface, on your machine (the hub), can run sessions on other machines that run
`tact serve` (peers). You pick the machine in the interface, and the session runs there: in that
machine's checkout, with that machine's credentials, journaled on that machine. Sessions never move
between machines, and each peer keeps its own history.

### Set up a peer

On the other machine, make it reachable (`web.tailscale = true`, or a tunnel and `web.public_url`;
see "Remote access") and run:

```sh
tact serve [--workspace DIR]
```

`tact serve` runs the sessions and the web server with no terminal, always in a new session, until
SIGINT or SIGTERM. It serves only the API, so it has no web interface of its own to open. Start-up
failures are fatal and exit non-zero (the web interface disabled, a bind or token error, Tailscale
not installed); other Tailscale problems are reported on stderr and retried every 30 to 60
seconds, so a peer recovers when `tailscaled` comes back. A peer needs its own credentials
(`tact auth login`), and since one Tact per machine can hold the tailnet's port 443, it runs one
instance. It never prints its token. Run it under a supervisor: the last live session cannot be
closed, so stop the server instead.

A new session on a peer can start in the peer's default workspace (`--workspace`, else the current
directory) or in another checkout of its repository, as described in [workspaces](workspaces.md).
Run `tact serve` from the repository you want to work in.

### Link a peer from the hub

```sh
ssh peer tact web token | tact machine add peer https://peer.tailnet.ts.net
tact machine list
tact machine remove peer
```

`tact web token` prints the peer's token and nothing else. `tact machine add NAME URL` reads the
token from stdin (or prompts without echo on a terminal), checks it against the peer's
`/api/instance`, and saves it only if the peer accepts it. It refuses a token equal to this
machine's own, which is what a synced `$TACT_HOME` would produce, and it needs `--replace` to
overwrite an existing machine. A protocol version mismatch is reported but saved: the interface
refuses to use a peer whose version differs from its own, so upgrade both. Names are lowercase
letters, digits, and `-` (at most 32). The URL must be `https` with a host, no userinfo, path,
query, or fragment, and not a loopback address. Plain `http` is not accepted, because the token
would cross the network unencrypted, and a forwarded loopback port can be taken over by another
local user.

Each machine is one file, `$TACT_HOME/web/machines/<name>.toml` (directory 0700, files 0600),
holding the URL and the token. Only the command writes it, and the server reads it on every
request, so `add` and `remove` apply to a running hub. Never sync `$TACT_HOME/web` between
machines.

### Use it

When a machine is linked, the sidebar header gets a machine menu: **This machine** and one entry
per linked name. Choosing one reloads the interface at `?m=<name>`, scoped to that machine: its
sessions, workspaces, models, review, and settings. A chip in the header, a prefix on the tab
title, and the composer name the machine. New chats start on the machine you have selected. The
interface shows a card instead of the chat when the machine refused its token (link it again with
`tact machine add --replace`), is not linked, or runs a different protocol version, and it
reconnects on its own while the machine is unreachable.

### How the hub reaches a peer

The browser only ever talks to the hub, with the hub's cookie. The hub relays
`/api/m/<name>/<route>` to the peer's `/api/<route>` with the peer's token, which never reaches
the browser. `GET /api/machines` lists the linked names and nothing else.

Only these routes are relayed; everything else is a 404 and no connection is opened:

| Method | Route |
| :-- | :-- |
| GET | `instance`, `stream` (SSE), `file`, `review` |
| GET | `sessions/<id>/entries/<n>`, `sessions/<id>/entries/<n>/images/<i>`, `sessions/<id>/agents/<a>/entries`, `sessions/<id>/agents/<a>/entries/<n>` |
| POST | `cmd` (32 MiB), `query`, `review/compose`, `refresh`, `range`, `overview`, `ai-review`, `question`, `questions`, `question/cancel` |

Each request is parsed into a typed route and rebuilt from its parts, so no browser header and no
raw path or query reaches the peer, which receives its own cookie and nothing the browser sent.
Each response is rebuilt too: the hub picks the status, content type, and security headers, never
copies a peer header (no cookies, redirects, CSP, or CORS), accepts images only after checking
their bytes, and caps a response at 64 MiB. Only the event stream is passed on as it arrives, and
it ends after 45 seconds of silence. The relay uses HTTPS only, follows no redirects, ignores proxy
environment variables, and puts no time limit on requests other than the stream, because agent-run
review routes send nothing until the run finishes. A `401` from the peer becomes `409
machine_unauthorized`, never a sign-out of the hub.

### Trust

- **The hub's token is control of every linked machine.** It can start an agent on the hub, and the
  hub holds each peer's token. A stolen hub login is a compromise of all of them. Keeping the
  tokens in a file only the command writes keeps them out of the browser-editable config and out of
  `tact config`; it is not a boundary against someone who holds the hub token.
- **A peer is trusted to run its own sessions, not to touch the hub.** It cannot run script in the
  hub's page, set its cookies, redirect it, or learn its token, and one peer cannot reach another.
  Everything a peer sends is rendered as text or through the sanitizing renderer, and local state
  keyed by session ID is kept apart per machine.
- **Revoking a peer:** `tact machine remove` on the hub stops the hub using it. To make the token
  itself useless, delete `$TACT_HOME/web/token` on the peer, restart every Tact instance of that
  user there, and link it again with `--replace`. The same procedure rotates a hub token.

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
