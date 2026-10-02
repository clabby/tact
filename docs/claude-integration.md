# Claude integration

Claude is opt-in through `[claude] enabled = true`. Tact uses Nanocodex's native Claude client,
model and effort types, and Code Mode tool runtime.
The temporary dependency uses the fork branch `cl/fix-code-mode-cancellation`;
`Cargo.lock` pins [the cancellation fix](https://github.com/clabby/nanocodex/commit/f7d0e042371a3a8453d439abb6233c1726db64e9).

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
