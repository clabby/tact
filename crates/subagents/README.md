# tact-subagents

[![GitHub Actions Workflow Status](https://img.shields.io/github/actions/workflow/status/clabby/tact/ci.yaml?style=for-the-badge&label=CI)](https://github.com/clabby/tact/actions/workflows/ci.yaml)
[![Crates.io License](https://img.shields.io/crates/l/tact-subagents?style=for-the-badge)](https://crates.io/crates/tact-subagents)
[![Crates.io MSRV](https://img.shields.io/crates/msrv/tact-subagents?style=for-the-badge)](https://crates.io/crates/tact-subagents)
[![Crates.io Version](https://img.shields.io/crates/v/tact-subagents?style=for-the-badge)](https://crates.io/crates/tact-subagents)

`tact-subagents` lets a Nanocodex agent delegate work to clean child sessions. It owns the child
sessions, a task tree for each root session, bounded concurrent turns, directed messages between
agents, structured-result validation, and the lifecycle tools that expose all of this to models.

The crate has four integration points:

- `Subagents` owns one in-process runtime. Its weak handle, `WeakSubagents`, installs the
  Nanocodex tools without creating an ownership cycle.
- `ScopedAgentUpdate` is the stream of lifecycle, model-event, and message updates.
  `SubagentRoster` folds that stream into the tree a front-end displays.
- `RootAgentAuthority` lets an application restrict its own tools to root sessions.
- `SUPPORTED_MODELS` and `parse_model` define the models a child may run.

One runtime can serve several root sessions. Each root session owns an isolated task tree, keyed by
its Nanocodex session ID, and agent IDs are local to that tree. Tact creates one runtime per root
agent so that replacing a session also discards its children.

## Usage

Create the runtime, give it a factory that builds a fresh child session on every call, and install
its tools through the weak handle. The same tool factory is inherited by child sessions, which lets
children delegate further while the runtime enforces task-tree authority. Drain the update receiver
for as long as the runtime lives; dropping it stops event forwarding.

```rust,no_run
use nanocodex::{AgentEvents, HarnessModel, Nanocodex, NanocodexError, Thinking};
use nanocodex::tools::ToolsBuilder;
use tact_subagents::{Speed, SubagentRoster, Subagents};

fn build_child(
    model: HarnessModel,
    thinking: Thinking,
    speed: Speed,
) -> Result<(Nanocodex, AgentEvents), NanocodexError> {
    // Build a clean session with the requested model, effort, and speed.
    # let _ = (model, thinking, speed);
    # unimplemented!()
}

# async fn run() -> Result<(), NanocodexError> {
let (subagents, mut updates) = Subagents::new(8);
subagents.set_agent_factory(Thinking::High, Speed::Standard, build_child)?;

// Capture only the weak handle in tool factories.
let weak = subagents.downgrade();
let install_tools = move |tools: ToolsBuilder| weak.install_tools(tools);
# let _ = install_tools;

let mut roster = SubagentRoster::new(8);
tokio::spawn(async move {
    while let Some(scoped) = updates.recv().await {
        roster.apply(&scoped.update);
    }
});
# Ok(())
# }
```

## Models and effort

Each spawn names its model and reasoning effort with Nanocodex's `HarnessModel` and `Thinking`
types. A Codex child cannot run a higher tier than a Codex parent (Luna < Sol < Astra). Claude models
(Sonnet 5.5, Opus 5.5, and Fable 5.1) are rejected until `Subagents::set_claude_enabled(true)`;
once enabled, any model may delegate to them and they may delegate to any model. Every spawn is
bounded by the runtime's live effort cap, and a registered child cannot spawn above its own
effort. `AgentContext::prompt` appends the executing turn's model and effort to a root prompt; the
runtime does the same on every child turn.

## Limits

The runtime is process-local. It does not persist live child sessions, isolate filesystem access,
or provide a distributed job queue. Root and child sessions use the process and tool authority
granted by the embedding application.

See the [Tact subagent design](https://github.com/clabby/tact/blob/main/docs/subagents.md) for the
complete lifecycle, messaging, and failure contracts.
