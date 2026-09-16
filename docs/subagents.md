# Subagents

This document specifies Tact's subagent runtime. The feature lets one agent delegate focused work
to clean child sessions, coordinate a task tree, and collect typed results without sharing the
parent's conversation history.

The reusable runtime and tool surface live in the `tact-subagents` crate. Applications construct a
`Subagents` runtime, provide a factory for clean Nanocodex sessions, install its tools, and consume
typed `ScopedAgentUpdate` values. Tool factories capture `WeakSubagents`, which prevents the
runtime-owned child factory from creating a strong ownership cycle. Tact keeps configuration,
session references, memory policy, headless recording, and TUI presentation in the binary.

## Product contract

Subagents are enabled by default. They can be disabled explicitly:

```toml
[subagents]
enabled = false
```

Disabling the feature removes all subagent tools and omits Tact's built-in delegation instructions
from fresh sessions. Resumed parents retain their saved instructions. The setting does not disable
memory, skills, MCP servers, or ordinary code-mode tools.

The setting applies when an agent runtime is created. Reloading configuration does not mutate the
tool surface or instructions of an existing runtime. A later new or restored session uses the
reloaded setting.

Subagents explicitly choose `luna`, `terra`, `sol`, or `astra` for each task. There is no `selected`
alias or per-model configuration switch. Consider Luna for simple tasks, Terra for a balance of
capability and cost, Sol for bounded coding and analysis, and Astra for the hardest reasoning.

`agent.max_subagents` is independent of the enable switch:

```toml
[agent]
max_subagents = 32
```

The limit bounds concurrent active subagent turns when subagents are enabled. It does not enable
or disable the feature. The default limit is 32. The TUI can change the limit while turns are
active. Lowering it does not cancel existing work; it prevents new reservations until active work
falls below the new limit.

## Tool surface

An enabled runtime installs seven tools:

| Tool | Contract |
| --- | --- |
| `spawn_agent` | Create a clean child session with a required role, focused task, model choice, thinking effort, and output schema. |
| `submit_result` | Submit the current subagent turn's final JSON value. The value must satisfy its output schema and use the current turn token. |
| `send_agent_message` | Send a bounded directed message within the current task tree. |
| `list_agents` | List visible agents, their status, topology, and the caller's messaging and management authority. |
| `wait_agent` | Wait until any selected agent becomes terminal, with a bounded timeout. |
| `interrupt_agent` | Stop an agent's active turn and active descendants while keeping their sessions reusable. |
| `close_agent` | Close an agent and its descendant subtree. Closed agents remain inspectable but cannot be reused. |

The tools are installed for root and child agents. This lets children delegate independent
subtasks and coordinate with peers. Authorization is enforced by the runtime, not only by prompt
text.

`submit_result` is usable only by a registered child during an active turn. Root sessions and
user-created forks are not registered as children. Conversely, memory mutation uses the same
registry to preserve root-only authority even when subagent tools are disabled. The memory tool is
therefore installed independently of the subagent tool group.

## Clean sessions and output contracts

`spawn_agent` creates a new Nanocodex session through Tact's configured child factory. The child
does not inherit the caller's conversation. Each child model's instructions are composed from the
configuration and skill catalog when the root runtime starts. This includes configured replacement
and appended instructions. A resumed parent keeps its saved instructions; its new children use
these freshly composed instructions. Children inherit the configured reasoning mode and fast-mode
setting. Their initial prompt contains:

- the assigned role and task;
- its agent ID and place in the task tree;
- its own model and reasoning effort;
- coordination rules for peers and descendants; and
- the required structured-output contract.

The required `model` and `thinking` fields choose the child's capabilities at creation. A child
cannot exceed the spawning parent's model. Model order is Luna < Terra < Sol < Astra; effort order
is low < medium < high < xhigh < max. Root agents use the live configured `agent.thinking` cap for
spawning effort. Registered subagents are additionally bounded by their own assigned effort. For
example, an Astra root with a high cap can spawn a Sol/medium child, but that child cannot spawn
Astra or request high effort. The root can launch a stronger verifier after receiving the child's
result.

The live configured `agent.thinking` cap also bounds every new spawn. User changes to this cap
take effect for subsequent spawns, including those requested by an already active root turn.
Existing children retain their model and effort and cannot exceed either when spawning descendants.
Requests above either applicable cap fail before the child factory runs.

Every new turn receives an `<agent_context>` block identifying its own model and effort. Root,
restored, forked, auxiliary, and child sessions receive this context. Effort changes update it for
subsequently accepted turns; already accepted turns and active steering retain their original effort.

Choose model and effort for the full delegated reasoning obligation. Optimize expected total cost
and time to a correct completed result, including rework. Higher effort or a stronger model upfront
can avoid repeated weaker runs; a cheaper or lower-effort attempt is not a prerequisite.

| Effort | Typical delegated work |
| --- | --- |
| `low` | Lookups, extraction, mechanical edits, prescribed checks. |
| `medium` | Localized implementation, editorial/design work, or focused review with an established contract. |
| `high` | Difficult but bounded correctness investigation with identifiable invariant owners and failure cases, such as tracing cancelled I/O through completion and successor reopen. |
| `xhigh` | Derive a missing contract or reconcile interacting owners and competing designs, such as placing a durability barrier while accounting for crash safety and namespace-lock contention. Also consider it after a completed high result misses the same invariant. |
| `max` | Own an integrated architecture or proof with several coupled invariants and potentially misleading local success: runtime-to-journal durability across pruning, publication, and repeated crashes, or a protocol redesign spanning authentication, proof retention, recovery, and replay. |

Choose `xhigh` or `max` immediately when these challenges are apparent. Assign the complete proof
and attempts to falsify it to the agent responsible for the integrated result. Repeated weaker
local reviews do not discharge a global proof obligation. A review label, file count, difficult
parent project, or changed requirements alone does not justify high effort for every child.
Identify the concrete reasoning challenge in briefs for `high` or above. Higher effort still
requires causal tests and evidence.

If a completed answer is unsatisfactory, supply missing context or request focused follow-up when
that can resolve the gap. When stronger reasoning is needed, start a child at higher effort within
the caps, using a more capable model when appropriate. Pass the original request and constraints,
the prior result, evidence and counterexamples, and unresolved questions or failed checks. The
child should challenge the earlier conclusion and verify its resolution. A child that needs a
model or effort above its own cap returns that package to a capable ancestor. Still-running work
should be allowed to finish. If permitted escalation cannot settle the question, report what
remains unresolved.

Every `spawn_agent` call must provide `role`, `task`, `model`, `thinking`, and `output_schema`:

```json
{
  "role": "reviewer",
  "task": "Review the configuration parser diff against the documented keys and report supported findings.",
  "model": "sol",
  "thinking": "medium",
  "output_schema": {
    "type": "object",
    "properties": { "findings": { "type": "string" } },
    "required": ["findings"],
    "additionalProperties": false
  }
}
```

The caller supplies a JSON Schema for the result. Tact compiles that schema before creating the
child. A successful turn must call `submit_result` exactly once with a value that validates against
the schema. Tact reports at most four validation errors for an invalid submission so the child can
correct it.

Each active turn has a monotonically changing token. `submit_result` must provide the current
token. Steering rotates the token, so output from the superseded turn cannot satisfy the new turn.
A successful model turn without an accepted submission is a failed subagent turn.

The parent receives structured status and the validated JSON output. Tact does not require a
free-form prose result and does not parse a child's final answer to infer completion.

## Task tree and authority

Every root session owns an isolated task tree. Agent IDs are local to that tree. Messages, waits,
interrupts, closes, and directory queries cannot cross root-session boundaries.

The tree records each child's parent. Management authority follows ancestry:

- the root can manage every descendant;
- an agent can manage its descendants;
- an agent cannot manage siblings, ancestors, or agents in another root tree; and
- messaging is broader than management: agents in the same tree may send ordinary coordination
  messages to one another, subject to routing rules.

Closing and interruption operate on subtrees. Descendants are stopped before their parent. Closing
is terminal for those child sessions. Interruption preserves the sessions for later messages or
delegation.

## Directed messages

`send_agent_message` attaches typed routing metadata to a message:

| Field | Values | Meaning |
| --- | --- | --- |
| `priority` | `deferred`, `urgent` | Deferred delivery preserves turn boundaries. Urgent delivery steers a running turn at its next safe boundary. |
| `purpose` | `delegate`, `coordinate`, `finding`, `question`, `reply` | Describes intent. Only `delegate` changes the recipient's assigned task. |
| `in_reply_to` | message ID | Continues an existing two-party thread. Only the original recipient may reply, and the reply direction must reverse the original message. |

Message bodies are limited to 2 KiB of UTF-8. Empty messages are rejected. The runtime retains at
most 256 completed message records per root tree while preserving pending records until they reach
a terminal delivery state.

Deferred delivery starts an idle recipient or queues behind its active turn. A queued sender must
finish its own turn before the queued message can be delivered. This prevents an agent from waiting
inside the turn whose completion is needed to release the recipient.

Urgent delivery steers an active turn. Ordinary coordination messages add context but do not
replace the task. A `delegate` message replaces the recipient's assigned task only when the sender
has management authority. The recipient keeps its original output schema.

The runtime reports message admission and delivery separately. A message may be started, queued,
or steered. Failed or interrupted delivery is surfaced explicitly rather than treated as success.

## Waiting and lifecycle

`wait_agent` accepts one or more agent IDs and returns when any selected agent reaches a terminal
status. Its default timeout is 30 seconds and its maximum timeout is 300 seconds. A timeout returns
current summaries with `timed_out = true`; it does not cancel the agents.

`interrupt_agent` cancels active model and tool work for the selected subtree and waits for cleanup.
The sessions remain reusable. `close_agent` additionally closes those sessions and prevents later
turns. Both operations have a bounded internal stop deadline of 30 seconds.

Tact closes every remaining child before its root runtime shuts down or is replaced. Runtime-owned
tasks and event forwarders are awaited so child work does not outlive the owning session.

## Capacity and backpressure

Capacity is reserved per active subagent turn, not per child object. A completed, failed,
interrupted, or closed turn releases its reservation. Idle reusable children consume no turn
capacity.

The limit is shared by the complete root tree, including grandchildren. Reservations are checked
atomically. When the limit is reached, spawning or starting another turn fails with an explicit
capacity error; Tact does not silently queue unbounded model work.

Message delivery also uses bounded command channels. Deferred and urgent messages have distinct
admission behavior so urgent steering cannot be trapped behind an unlimited deferred queue.

## Runtime and TUI boundary

The registry emits typed updates for agent creation, status changes, events, and message delivery.
Each update carries a runtime ID and root-session ID. The TUI discards updates from replaced
runtimes, which prevents stale child events from appearing in a new session that occupies the same
pane.

The `/subagents` panel renders the current tree, statuses, tasks, messages, and child transcripts.
It is an observation and control surface for the live runtime. Changing configuration does not
rewrite the state of an already-running tree.

Subagent registry state is process-local. Restoring a root model session creates a new empty
subagent runtime; prior child sessions are not resumed as live children. Tool calls already present
in the root transcript remain ordinary historical transcript records.

## Failure model

The runtime treats these as explicit failures:

- invalid output schemas or submitted values;
- stale turn tokens and duplicate submissions;
- cross-tree access, self-messaging, or unauthorized management;
- invalid reply direction or unknown message IDs;
- capacity exhaustion and bounded-channel exhaustion;
- a model turn that ends without `submit_result`;
- child model, tool, event-forwarding, or shutdown errors; and
- messages interrupted before terminal delivery.

Failures update the affected agent or message state and are returned through the tool boundary.
They do not silently produce a successful result. A completed agent remains inspectable and may be
started again by a later deferred message. A closed agent does not.

## Security and trust boundaries

Subagents run with the same configured model provider, workspace, base tool selection, and process
authority as the root runtime. They are an execution and coordination boundary, not a security
sandbox. A clean conversation reduces accidental context sharing but does not restrict filesystem
or network access granted to their tools.

The shared workspace means concurrent agents can observe one another's file changes. Tact instructs
agents to treat concurrent changes as owned by their authors, but the filesystem does not enforce
disjoint write scopes. The root remains responsible for assigning separable tasks, resolving
overlap, reviewing results, and validating the final state.

Directed message metadata is trusted runtime context. Message bodies remain agent-authored content.
Delegate authority, reply routing, root scoping, output validation, and lifecycle permissions are
checked in code.

## Design invariants

The implementation preserves these invariants:

- disabling subagents removes their tools and omits their fixed instructions from fresh sessions;
- model cannot exceed the spawning parent's model, and the live configured effort cap bounds every
  new spawn; registered subagents also cannot delegate above their own assigned effort;
- each new turn receives its own effective model and effort in context;
- memory remains independent from the subagent enable switch;
- every child starts with clean conversation context and a caller-supplied output contract;
- task-tree scope prevents cross-root access;
- management follows ancestry while coordination can cross sibling branches;
- one active turn consumes one shared capacity reservation;
- successful completion requires a schema-valid `submit_result` with the current turn token;
- queued delivery cannot require the sender to remain inside the blocking turn;
- interruption is reusable, closure is terminal, and shutdown closes all descendants; and
- stale runtime updates cannot attach to a replacement TUI session.

These constraints are deliberately narrower than a general distributed workflow engine. Durable
job queues, remote workers, cross-process recovery, independent credentials, filesystem isolation,
and restoration of live child trees are out of scope.
