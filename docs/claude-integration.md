# Claude integration

Claude is opt-in through `[claude] enabled = true`. Tact uses Nanocodex's native Claude client,
model and effort types, subscription manager, and Code Mode tool runtime.
The temporary dependency uses the fork branch `cl/fix-code-mode-cancellation`;
`Cargo.lock` pins [the cancellation fix](https://github.com/clabby/nanocodex/commit/f7d0e042371a3a8453d439abb6233c1726db64e9).

## Authentication

Both providers use `tact auth --provider codex|claude login`, `status`, and `logout`. Codex is the
default provider for these commands. Both login routes support `--no-open`.

Claude defaults to subscription authentication. Its separate Nanocodex OAuth login does not
import Claude Code credentials. Set `claude.auth = "api-key"` to select `ANTHROPIC_API_KEY`
instead. Neither route falls back to the other. Logging in does not change the configured route.
One subscription manager is shared by the task tree, including Claude children of a Codex root,
so concurrent agents coordinate token refresh.

The host stores encrypted credentials separately from session history and checkpoints. The
upstream manager owns OAuth state, exchange, refresh, and logout; Tact supplies private storage
with durable compare-and-swap and bounded HTTP. Authentication information is not added to the
agent's instructions.

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

Integration tests use loopback OAuth/Messages servers and a synthetic Codex transport. They cover
both delegation directions, authentication selection, coordinated refresh, Code Mode round trips,
images and structured results, yielded execution, terminal-event admission, stalled compaction,
and cancellation of cells and real local shell processes. The upstream runtime tests additionally
cover dropped initial execution callbacks and concurrent observers.

```sh
cargo test -p tact --bin tact core::claude::tests
cargo test -p tact --bin tact mixed_provider_tests
cargo test -p tact --bin tact app::claude_auth::tests
```

These tests use no live login or paid inference. They establish local protocol and lifecycle
behavior, not provider admission or subscription billing acceptance.

Native Claude does not support conversation forks or effort changes after the first prompt.
Tact reports these restrictions. Upstream does not provide Claude dollar estimates.
