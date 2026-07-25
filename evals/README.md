# Harbor evaluations

Tact runs the 89 tasks in Terminal-Bench 2.1 release 6 through Harbor. Each task starts in its own
local Docker environment, Tact attempts the task, and the task's canonical verifier assigns the
reward.

## Prerequisites

- Docker with Buildx, using a local Unix socket
- `uv`
- a current ChatGPT login stored in Codex's `auth.json`

On Apple Silicon, enable Docker Desktop's Rosetta support for `amd64` containers. Some
Terminal-Bench verifiers use x86-64 executables; QEMU failures can otherwise appear as benchmark
failures with a reward of `0`.

## Set up

Install the pinned Python environment and validate the integration:

```sh
just harbor-bootstrap
just check-harbor
```

`check-harbor` runs the adapter and Rust tests and resolves the Harbor configuration. It does not
start a benchmark or make a model request.

Build the instrumented static Linux binary used by the task containers:

```sh
just build-harbor-agent
```

The binary is written to `.tact/installed/tact`. Rebuild it whenever the Tact source changes.

## Run evaluations

Start with one attempt of a known small task:

```sh
just harbor-eval \
    --include-task-name terminal-bench/openssl-selfsigned-cert \
    --n-attempts 1 \
    --n-concurrent 1
```

Run the complete 89-task suite:

```sh
just harbor-eval --n-attempts 1 --n-concurrent 1
```

Harbor flags placed after `just harbor-eval` are forwarded to the run. Useful ways to select work
include:

```sh
# Run at most five tasks.
just harbor-eval --n-tasks 5 --n-concurrent 1

# Include or exclude tasks with a glob.
just harbor-eval --include-task-name 'terminal-bench/*ssl*' --n-concurrent 1
just harbor-eval --exclude-task-name 'terminal-bench/*cuda*' --n-concurrent 1

# Run three attempts per task and up to four trials concurrently.
just harbor-eval --n-attempts 3 --n-concurrent 4
```

Use one attempt while checking the setup. Use multiple attempts when comparing agent changes so
that a single stochastic success or failure does not dominate the result. Increase concurrency
carefully: each concurrent trial starts its own task and proxy containers and makes independent
model requests.

The configured model is `openai/gpt-5.6-sol`, matching Tact's pinned Nanocodex build. Changing only
Harbor's model label does not change Tact's model and is rejected.

### Test orchestration behavior

The default run measures task completion without requiring Tact to delegate. Optional assertions
can make a trial fail when a specific orchestration behavior is absent:

```sh
just harbor-eval \
    --include-task-name terminal-bench/openssl-selfsigned-cert \
    --agent-kwarg effort=high \
    --agent-kwarg max_subagents=8 \
    --agent-kwarg minimum_subagents=2 \
    --agent-kwarg require_wait=true \
    --agent-kwarg fail_on_subagent_error=true \
    --n-concurrent 1
```

- `minimum_subagents` requires at least that many child agents to start.
- `require_wait` requires a successful `wait_agent` result after delegation.
- `fail_on_subagent_error` rejects a run in which any child agent fails.

Keep these assertions disabled when measuring ordinary Terminal-Bench capability.

## Authentication

`just harbor-eval` looks for Codex authentication in this order:

1. `TACT_CODEX_AUTH_FILE`
2. `$CODEX_HOME/auth.json`
3. `~/.codex/auth.json`

If Codex uses the operating-system keyring and no file exists, set
`cli_auth_credentials_store = "file"` in the Codex configuration and run `codex login` again. The
access token must have at least one hour remaining when a trial starts.

The benchmark container receives mock credentials. A digest-pinned local iron-proxy container
substitutes the real access token and account ID only when the request leaves for the Codex backend.
The source `auth.json`, refresh token, identity token, and proxy secrets are not mounted into the
benchmark container. Remote Docker contexts and unsafe task-container configurations are rejected
before credentials are read. The commands do not publish task or proxy images.

## Read the results

Harbor prints a summary when the job finishes and retains the complete job under
`.tact/harbor/jobs/<job-id>/`. Open the local results browser with:

```sh
just harbor-view
```

For Terminal-Bench, each completed trial normally receives a binary reward:

- `1`: the task's verifier passed.
- `0`: the verifier ran and the task did not satisfy its checks.
- error: the trial did not produce a valid benchmark result because the agent, environment,
  verifier, or evaluation harness failed.

The job's mean reward is the fraction of completed attempts that passed. Always read it together
with the completed and errored trial counts: errors indicate an invalid or incomplete evaluation,
not an ordinary task failure. With multiple attempts, inspect per-task rewards as well as the
overall mean to distinguish consistent improvements from variance.

### Compare one task across harnesses

You do not need to run the full suite to compare Tact with another harness. Run the same task with
the same Terminal-Bench release and the same number of attempts in each harness:

```sh
just harbor-eval \
    --include-task-name terminal-bench/openssl-selfsigned-cert \
    --n-attempts 3 \
    --n-concurrent 1
```

Confirm that `task_name` and `task_checksum` match in each trial's `result.json`; matching only the
human-readable task name is not enough if the dataset revisions differ. Keep model, reasoning
effort, timeout, and tool access fixed when the goal is to compare harnesses rather than complete
agent configurations.

For each task and attempt, compare these values:

- **Output:** start with `verifier_result.rewards.reward`. Then compare verifier output and the final
  agent message to understand why two runs with the same reward behaved differently. The verifier
  reward is the benchmark outcome; a confident final message is not evidence that the task passed.
- **Time:** compare `agent_execution.started_at` and `agent_execution.finished_at` from the trial
  result. This isolates the agent phase from image builds, environment setup, and verification.
  Compare distributions across several attempts rather than one wall-clock sample.
- **Cost:** compare `agent_result.cost_usd` when both harnesses report it. ChatGPT subscription
  requests currently do not return a dollar cost, so Tact records `null`, not zero. In that case,
  report input, cached-input, and output tokens separately from `agent_result`; token counts are a
  useful workload measure but are not interchangeable with dollars across models or providers.

The comparable fields can be extracted from one trial with:

```sh
jq '{
  task_name,
  task_checksum,
  reward: .verifier_result.rewards.reward,
  agent_execution,
  cost_usd: .agent_result.cost_usd,
  input_tokens: .agent_result.n_input_tokens,
  cached_input_tokens: .agent_result.n_cache_tokens,
  output_tokens: .agent_result.n_output_tokens,
  exception_info
}' .tact/harbor/jobs/<job-id>/<trial-id>/result.json

jq '[.steps[] | select(.source == "agent") | .message][-1]' \
    .tact/harbor/jobs/<job-id>/<trial-id>/agent/trajectory.json
```

For a fair summary, report the task checksum, attempts, pass rate, median agent time, time range or
tail percentile, measured dollar cost when available, and token counts. Preserve failed attempts
and infrastructure errors in the report instead of averaging them away.

The most useful retained files are:

- `result.json`: aggregate counts, mean reward, token usage, and per-evaluation statistics.
- `<trial>/result.json`: the individual trial outcome and any exception information.
- `<trial>/verifier/reward.txt`: the verifier's reward for that trial.
- `<trial>/verifier/test-stdout.txt`: verifier output explaining failed checks or infrastructure
  failures.
- `<trial>/trial.log`: Harbor's complete trial lifecycle log.
- `<trial>/agent/stderr.log`: Tact diagnostics.
- `<trial>/agent/events.jsonl`: the root agent event stream.
- `<trial>/agent/orchestration.jsonl`: child-agent topology, lifecycle, messages, and cleanup state.
- `<trial>/agent/trajectory.json`: the Harbor ATIF trajectory and aggregate root-plus-child usage.

When a reward is surprising, check `test-stdout.txt` first. A normal assertion failure means the
agent missed the task. Segmentation faults, package-install failures, timeouts, or missing verifier
output indicate an environment problem and should not be treated as an agent score.
