# Claude integration

Claude is opt-in through `[claude] enabled = true`. Tact uses Nanocodex's native Claude client,
model and effort types, and Code Mode tool runtime.
The temporary dependency uses the fork branch `cl/claude-integration`;
`Cargo.lock` pins [the combined runtime fixes](https://github.com/clabby/nanocodex/commit/5994a860ef9e229ac67a2a939406b3ce48b799ad).

## Authentication

Claude requires `ANTHROPIC_API_KEY`. `tact auth --provider claude status` reports whether
that environment variable is configured without displaying its value. Claude login and logout
commands explain how to configure or unset the key; they do not perform authentication.
Tact does not read or store Claude subscription credentials.
Keys without a workspace scope require `claude.workspace_id`, which supplies the
`anthropic-workspace-id` request header. Unset or blank values omit the header.
Both status and client construction accept the `sk-ant-api` and `sk-ant-usr-` key prefixes
and reject other formats, including OAuth access and refresh tokens placed in `ANTHROPIC_API_KEY`.

Codex authentication is independent. A Codex root or child can use its ChatGPT subscription
while Claude agents use API keys. Secrets and authentication details are not added to model
instructions or checkpoints. Unknown `[claude]` fields are rejected, so a retired subscription
configuration cannot silently switch to API billing.

## Code Mode and lifecycle

Claude calls `exec` with a JSON `code` field and resumes yielded work through `wait`. Both use
Tact's existing runtime and nested tool catalog, including memory, MCP, shell commands, and
subagents. Tool invocations retain their native session, turn, and call identities. Text and image
results are delivered to Claude; transcript events retain the original tool output and exact
`structured_result` values. Audio tool outputs are rejected explicitly by the native adapter.

Each agent has its own tool runtime. A turn's terminal event is published only after its Code Mode
producers stop and buffered nested-tool updates are delivered. The upstream cancellation observer
shares the execution observer's cursor, so it emits each completion once, including operations
that completed after `exec` yielded. It also covers cancellation before the initial `exec` returns.
Tact does not infer a tool's result from cancellation or reconstruct its structured value.

Interruption and shutdown stop producers before draining shell processes. Ordinary successful
turns retain session-owned shells, following the existing runtime's ownership rules. Shutdown can
cancel a stalled compaction request without waiting for its admission lock.

## Context recovery

If Claude fills its context window during generation, Nanocodex retains the completed response,
summarizes the earlier history, and continues the turn once. Compaction preserves the latest
signed assistant/server-tool boundary and completed receipts. The recovery budget survives
reopening a durable session. Failed or cancelled summaries keep the received content intact.

Repeated exhaustion, incomplete tool calls, unresolved server effects, or context that cannot
be reduced enough still return an error. Recovery does not discard effect evidence to force a
request to fit. Proactive compaction continues to run between model requests.

## Validation and limits

Integration tests use loopback Messages servers and a synthetic Codex transport. They cover
both delegation directions, API-key authentication, Code Mode round trips,
images and structured results, yielded execution, terminal-event admission, stalled compaction,
and cancellation of cells and real local shell processes. The upstream runtime tests additionally
cover dropped initial execution callbacks and concurrent observers.

```sh
cargo test -p tact --bin tact core::claude::tests
cargo test -p tact --bin tact mixed_provider_tests
```

These tests use no live login or paid inference. They establish local protocol and lifecycle
behavior, not live provider admission.

Native Claude does not support conversation forks or effort changes after the first prompt.
Tact reports these restrictions. Upstream does not provide Claude dollar estimates.
