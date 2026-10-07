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
- The login URL is `http://127.0.0.1:<port>/#k=<token>` (or `public_url` + fragment).

## Authentication

Static assets are public. Every `/api/*` route except `POST /api/login` requires the cookie.

- `POST /api/login` `{ "token": string }` -> 204 and `Set-Cookie: tact=<token>; HttpOnly;
  SameSite=Strict; Path=/`. Otherwise 401.
- Mutating requests (`POST`) must send header `X-Tact: 1`. Any request with an `Origin` header
  must have it match `Host`. The SSE request is checked the same way. No CORS headers.
- Errors are JSON `{ "code": string, "message": string }`. Codes: `unauthorized` (401),
  `invalid_request` (400), `turn_running`, `queue_not_empty`, `nothing_running`, `draft_changed`,
  `session_locked`, `unknown_session`, `too_many_sessions`, `not_available_remotely` (409/404),
  `failed` (500), plus the review codes in `web/app/protocol.ts`.

## Reads

| Endpoint | Response |
| :-- | :-- |
| `GET /api/instance` | `{ protocol_version, workspace, repository, live, running }` |
| `GET /api/instances` | `{ instances: [{ pid, port, workspace, live, running, current }] }` (siblings that answer) |
| `GET /api/models` | `{ models: [{ id, label }], efforts: string[] }` |
| `GET /api/history?q=&cursor=` | `{ sessions: [{ session_id, started_at_unix_ms, model, preview, workspace }], next_cursor }` persisted, not live |
| `GET /api/sessions/{id}/entries/{n}` | `ToolDetail` of one entry (full arguments, result, metadata, each string truncated at 256 KiB) |
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
| `settings` | `{ session, model, effort, fast_mode }` |
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
  session: string; title: string; model: string; effort: string; fast_mode: boolean;
  entries: WireEntry[]; status: TransientStatus | null; queue: QueuedPrompt[];
  draft: { rev: number; text: string }; running: boolean;
};
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

`POST /api/cmd/<name>` with JSON, `X-Tact: 1`, always including `client` (a random per-tab integer).
Success is 200 `{}` (`open_session` returns `{ session }`). The command is acknowledged only after
the loop applied it.

| Command | Body | Keyboard equivalent | Refused |
| :-- | :-- | :-- | :-- |
| `set_draft` | `session, text` | typing | never (last writer wins) |
| `submit` | `session, rev` | Enter in the composer | `draft_changed` if the draft moved; `not_available_remotely` for terminal-local slash commands such as `/copy`. Queues if a turn is running or a steer is pending. |
| `interrupt` | `session` | cancel-all | `nothing_running` |
| `steer` / `dequeue` | `session, queue_id` | queue panel | `nothing_running` / `unknown_session` for a consumed item |
| `compact` | `session` | Actions: Compact | `turn_running`, `queue_not_empty` |
| `set_model` / `set_effort` / `set_fast` | `session, model\|effort\|enabled` | pickers | same as TUI |
| `activate` | `session` | focus / Sessions action | `unknown_session` |
| `open_session` | `{ new: { model? } }` \| `{ resume: { session } }` \| `{ fork: { session } }` | Sessions action | `too_many_sessions`, `unknown_session`, `session_locked`, `turn_running` (fork) |
| `close_session` | `session, force?` | close pane | `turn_running` without `force` |

Opening or activating a session from either window makes it the shared active session.

## Review (diff and overview)

The review engine keeps its existing payloads (`web/app/protocol.ts`) with these changes:

- The diff context is per workspace and prepared lazily by `GET /api/review`, then cached by
  generation. `POST /api/refresh`, `/api/range` are unchanged.
- Overviews, AI reviews, and inline question threads belong to a session. Their requests gain
  `session`; they run through that session's worker as a clean-context auxiliary prompt, and are
  cancelled when the session closes.
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
