# tact

[![GitHub Actions Workflow Status](https://img.shields.io/github/actions/workflow/status/clabby/tact/ci.yaml?style=for-the-badge&label=CI)](https://github.com/clabby/tact/actions/workflows/ci.yaml)
[![Crates.io License](https://img.shields.io/crates/l/tact?style=for-the-badge)](https://crates.io/crates/tact)
[![Crates.io MSRV](https://img.shields.io/crates/msrv/tact?style=for-the-badge)](https://crates.io/crates/tact)
[![Crates.io Version](https://img.shields.io/crates/v/tact?style=for-the-badge)](https://crates.io/crates/tact)

`tact` is a terminal interface for [Nanocodex](https://github.com/gakonst/nanocodex).

<https://github.com/user-attachments/assets/5c634ae8-5c74-47c9-bb8c-9c18cb7fc97d>

## Execution environment

Tact does not sandbox agent commands by default. The agent can read and modify files and run
processes with the same permissions as the user running Tact. For a containerized, credential-
isolated setup, see the example [development environment](docker/dev/README.md), which keeps real
OpenAI credentials outside the development container while mounting the workspace and Tact state
read-write.

## Installation

The release installer supports x86-64 and ARM64 glibc-based Linux, as well as Intel and Apple
Silicon Macs:

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://tact.clab.by/install.sh | sh
```

It verifies the release checksum and installs `tact` in `~/.local/bin` without `sudo`. Set
`TACT_INSTALL_DIR` to another absolute directory if you prefer a different location.

You can also install the published crate with Cargo:

```sh
cargo install tact --locked
```

To build the current source instead:

```sh
git clone https://github.com/clabby/tact.git
cd tact
cargo install --locked --path bin/tact
```

### Updates

Official release binaries can update themselves:

```sh
tact update
```

The updater verifies both the release checksum and signature before replacing a release-installer
binary. If Cargo owns the installation, tact instead prints `cargo install tact --locked` so
Cargo's records stay accurate, and builds declared through `TACT_PACKAGE_MANAGER` defer to the
named package manager. Automatic update notifications are shown by every installation except
development builds.

### Packaging

Distribution packagers can declare the package manager that owns the installation at build time:

```sh
TACT_PACKAGE_MANAGER=nix cargo build --release
```

Such builds keep update notifications and the managed review interface, but `tact update` points
at the owning package manager instead of replacing the binary in place. When building from a
source archive without the git repository, the metadata reported by `tact --version` can be
provided through the `TACT_GIT_SHA`, `TACT_GIT_BRANCH`, `TACT_GIT_COMMIT_TIMESTAMP`, and
`TACT_GIT_DIRTY` environment variables.

## Authentication

By default, tact uses the ChatGPT session stored by Codex in `$CODEX_HOME/auth.json` or
`~/.codex/auth.json`. If that file does not exist, it uses `OPENAI_API_KEY`, then `openai.api_key`
in the TOML configuration.

To sign in with a ChatGPT subscription:

```sh
tact auth login
tact auth status
```

`tact auth logout` removes the shared credential file, which also signs Codex out. To require
API-key authentication even with a stored ChatGPT login, configure:

```toml
[auth]
mode = "api-key"

[openai]
api_key = "your-api-key"
```

You can also select API-key authentication with `tact --auth api-key`. A nonblank `OPENAI_API_KEY`
overrides `openai.api_key`; a blank or absent environment value uses the configured key. Auto mode
continues to prefer the stored ChatGPT login. A rejected credential returns an error without
switching authentication or billing sources.

Files containing an API key require private permissions on Unix, such as `chmod 600 ~/.tact/config.toml`.
Config output, debug output, and authentication status redact the key; status identifies its source.
The same OpenAI credentials authenticate web search and image generation, including those tools
used by Claude sessions. Remove `openai.api_key` and unset `OPENAI_API_KEY` to remove API credentials.

## Non-interactive use

For scripts and integrations, `tact run` submits one prompt and streams Nanocodex events as JSONL:

```sh
tact run "inspect the workspace"
```

Override the configured model for a newly started agent:

```sh
tact --model sol
tact --model luna run "inspect the workspace"
```

New sessions use Sol unless a model is configured. `--model` accepts `luna`, `sol`, or `astra`, and their full IDs: `gpt-6-luna`, `gpt-6.1-sol`,
and `gpt-6-astra`. `TACT_MODEL` provides the same per-launch override.
Resumed sessions retain the model recorded when they were created. Sessions using retired
model IDs cannot resume; start a new session with a supported model.

When no effort is configured, Sol and Astra use low effort and Luna uses medium. A configured
effort takes precedence. Tact supports low through max effort. Sol and Luna support Pro mode;
Astra uses standard mode.

Click the speed icon or choose **Change speed** to open the Standard/Fast/Ultrafast dial.
The Nerd Fonts turtle, rabbit, and rocket glyphs show Standard, Fast, and Ultrafast effective speeds.
Speed is independent of effort and remains selected when switching models. Ultrafast uses the
fastest tier supported by each model:

| Model | Effective ultrafast preference |
| --- | --- |
| Astra | Ultrafast |
| Sol, Luna, Opus 5.5 | Fast |
| Sonnet 5.5, Fable 5.1 | Standard |

Astra sends `service_tier = "ultrafast"`; access and pricing depend on the account.
See [OpenAI ultrafast mode](https://developers.openai.com/api/docs/guides/ultrafast-mode).

OpenAI models use automatic prompt caching. Tact keeps request prefixes and conversation history
stable across related turns, children, and restored sessions to support reuse. Cache hits depend on
the provider and matching context. For the GPT-6 API models, the default minimum cache lifetime is
30 minutes after a write or reuse; see [OpenAI prompt caching](https://developers.openai.com/api/docs/guides/prompt-caching).

Claude support requires an explicit config opt-in and an Anthropic API key in `claude.api_key`
or `ANTHROPIC_API_KEY`:

```toml
[claude]
enabled = true
api_key = "sk-ant-api03-..."

[agent]
model = "sonnet-5.5" # or opus-5.5, fable-5.1
thinking = "medium"
```

Type in the model picker to filter by name, provider, or model ID. Use ↑/↓ to select a model,
Enter to apply it, and Esc to cancel. The check mark identifies the current model.

With Claude enabled, `--model` and the model picker also accept `sonnet-5.5`, `opus-5.5`, and `fable-5.1`
(native IDs `claude-sonnet-5-5`, `claude-opus-5-5`, and `claude-fable-5-1`). All support
`low`, `medium`, `high`, `xhigh`, and `max`. Their default efforts are medium for Opus and high for Sonnet and Fable.
Claude uses standard reasoning mode. Opus 5.5 supports accelerated processing through
`agent.speed = "fast"` or the speed dial; Sonnet 5.5 and Fable 5.1 use standard processing.
The default speed is standard. Opus fast processing uses
Anthropic's [premium fast-mode service](https://platform.claude.com/docs/en/build-with-claude/fast-mode),
which requires access on the API account. Changes apply to subsequently accepted turns.
Claude requests use automatic prompt caching with a one-hour TTL, covering the system prompt,
tools, and reusable conversation prefix across long tool calls and user pauses. Cache hits refresh
the TTL. One-hour cache writes cost twice the base input rate; repeated prefixes use discounted
cache reads. Expiration affects cost and latency, not the saved conversation.
Web search and image generation
remain available to Claude and use OpenAI credentials. For an Anthropic-only setup, set
`agent.web_search = false` and `agent.image_generation = false`. Codex children need the
configured OpenAI credentials.

Check whether the Claude API key is configured:

```sh
tact auth --provider claude status
```

An explicitly set, nonblank `ANTHROPIC_API_KEY` overrides `claude.api_key`. Configured keys are not
added to shell or MCP environments. Config files containing a key must have private permissions
on Unix, for example `chmod 600 ~/.tact/config.toml`, just like files containing a memory token.
Config output, debug output, and authentication status do not display the key. Status identifies
which source is selected. On platforms where file privacy cannot be verified, use the environment.

Claude supports API-key authentication only. Tact does not log into Claude subscriptions or
read subscription credentials. Keys must use the `sk-ant-api` or `sk-ant-usr-` prefix; subscription
tokens and other credential formats are rejected before a client is constructed. This checks
the credential format, not its validity with Anthropic. Keys are not added to model prompts or
session checkpoints. Codex can independently use a ChatGPT subscription through
`tact auth --provider codex login`, including in mixed-provider task trees.

For an API key that is not scoped to a workspace, set `claude.workspace_id` to the ID from
[Console Settings → Workspaces](https://platform.claude.com/settings/workspaces). Tact sends it
as `anthropic-workspace-id` for Claude roots, children, and auxiliary agents. Workspace-scoped
keys can leave this unset. Start a new session after changing it.

The `[claude]` section accepts `enabled`, `api_key`, `api_base_url`, and `workspace_id`. Unknown fields,
including the retired `auth` and `subscription_store` settings, are rejected. Remove those
fields and supply an API key to use Claude.

`claude.api_base_url` selects a Messages API base URL; Tact appends `/messages`.

Claude uses Tact's Code Mode `exec`/`wait` runtime, including nested tools, memory, MCP, and
mixed-provider subagents. At this pinned upstream version, Claude cannot fork a conversation or
change effort after its first prompt; start a new session to choose another effort. Claude usage
has no dollar estimate from upstream. These limits are reported without substituting Codex
behavior or pricing.

See the [Claude integration](docs/claude-integration.md) for authentication ownership, Code Mode
cleanup, and validation boundaries.

## Configuration

The configuration file is optional. Tact reads `$TACT_HOME/config.toml`, or
`~/.tact/config.toml` when `TACT_HOME` is unset. Select another file with `--config PATH` or
`TACT_CONFIG`.

Use `config show` to discover every available field and inspect the complete effective
configuration after file, environment, command-line, and default values have been applied:

```sh
tact config path
tact config show
```

`agent.speed` stores the requested preference even when the current model uses a slower supported
speed. Existing `agent.fast_mode` booleans load as standard or fast; an explicit `agent.speed` wins.
Saving a speed choice writes `agent.speed` and removes the legacy key.

The default effective configuration looks like this (paths depend on your environment):

```toml
[auth]
mode = "auto" # auto, chatgpt, or api-key
file = "/path/to/.codex/auth.json"

[openai]
api_key = ""

[agent]
workspace = "/path/to/workspace"
model = "sol" # luna, sol, or astra
thinking = "low" # low, medium, high, xhigh, or max
reasoning_mode = "standard" # standard or pro
speed = "standard" # standard, fast, or ultrafast
max_subagents = 32
instructions = ""
append_instructions = ""
web_search = true
image_generation = true
websocket_url = ""
api_base_url = ""
transport = "websocket" # websocket or https
completion_hook = ""

[mcp_servers]

[skills]
enabled = false
roots = []

[memory]
enabled = false

[memory.local]
max_records = 512
max_record_bytes = 1024
max_total_bytes = 262144

[subagents]
enabled = true

[tui]
mouse_scroll_lines = 3

[theme]
mode = "auto" # auto, light, or dark

[theme.light]
text = "reset"
border = "dark-gray"
muted = "dark-gray"
accent = "blue"
code_text = "#262626"
code_background = "#EEEEEE"
thinking_low = "dark-gray"
thinking_medium = "#007878"
thinking_high = "#9A6700"
thinking_xhigh = "red"
thinking_max = "magenta"
model_luna = "reset"
model_sol = "yellow"
model_astra = "magenta"
model_sonnet = "green"
model_opus = "red"
model_fable = "cyan"

[theme.dark]
text = "reset"
border = "dark-gray"
muted = "dark-gray"
accent = "blue"
code_text = "#D7D7D7"
code_background = "#262626"
thinking_low = "gray"
thinking_medium = "cyan"
thinking_high = "yellow"
thinking_xhigh = "red"
thinking_max = "magenta"
model_luna = "reset"
model_sol = "yellow"
model_astra = "magenta"
model_sonnet = "green"
model_opus = "red"
model_fable = "cyan"
```

Set `agent.completion_hook` to a shell command to run after each conversation turn finishes. Tact
runs the command in the configured workspace and ignores its output and exit status. Interactive
sessions start the hook asynchronously so it does not block the UI; `tact run` waits for the hook
before exiting.

The workspace defaults to the directory where tact starts. Relative paths in the configuration are
resolved from the configuration file's directory; relative command-line paths are resolved from the
current directory. Command-line options take precedence over environment variables, which take
precedence over the file.

New sessions append concise built-in guidance for orchestrating related tool calls in code mode.
When subagents are enabled, they also append guidance for delegation and multi-agent pipelines.
Configured `append_instructions` follow that guidance.

Tact loads global instructions from `AGENTS.override.md` or `AGENTS.md` in `CODEX_HOME`, which
defaults to `~/.codex`, followed by project instructions from the Git repository root through the
configured workspace.

The main agent options can also come from the environment. For example, `--workspace`,
`--thinking`, and `--resume` correspond to `TACT_WORKSPACE`, `TACT_THINKING`, and `TACT_RESUME`.
The prompt for `tact run` can be supplied through `TACT_PROMPT`. Run `tact --help` for the complete
command-line reference.

The `/subagents` panel shows the current concurrency limit. Use `-` and `+` there to update it.

Use **Reload config** in the Actions menu after editing the file. Theme and UI changes apply immediately.
Most agent settings apply when a session starts or is restored, while effort and fast mode can also
be changed during a session. Workspace changes require restarting tact.

Set `[tui].mouse_scroll_lines` to control transcript rows scrolled per mouse-wheel event.
The default is `3`; valid values are `1` through `65535`. Use `1` for finer scrolling. The setting
applies to main, fork, and subagent transcripts. Use **Reload config** to apply a new value.

### Themes

All theme options can be set directly under `[theme]` to apply to both palettes:

```toml
[theme]
mode = "auto" # auto, light, or dark
text = "reset"
border = "dark-gray"
muted = "dark-gray"
accent = "blue"
code_text = "#D7D7D7"
code_background = "#262626"
thinking_low = "gray"
thinking_medium = "cyan"
thinking_high = "yellow"
thinking_xhigh = "red"
thinking_max = "magenta"
model_luna = "reset"
model_sol = "yellow"
model_astra = "magenta"
model_sonnet = "green"
model_opus = "red"
model_fable = "cyan"
```

The `model_*` colors apply to the model picker, composer, and subagent displays. Their defaults
use the terminal's foreground for Luna and its yellow, magenta, green, red, and cyan slots for
the other models in both light and dark mode, so the terminal theme controls their appearance. Set a color to an RGB value for a fixed override.
Put any of the color options under `[theme.light]` or `[theme.dark]` to override that palette. Colors
may be Ratatui names, indexed values such as `239`, or RGB values such as `"#AABBCC"`. Auto mode
follows the operating-system theme while tact is running.

### Custom Endpoints

Advanced deployments can set `agent.websocket_url` and `agent.api_base_url`, or use the
`--websocket-url` and `--api-base-url` options. Leave them unset to use Nanocodex's defaults for the
selected authentication method. Set `agent.transport = "https"` (or `--transport https`) for proxies
that do not accept Responses WebSocket connections; tact then streams over HTTPS only.

## Features

### Subagents

Subagents are enabled by default. Disable their tools and built-in delegation instructions with:

```toml
[subagents]
enabled = false
```

The reusable runtime and Nanocodex tool surface are published as the `tact-subagents` crate.

This setting applies when a session starts or is restored. Reloading the configuration does not
change the tool surface of an already-running session. `agent.max_subagents` controls concurrency
when the feature is enabled; setting it does not enable or disable subagents. See the
[subagent design](docs/subagents.md) for the tool, lifecycle, messaging, and authority contracts.

Agents explicitly choose a model and `thinking` for each delegated task. The default choices are
`luna`, `sol`, and `astra`; enabling Claude adds `sonnet-5.5`, `opus-5.5`, and `fable-5.1` for both
root and child agents. Agents may mix providers within one task tree. Each turn receives its own
model and effort in context. When both parent and child use Codex, the child cannot exceed the
parent in the order Luna < Sol < Astra. Cross-provider selection and delegation between Claude
models are allowed.
Tact's subagent instructions include tips for choosing a model, reasoning effort, and when
to seek an independent opinion.
Root agents use the live `agent.thinking` cap for new spawns, including after an update during an
active turn. Registered subagents are also bounded by their own assigned effort. Changing the cap
leaves existing children unchanged. Model selection has no per-model configuration switches or
`selected` alias.

Optimize total cost and time to a correct result, including rework. Use `low` for mechanical work,
`medium` for localized implementation, `high` for bounded difficult correctness proofs, `xhigh`
for interacting contracts or competing designs, and `max` for the hardest integrated proofs or
architecture. Choose higher effort upfront when it is likely to avoid repeated weaker runs.

When a completed answer needs stronger reasoning, pass the original request, constraints, result,
evidence, and unresolved questions to a stronger child within the caps. A child that needs more
capability than its own cap must return that package to a capable ancestor.

### Memory

Tact's bounded cross-session memory is disabled by default. Opt in explicitly:

```toml
[memory]
enabled = true
```

Local memory is global to the selected Tact configuration, not scoped to a workspace. Tact stores it
in `memory/v1.sqlite3` beside the selected `config.toml`. Set `memory.local.max_records`,
`memory.local.max_record_bytes`, and `memory.local.max_total_bytes` to positive integers to
independently limit the record count, UTF-8 content bytes per record, and total content bytes. The
defaults are 512 records, 1 KiB per record, and 256 KiB total. These limits also apply to local
snapshots used by explicit push and pull commands. Remote limits are configured by the service.
The Cloudflare example applies the same three limits separately to each namespace.

Agents access the selected local or remote backend only through explicit memory tool calls, and the
corpus is never inserted into prompts automatically. For later user messages and in-flight steers,
Tact adds a fixed, content-free checkpoint asking the agent to review the conversation and update
memory when it finds a durable conclusion. See the [global memory design](docs/memory.md) for the
tool contract, limits, privacy model, and evaluation criteria.

To share memory with a team, configure an authenticated remote backend. Each person uses a distinct
namespace and may receive either writer or read-only credentials. Tact chooses exactly one backend
for each runtime: remote inside a configured workspace root or a linked worktree from one, and local
outside all configured roots.

```toml
[memory.remote]
endpoint = "https://memory.example.com/"
namespace = "alice"
bearer_token = "replace-with-a-secret-token"
workspace_roots = ["/path/to/team-projects"]
```

Keep the configuration file private with mode `0600`: it contains the bearer token directly. Remote
memory with a direct token currently requires Unix so Tact can verify the file permissions.
`tact config show` and debug output redact the token. An in-scope remote error is returned to the
caller and never falls back to local memory. Runtime operations never push local records.

Use `tact memory push [--dry-run]` from any directory to reconcile the complete global local
store to the writer's personal namespace. Use `tact memory pull --all` or repeat
`--namespace NAME` to non-destructively merge remote records into the local schema v1
store. The [memory guide](docs/memory.md#remote-memory) includes the selection and transfer
contracts, the remote HTTP contract, and an in-memory server walkthrough.

Config reload applies memory-browser availability immediately. Like other agent tool and prompt
settings, the agent-facing memory setting applies when a new session starts or is restored; an
already-running agent retains the tool surface and instructions with which it was created.

### Manual compaction

Enter `/compact` or choose **Compact context** from the Actions menu while the session is idle.
Tact uses the selected provider's native compaction and shows **Compacting context…** in the composer.
Successful compaction updates the saved session, so resume uses the compacted context. Failed compaction
does not replace the previous saved state.

### Reflection

Choose **Reflect on session** from the Actions menu while the session is idle. The composer accepts
optional instructions for the reflection; press Enter with an empty composer to use the default
scope, or press Escape to cancel. Tact submits the reflection to the existing agent so it can use
the conversation already in context.

The transcript shows a muted **Reflection started** marker instead of the internal prompt, followed
by the agent's report as a normal assistant response. Reflection is read-only: the report ends with
findings and recommended actions for discussion, and memory updates or other durable actions require
a later explicit request.

The built-in workflow starts with the current conversation, uses `find_sessions` to discover a
bounded set of relevant historical sessions, then uses `read_session` to inspect only the strongest
candidates. Parent-session lineage helps the agent avoid counting related forks as independent
evidence. It checks supported lessons against existing memories and active instructions, and
recommends whether each lesson belongs in memory, always-on configuration, or nowhere. Additional
instructions can narrow the topic or expand the workspace and task-family scope. The report states
the coverage and uncertainty of its evidence; it does not apply its recommendations.

### Web Interface

Every running Tact serves a web interface in the background. It is a second front-end onto the
sessions that process runs: the terminal and the browser share the active session and each
session's draft, queue, settings, and transcript, and either can start, switch, fork, and close
sessions. The terminal must stay open (there is no headless mode). The interface binds to
`127.0.0.1:7878` by default (the next free port is used when that one is taken); put it behind
Tailscale, an SSH forward, or similar to reach it from elsewhere. For Tailscale, set
`tailscale = true` under `[web]` and Tact publishes the interface to your tailnet with
`tailscale serve` the first time you ask for a QR code (Tact checks that Tailscale is online each
time, and the local interface works regardless); for any other tunnel, set `public_url` to its
address. The two settings are mutually exclusive.

Open the login URL Tact shows for the web interface. Its fragment carries the machine token, which
is stored in `~/.tact/web/token` (mode 0600) and shared by every Tact instance of your user. The
token grants the same access as a shell, so treat the URL like a password. Configure the server in
the `[web]` section (`enabled`, `bind`, `port`, `public_url`, `tailscale`, `max_live_sessions`) or pass
`--web=false` (`TACT_WEB`) to turn it off. The design is described in [docs/web.md](docs/web.md).

The review tool lives in the interface's side panel. It shows the full branch from trunk by
default; you can narrow the range, inspect the live diff, leave overall or inline feedback, and
**Send to chat**, which writes the review as Markdown into the active session's draft so you can
edit it before sending. Inline selections can open a private question thread with the agent, and an
agent-authored visual overview of the selected range can be generated on demand. Overviews,
AI reviews, and question threads belong to a session, run as clean-context prompts on that
session's worker, and stop when the session closes.

The browser files are a separate bundle. Official releases publish `tact-web-v<version>.tar.gz`; the
server serves the bundle installed at `~/.tact/web/assets/v<version>` (it is re-checked on request
while missing, so installing it needs no restart). Until it is installed the server answers with a
page that explains how to install it. Official releases download and verify the matching bundle
in the background on first start.

#### Developing the web interface

Development builds do not download browser assets. Install Bun, then build and link the assets into
the development Tact directory:

```sh
cd web
bun install --frozen-lockfile
just install-dev
```

To work on the interface in a browser with sample data, run:

```sh
just dev
```

`TACT_WEB_ASSETS=/absolute/path/to/web/dist` remains available as a manual override. The
development server watches browser sources, rebuilds them, and reloads connected pages.

### Copying Responses

Type `/copy` and press Enter to copy the latest completed assistant message as raw Markdown.
Use `/copy N` to copy the Nth most recent completed assistant message (`/copy 1` is the latest).
Empty messages, reasoning, tool output, and messages still streaming are skipped. Copying uses
only the current pane's transcript, including restored history, and works while a turn is running.
You can also choose **Copy response** from the `/` Actions menu.

### Session Forking

Press `Ctrl+T` or choose **Fork session** from the Actions menu to open an independent session next
to the current one. The fork starts from the stable conversation history available at that point;
new prompts, model responses, and transcripts then remain independent in each pane.

Tact supports one open fork at a time. Close the fork before creating another. Forked sessions are
persisted separately and can be resumed like other sessions.

### Resume

Tact checkpoints each successful turn and keeps an append-only transcript. Ordinary failed turns
retain the last successful checkpoint. A terminal provider policy stop makes the session
non-resumable; its transcript remains available for inspection. Open **Resume session**
from the Actions menu to search sessions for the current workspace, or resume a known ID directly:

```sh
tact --resume SESSION_ID
```

Tact prints the active session's resume command when it exits. Sessions are stored in
`sessions/v2.sqlite3` beside the selected configuration.
Checkpoints contain the complete model-visible conversation and are not redacted, so treat them as
private data.

### Skills

Skills are local `SKILL.md` files containing instructions the model can choose to follow. They are
disabled by default to avoid adding their catalogs to every session's persistent context. Skills
can also direct tool and shell execution, so enable only directories you trust:

```toml
[skills]
enabled = true
roots = ["skills", "/path/to/shared-skills"]
```

Type `$` at a token boundary in the composer to search the active session's skills. Enter or Tab
inserts the selected `$skill-name` into the prompt.

When enabled, tact also searches `$CODEX_HOME/skills` (or `~/.codex/skills`) and
`~/.agents/skills`. A new session discovers the current set of skills. Restored sessions keep the
skill catalog they started with so their instructions remain stable.

### MCP Servers

Tact supports local stdio servers and remote Streamable HTTP servers. Add a local server with:

```sh
tact mcp add filesystem -- \
  npx -y @modelcontextprotocol/server-filesystem /path/to/workspace
```

Use `--cwd PATH` to set its working directory. To pass a secret from tact's environment, put
`--env NAME` before `--`; tact copies the value into that server's configuration without placing it
in shell history or process arguments:

```sh
tact mcp add --env API_TOKEN private-server -- command --flag
```

The resulting TOML contains the copied value. `tact config show` redacts it, but you should still
protect the configuration file as you would any credential file.

Remote servers refer to environment-variable names instead of storing their values:

```sh
tact mcp add docs --url https://example.com/mcp \
  --bearer-token-env-var DOCS_MCP_TOKEN \
  --header-env X-Tenant-ID=DOCS_TENANT_ID
```

Remote URLs must use HTTP or HTTPS and cannot contain embedded credentials. Each server starts
independently, so a broken server does not prevent the session or other servers from working.
