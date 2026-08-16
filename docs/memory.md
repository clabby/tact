# Local and remote shared memory

Tact memory stores a small set of durable conclusions across sessions. Records enter model context
only after an explicit `scan` or `read`; Tact never inserts the corpus into prompts automatically.
Memory content is data, not a higher-priority instruction layer. Current user requests, system
policy, and `AGENTS.md` still govern the agent.

## Backend selection

Memory is disabled by default:

```toml
[memory]
enabled = true
```

When enabled, every runtime selects exactly one backend:

- inside any configured `memory.remote.workspace_roots` or a linked worktree from one, it uses only
  the remote backend;
- outside all configured roots, it uses only the local backend; and
- with memory disabled, it uses no backend.

An in-scope remote configuration, authentication, network, protocol, or storage failure is returned
to the caller. Tact never falls back to local memory for an in-scope workspace. This keeps a team
workspace on one authoritative corpus instead of silently splitting writes between stores.

Relative workspace roots are resolved against the directory containing `config.toml`. Tact uses
canonical path targets when deciding whether a workspace is in scope. A Git worktree is also in
scope when its repository is rooted inside a configured directory or another worktree of the same
repository is configured. The selection applies to the memory tool and the TUI memory browser.
Configuration reload changes browser availability immediately; an existing agent retains the tool
surface and instructions with which it started.

## Local memory

The local store is:

```text
<config-dir>/memory/v1.sqlite3
```

`<config-dir>` is the parent of the selected `config.toml`, including one selected by `--config`,
`TACT_CONFIG`, or `TACT_HOME`. There is one global local corpus per configuration directory. It has
no workspace, repository, branch, session, agent, or author namespace. Workspace-specific records
must state their scope in their content.

The SQLite schema remains v1, and the existing `memories` record format is unchanged. Tact lazily
adds backward-compatible allocator metadata so IDs are not reused after deletion or snapshot sync;
older builds ignore that metadata. No v2 migration is required.

## Remote memory

Configure the remote service and the workspaces that must use it:

```toml
[memory]
enabled = true

[memory.remote]
endpoint = "https://memory.example.com/"
namespace = "alice"
bearer_token = "replace-with-a-secret-token"
workspace_roots = ["/path/to/team-projects"]
```

The bearer token is stored directly in `config.toml`. Keep that file private and set its mode to
`0600`. Direct-token remote memory configuration currently requires Unix so Tact can verify these
permissions. `tact config show` and configuration debug output redact the token. Do not put the token in
logs, errors, status output, rendered configuration, or tests.

The service authenticates the token and derives its namespace and reader or writer role. The
configured namespace is an assertion checked against that identity. A writer may put, replace, and
delete only records in its own namespace. A reader can scan, read, and list but cannot mutate.
Remote keys retain the author's namespace, including records returned to another user.

Local and remote modes expose the same memory operations and meanings:

| Operation | Contract |
| --- | --- |
| `scan` | Search the selected backend. Local scans populate `ours`; remote scans make separate requests returning up to five `ours` and five `theirs` candidates. |
| `read` | Fetch complete records by exact current key. Missing or stale keys are omitted. |
| `put` | Add one atomic conclusion, or replace a known record using its key and expected version. |
| `delete` | Remove a known record using its key and expected version. |
| `list` | List records without changing scan or read telemetry. |

The agent tool uses one exact key shape throughout. Pass scan candidate keys unchanged in the
`keys` array when reading, and pass one complete `key` when deleting:

```json
{"operation":"read","keys":[{"namespace":"alice","id":7,"version":1}]}
{"operation":"delete","key":{"namespace":"alice","id":7,"version":1}}
```

Root agents may mutate; child agents may only scan and read. Reader credentials make remote
mutation unavailable regardless of agent role. Remote operations include the caller's namespace:
an author can scan, read, and list their own remote records as well as records from other authors.
Scan keeps those sources explicit: `ours` contains one response scoped to the authenticated
namespace and `theirs` contains a second response scoped to every other visible namespace. Every key
retains its author namespace. The requests are independently ranked and bounded, so one cannot crowd
the other out and their corpus-relative scores are not comparable. If either request fails, the scan
returns an error rather than a partial result or a local fallback. The agent decides which records to
read and apply. Normalized duplicate content may exist in separate namespaces because ownership
remains per author.

The remote API is an authenticated JSON/HTTP interface. Its protocol generation is
`tact_memory::VERSION`; routes and the session compatibility check use that same value:

| Method and path | Role | Contract |
| --- | --- | --- |
| `GET /v{VERSION}/session` | reader | Return protocol version, authenticated namespace, and role. |
| `POST /v{VERSION}/memories/scan` | reader | Search one requested scope (`all`, `own`, or `others`) and return at most five candidates. `own` and `others` are resolved from the authenticated principal. |
| `POST /v{VERSION}/memories/read` | reader | Resolve exact caller-visible keys. |
| `POST /v{VERSION}/memories/list` | reader | List caller-visible records without telemetry changes. |
| `POST /v{VERSION}/memories/put` | writer | Create a server-authored record or replace an exact current key in the writer's namespace. |
| `POST /v{VERSION}/memories/delete` | writer | Delete an exact current key in the writer's namespace. |
| `POST /v{VERSION}/memories/sync` | writer | Atomically reconcile the writer's namespace to a complete local snapshot. |
| `POST /v{VERSION}/memories/export` | reader | Page through all or selected caller-visible namespaces. |

Each request sends a bearer credential and a configured namespace assertion. Authentication derives
the actual namespace and role from the credential; a mismatched assertion is rejected. Mutation
bodies cannot select a target namespace. Namespaces contain at most 128 ASCII letters, digits,
periods, hyphens, or underscores.

Put allocates IDs, versions, timestamps, telemetry, and probation state on the server. Replacement
checks the exact current key, increments the version, and resets server-owned state. Clients do not
send local record metadata through ordinary put. Read omits missing or stale keys and updates read
telemetry. Each scan request ranks one namespace scope at the store, returns no more than the
requested limit or five candidates, and updates telemetry only for returned records. Ordinary
requests do not transfer the remote corpus. List returns a deterministic inspection window of at
most 512 visible records; production backends should enforce that bound in their storage query.
Export preserves every namespaced record and uses stable bounded pages with an opaque continuation
position; it does not deduplicate equivalent content.
Stable exports must neither omit nor repeat records. A backend must define transaction or snapshot
behavior that makes concurrent changes predictable.

### Remote server integration

The `tact-memory` crate exposes one async `MemoryStore` trait. Local SQLite, the authenticated HTTP
client, the runtime-selected backend, and server-side backends implement that contract.
`tact_memory::server::MemoryServer<S>` is the generic Axum wrapper. It authenticates a request,
passes its namespace to a factory, and uses the resulting namespace-bound store.

Tact uses one shared secret-content detector at its client-side storage boundaries. The selected
store rejects unsafe puts and snapshots before they reach either backend; local and remote clients
also suppress unsafe records that predate or bypass that check. Server-side `MemoryStore`
implementations remain content-policy agnostic and enforce storage invariants, not Tact's agent
policy.

The server wrapper owns bearer authentication, namespace and role checks, request validation and
bounds, protocol errors, and operation tracing that excludes tokens and memory content. A store
owns persistence, indexes, transactions, pagination, telemetry concurrency, capacity enforcement,
and backend errors. It must make version-checked mutations atomic, serialize conflicting writes,
keep snapshot replacement atomic, and provide stable export pagination. The server-side store captures the
authoritative time for remote timestamps, telemetry, and probation unless a protocol operation
explicitly preserves local snapshot identity.

Backends return semantic `MemoryError` variants for conflicts, bounds, and validation. A custom
backend wraps implementation-specific failures with `MemoryError::backend` or
`MemoryError::unavailable`; concrete database error types are not part of the shared contract.

The HTTP wrapper limits the router to 64 in-flight requests, times out store operations after 30
seconds, and keeps operation futures attached to request tasks. The hosting executable owns signal
handling and graceful shutdown. Backend implementations own their connection, task, and durability
lifecycle.

Other backends must preserve the same semantics. SQL backends need transactional version checks,
per-namespace uniqueness, capacity checks, and a deterministic export cursor. Cloudflare D1 or
Durable Objects need a transactional or single-writer boundary. Database-native search is valid
only when it reproduces visible single-scope scan behavior and the five-results-per-request bound.

Remote errors are JSON objects containing only a stable code. Responses and traces must not echo
request content, bearer tokens, database diagnostics, or credential details. Authentication,
authorization, stale keys, invalid or oversized requests, capacity, and transient storage failures
remain distinguishable through the defined status and error mapping. The client surfaces all
in-scope remote failures and never switches to local memory after one. Production deployments need
HTTPS, private credentials, and storage encryption appropriate to the deployment.

Runtime operations never push local records, write through to another backend, combine backend
results, or schedule background synchronization.

## Explicit transfer commands

Transfer commands operate on the global local store and therefore work from any directory. They
ignore `memory.enabled` and runtime workspace-root selection; configured remote credentials still
govern authentication and authorization.

### Push local memory

```console
tact memory push --dry-run
tact memory push
```

Push requires configured remote memory and a writer credential. It treats the complete live local
store as the authoritative snapshot for the writer's personal namespace. The client checks for a
concurrently changing local snapshot for up to three reconciliation passes. The service inserts
missing rows, replaces divergent generations or versions, preserves identical rows, and removes
rows in that namespace that are absent locally. Other namespaces are never changed. `--dry-run`
reports the local snapshot without contacting or modifying the service.

Push is an explicit administrative action. Runtime operations never trigger it and an outage does
not create a queue for a later push.

### Pull remote memory

```console
tact memory pull --all
tact memory pull --namespace alice --namespace bob
```

Pull accepts reader or writer credentials and requires either `--all` or at least one
`--namespace NAME`. It pages through the selected remote export and non-destructively merges its
records into local schema v1. Existing local content is preserved. Records with equivalent
normalized content are deduplicated. Because local v1 has no author field, imported records lose
their namespace provenance after merging.

The complete pull is atomic with respect to capacity and errors: validation, pagination, merge, and
capacity checks must succeed before any local change becomes visible. A failed or over-capacity
pull leaves the local store unchanged. A successful command reports fetched, inserted, and skipped
records together with the requested namespace selection.

## Local server walkthrough

The example runs the production-shaped Cloudflare Worker and D1 backend locally through Wrangler.
Install its build dependencies and start it from the repository root:

```console
cargo install worker-build --version 0.8.5 --locked
cd examples/tact-memory-cloudflare
npm ci
cp credentials.example.toml credentials.toml
chmod 600 credentials.toml
npm run migrate:local
npm run dev
```

Set independent tokens for Alice, Bob, and a read-only observer in the ignored `credentials.toml`
before starting the server. Each `[[credentials]]` table declares a namespace, a `reader` or
`writer` role, and a token; the supplied example shows the complete format. `bun run dev` validates
the file and generates the local Worker secret. Wrangler prints the local endpoint, normally
`http://127.0.0.1:8787/`, and persists local D1 state outside the process.

Create `/tmp/tact-alice/config.toml` with mode `0600`:

```toml
[memory]
enabled = true

[memory.remote]
endpoint = "http://127.0.0.1:8787/"
namespace = "alice"
bearer_token = "alice-local-test-token-000000000001"
workspace_roots = ["/absolute/path/to/this/repository"]
```

Create equivalent homes for Bob and the observer, changing namespace and token. Give Alice a local
record while running outside the configured workspace, then push it from any directory:

```console
chmod 600 /tmp/tact-alice/config.toml

TACT_HOME=/tmp/tact-alice cargo run -p tact -- \
  --workspace /tmp run 'Store a durable memory that the team uses cargo nextest in CI.'
TACT_HOME=/tmp/tact-alice cargo run -p tact -- memory push --dry-run
TACT_HOME=/tmp/tact-alice cargo run -p tact -- memory push
```

Verify that Bob uses remote memory exclusively inside the configured workspace and sees Alice's
namespaced record:

```console
TACT_HOME=/tmp/tact-bob cargo run -p tact -- \
  --workspace /absolute/path/to/this/repository \
  run 'Scan memory for the team CI runner, read the result, and report its namespace.'
```

Pull Alice and Bob into Bob's local v1 store, then use that local store outside the remote root:

```console
TACT_HOME=/tmp/tact-bob cargo run -p tact -- \
  memory pull --namespace alice --namespace bob
TACT_HOME=/tmp/tact-bob cargo run -p tact -- \
  --workspace /tmp run 'Scan local memory for the team CI runner.'
```

Finally, run the observer inside the configured workspace. It can scan, read, and list all visible
namespaces, including its own, but any put, replace, or delete must fail as read-only. The failure
must not create a local record.

For deterministic validation without model credentials, run from the repository root:

```console
cargo test -p tact-memory
cargo test -p tact
just check-wasm --locked
```

See `examples/tact-memory-cloudflare/README.md` for deploying the same Worker to Cloudflare.

## Record and retrieval contract

A record is one self-contained conclusion. Good records include durable user preferences,
corrections, authorization boundaries, or expensive-to-rediscover operational facts. Do not store
transcripts, reasoning, plans, raw output, credentials, transient state, generic knowledge, or facts
that a cheap repository search can recover.

Records carry a stable ID, monotonically increasing version, timestamps, and separate scan and read
telemetry. A replacement and delete check the expected version so concurrent work cannot be
silently overwritten. Remote keys add the server-authenticated author namespace; local keys do not.

Retrieval uses lexical BM25 with `k1 = 1.2` and `b = 0.75`. A scan request returns an empty candidate
vector when its query has no searchable terms or no active record in its scope shares a term. Each
request returns at most five cards and does not transfer its corpus to the caller. An agent-facing
remote scan sends the authenticated-namespace request first and the other-namespaces request second,
then preserves those responses as `ours` and `theirs`. A short record is its own preview; a longer
preview is a UTF-8-safe prefix of at most 64 bytes. `read` is the only operation that returns complete
selected content and increments deliberate-read telemetry.

New model-authored records enter seven days of unread probation. A scan does not graduate a record;
a successful read does. Replacement starts probation for the new version. The remote service uses
server time and server-owned telemetry.

## Bounds and transactions

| Limit | v2 value |
| --- | ---: |
| Record content | 1 KiB |
| Rows | 512 |
| Total content | 256 KiB |
| Local main database file | 4 MiB |
| Scan results per request | 5 |

Bounds apply to the global local corpus and independently to each remote writer namespace. A store
may prune expired unread probation records before rejecting a capacity-increasing mutation, but it
does not evict active graduated records to make room. Mutations, telemetry updates, bound checks,
authoritative push reconciliation, and local pull merge are transactional at their documented
scope.

The local database is unencrypted. Anyone who can read the configuration directory may be able to
inspect it. Production remote storage encryption and HTTPS termination belong to its deployment.
Deletion removes a record from Tact's retrieval surface but cannot promise forensic erasure from
SQLite sidecars, storage media, backups, snapshots, transcripts, model-provider records, or other
copies.
