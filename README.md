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
cargo install --path .
```

### Updates

Official release binaries can update themselves:

```sh
tact update
```

The updater verifies both the release checksum and signature before replacing a release-installer
binary. If Cargo owns the installation, tact instead prints `cargo install tact --locked` so
Cargo's records stay accurate. Automatic update notifications are shown only by official release
builds.

## Sign in

By default, tact uses the ChatGPT session stored by Codex in `$CODEX_HOME/auth.json` or
`~/.codex/auth.json`. If that file does not exist, it looks for `OPENAI_API_KEY`.

To sign in with a ChatGPT subscription:

```sh
tact auth login
tact auth status
```

`tact auth logout` removes the shared credential file, which also signs Codex out. If you want to
require API-key authentication, pass the key through the environment:

```sh
export OPENAI_API_KEY="your-api-key"
tact --auth api-key
```

API keys are never written to tact's configuration or shown in status output.

## Non-interactive use

For scripts and integrations, `tact run` submits one prompt and streams Nanocodex events as JSONL:

```sh
tact run "inspect the workspace"
```

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

The default effective configuration looks like this (paths depend on your environment):

```toml
[auth]
mode = "auto" # auto, chatgpt, or api-key
file = "/path/to/.codex/auth.json"

[agent]
workspace = "/path/to/workspace"
thinking = "medium" # low, medium, high, xhigh, or max
reasoning_mode = "standard" # standard or pro
fast_mode = false
max_subagents = 32
instructions = ""
append_instructions = ""
web_search = true
image_generation = true
websocket_url = ""
api_base_url = ""

[mcp_servers]

[skills]
enabled = false
roots = []

[memory]
enabled = false

[subagents]
enabled = true

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
```

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

Use **Reload config** in the Actions menu after editing the file. Theme changes apply immediately.
Most agent settings apply when a session starts or is restored, while effort and fast mode can also
be changed during a session. Workspace changes require restarting tact.

### Subagents

Subagents are enabled by default. Disable their tools and built-in delegation instructions with:

```toml
[subagents]
enabled = false
```

This setting applies when a session starts or is restored. Reloading the configuration does not
change the tool surface of an already-running session. `agent.max_subagents` controls concurrency
when the feature is enabled; setting it does not enable or disable subagents. See the
[subagent design](docs/subagents.md) for the tool, lifecycle, messaging, and authority contracts.

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
```

Put any of the color options under `[theme.light]` or `[theme.dark]` to override that palette. Colors
may be Ratatui names, indexed values such as `239`, or RGB values such as `"#AABBCC"`. Auto mode
follows the operating-system theme while tact is running.

### Custom endpoints

Advanced deployments can set `agent.websocket_url` and `agent.api_base_url`, or use the
`--websocket-url` and `--api-base-url` options. Leave them unset to use Nanocodex's defaults for the
selected authentication method.

## MCP servers

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

## Skills

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

## Shared memory

Tact's bounded cross-session memory is disabled by default. Opt in explicitly:

```toml
[memory]
enabled = true
```

Memory is global to the selected Tact configuration, not scoped to a workspace. Tact stores it in
`memory/v1.sqlite3` beside the selected `config.toml`, and agents access it only through explicit
memory tool calls. The corpus is never inserted into prompts automatically. For later user
messages and in-flight steers, Tact adds a fixed, content-free checkpoint asking the agent to
review the conversation and update memory when it finds a durable conclusion. See the
[global memory design](docs/memory.md) for the tool contract, limits, privacy model, and evaluation
criteria.

Config reload applies memory-browser availability immediately. Like other agent tool and prompt
settings, the agent-facing memory setting applies when a new session starts or is restored; an
already-running agent retains the tool surface and instructions with which it was created.

## Review changes

Enter `/review` while no agent work is active to open a browser-based human review tool for
workspace changes. Tact shows the full branch from trunk by default; you can narrow the range,
inspect the diff, leave overall or inline feedback, and approve or request changes. Tact converts
your review to Markdown and inserts it into the composer so you can edit it before passing it to the
agent. Inline selections can also open a private question thread with the agent; those conversations
help you understand the code but are not included in the rendered review feedback. The browser can
also generate an agent-authored visual overview of the selected range on demand.

Reloading or closing the browser does not cancel the review. Agent question threads remain available,
and an answer already in progress continues in the background. Reopen the live URL shown by Tact,
or cancel explicitly from the browser or Tact.

The review tool requires a separate browser bundle. The first `/review` from each official Tact
version asks for confirmation, then lazily downloads and verifies the matching release artifact.
The bundle is downloaded once per Tact version and stored under `~/.tact/review` by default.

### Developing the review interface

Development builds do not download browser assets. Install Bun, then build and link the assets into
the development Tact directory:

```sh
cd web/review
bun install --frozen-lockfile
just install-dev
```

To work on the interface in a browser with sample review data, run:

```sh
just dev
```

`TACT_REVIEW_ASSETS=/absolute/path/to/web/review/dist` remains available as a manual override. The
development server watches browser sources, rebuilds them, and reloads connected pages.

## Sessions and local data

Tact checkpoints each completed turn and keeps an append-only transcript. Open **Resume session**
from the Actions menu to search sessions for the current workspace, or resume a known ID directly:

```sh
tact --resume SESSION_ID
```

Tact prints the active session's resume command when it exits. Session files live beside the
selected configuration in private, versioned `checkpoints` and `transcripts` directories.
Checkpoints contain the complete model-visible conversation and are not redacted, so treat them as
private data.
