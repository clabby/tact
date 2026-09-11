# tact-subagents

[![GitHub Actions Workflow Status](https://img.shields.io/github/actions/workflow/status/clabby/tact/ci.yaml?style=for-the-badge&label=CI)](https://github.com/clabby/tact/actions/workflows/ci.yaml)
[![Crates.io License](https://img.shields.io/crates/l/tact-subagents?style=for-the-badge)](https://crates.io/crates/tact-subagents)
[![Crates.io MSRV](https://img.shields.io/crates/msrv/tact-subagents?style=for-the-badge)](https://crates.io/crates/tact-subagents)
[![Crates.io Version](https://img.shields.io/crates/v/tact-subagents?style=for-the-badge)](https://crates.io/crates/tact-subagents)

`tact-subagents` provides structured child-agent orchestration for Nanocodex applications. It owns
clean child sessions, a scoped task tree, bounded concurrent turns, directed messages, structured
result validation, and lifecycle tools.

The crate exposes three integration boundaries:

- `Subagents` owns one in-process runtime. Its weak handle installs Nanocodex tools without
  creating an ownership cycle.
- `ScopedAgentUpdate` is the observation stream for lifecycle, model, and message events.
- `RootAgentAuthority` lets an application restrict its own tools to coordinating root sessions.

Create one runtime for each root agent configuration. Drain its update receiver continuously,
configure a factory that creates a fresh Nanocodex session for each child, and call
`Subagents::downgrade` before capturing the handle in the root builder's tool factory. Call
`WeakSubagents::install_tools` there. The same tool factory is inherited by child sessions, which
permits nested delegation while the runtime enforces task-tree authority.

Each spawn explicitly names its model and reasoning effort. The runtime bounds new spawns by the
immediate parent's model and the live configured effort cap. Registered children also cannot spawn
above their own assigned effort. Applications can use `AgentContext::prompt` to include the
executing turn's model and effort in a root prompt; the runtime supplies that context on every
child turn.

The runtime is process-local. It does not persist live child sessions, isolate filesystem access,
or provide a distributed job queue. Root and child sessions use the process and tool authority
granted by the embedding application.

See the [Tact subagent design](https://github.com/clabby/tact/blob/main/docs/subagents.md) for the
complete lifecycle, messaging, and failure contracts.
