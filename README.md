# tact

[![GitHub Actions Workflow Status](https://img.shields.io/github/actions/workflow/status/clabby/tact/ci.yaml?style=for-the-badge&label=CI)](https://github.com/clabby/tact/actions/workflows/ci.yaml)
[![Crates.io License](https://img.shields.io/crates/l/tact?style=for-the-badge)](https://crates.io/crates/tact)
[![Crates.io MSRV](https://img.shields.io/crates/msrv/tact?style=for-the-badge)](https://crates.io/crates/tact)
[![Crates.io Version](https://img.shields.io/crates/v/tact?style=for-the-badge)](https://crates.io/crates/tact)

Tact is a coding agent for your terminal, built on [Nanocodex](https://github.com/gakonst/nanocodex).
It runs OpenAI's GPT-6 models out of the box and can also drive Anthropic's Claude models. Alongside
the terminal UI, every Tact process serves a web interface you can open from a browser or your phone,
with a built-in code review panel. Agents can delegate work to subagents, keep a small long-term
memory across sessions, load skills, and call MCP servers.


<https://github.com/user-attachments/assets/4d19d0ad-f811-402b-82f6-f4961f88c2a7>

- [Install](#install)
- [Set up](#set-up)
- [Web interface](#web-interface)
- [Subagents](#subagents)
- [Memory](#memory)
- [Models, effort, and speed](#models-effort-and-speed)
- [Sessions](#sessions)
- [Skills](#skills)
- [MCP servers](#mcp-servers)
- [Scripting](#scripting)
- [Configuration reference](#configuration-reference)
- [Developing the web interface](#developing-the-web-interface)

> [!WARNING]
> Tact does not sandbox agent commands. The agent can read and change files and run processes with
> your user's permissions. If you want isolation, the example
> [development container](docker/dev/README.md) runs Tact against a mounted workspace and keeps your
> real OpenAI credentials outside the container.

## Install

The install script supports x86-64 and ARM64 Linux (glibc) and both Intel and Apple Silicon Macs.

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://tact.clab.by/install.sh | sh
```

It checks the release checksum and installs `tact` into `~/.local/bin` without `sudo`. Set
`TACT_INSTALL_DIR` to an absolute path to install somewhere else.

When run in a terminal, the script asks whether you want the latest signed **release** (the default)
or the latest **pre-release** build of `main`. Without a terminal it installs the release. To skip
the question, set `TACT_CHANNEL` to `release` or `pre-release`.

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://tact.clab.by/install.sh | TACT_CHANNEL=pre-release sh
```

You can also install from crates.io or from source.

```sh
cargo install tact --locked

# or
git clone https://github.com/clabby/tact.git
cd tact
cargo install --locked --path bin/tact
```

### Updating

Release binaries, whether from the install script or a release archive, update themselves with
`tact update`. It verifies the release checksum and signature before replacing the binary. If
Cargo installed Tact, `tact update` prints the `cargo install` command instead so Cargo's records
stay correct. Every installation except a source build shows a notice when a new release is out.
Source builds show a red `dev` badge in the composer footer.

### Pre-release builds

Every change merged to `main` is published as a pre-release. To try one, pass at least the first
seven hex digits of its commit.

```sh
tact update 0123abc
```

A pre-release is a full build with the same platform archives and web bundle as a release. It shows
a blue `pre-release` badge in the composer, and `tact --version` prints its channel and commit.

- A pre-release still gets notified when an official release with a later version comes out.
- Plain `tact update` takes you back to the latest release, even if the version number matches.
- Official releases are checked against a signing key published in the crates.io package.
  Pre-releases aren't on crates.io, so Tact only checks the archive's SHA-256 checksum. That makes a
  pre-release exactly as trustworthy as the GitHub Releases of `clabby/tact`. Stick to official
  releases if that isn't enough.
- Only the five newest pre-releases are kept. Older commits, or commits whose release workflow
  hasn't finished, have no build.
- `tact update <commit>` replaces the binary that runs it. That works for install-script and
  archive installs, and for source builds (the download sits in `target/` until the next
  `cargo build`). It won't replace an install that Cargo or a package manager owns.
- Container images work the same way. `ghcr.io/clabby/tact:dev` tracks the latest pre-release
  and `ghcr.io/clabby/tact:dev-<commit>` pins one. `latest` and version tags are official releases.

### Packaging

Packagers can declare which package manager owns the install at build time.

```sh
TACT_PACKAGE_MANAGER=nix cargo build --release
```

These builds keep update notifications and the web interface, but `tact update` points users at the
package manager. When building from a source archive without git history, set `TACT_GIT_SHA`,
`TACT_GIT_BRANCH`, `TACT_GIT_COMMIT_TIMESTAMP`, and `TACT_GIT_DIRTY` to fill in `tact --version`.

## Set up

Tact works with no configuration if you already use Codex, but several features are off by default.
This section gets you signed in and turns everything on.

### Sign in

By default Tact reuses the ChatGPT login that Codex stores in `$CODEX_HOME/auth.json` (or
`~/.codex/auth.json`). To sign in with a ChatGPT subscription from Tact itself, run these.

```sh
tact auth login
tact auth status
```

The credential file is shared, so `tact auth logout` also signs Codex out.

If there's no ChatGPT login, Tact falls back to `OPENAI_API_KEY`, then to `openai.api_key` in the config
file. To always use an API key, even when a ChatGPT login exists, set `auth.mode = "api-key"` or pass
`--auth api-key`. A nonblank `OPENAI_API_KEY` beats the configured key. If a credential is rejected, Tact
reports the error rather than switching to another credential or billing source.

The same OpenAI credentials power web search and image generation, including in Claude sessions.

### The config file

The config file is optional. Tact reads `$TACT_HOME/config.toml`, or `~/.tact/config.toml` when
`TACT_HOME` is unset. Use `--config PATH` or `TACT_CONFIG` to pick another file. Two commands help.

```sh
tact config path   # where the active file lives
tact config show   # every field, with file, env, CLI, and defaults applied
```

Command-line options win over environment variables, which win over the file. Relative paths in
the file resolve from the file's directory.

If the file holds an OpenAI or Anthropic API key or a memory token, Tact refuses to load it unless
only your user can read it. On platforms where Tact can't check file permissions, use environment
variables for those secrets instead. `tact config show` and `tact auth status` never print keys, and
`auth status` tells you which source is in use.

```sh
chmod 600 ~/.tact/config.toml
```

### Turn on the extras

Memory, skills, decisions, and Claude are off by default. Here's a config that turns all of them on.

```toml
[memory]
enabled = true

[skills]
enabled = true
roots = ["/path/to/your/skills"]

[decisions]
enabled = true

[claude]
enabled = true
api_key = "sk-ant-api03-..." # or set ANTHROPIC_API_KEY

[web]
tailscale = true # optional, lets you reach the web UI from your tailnet
```

Subagents and the web interface are on by default. Each feature has its own section below. After
editing the file, choose **Reload config** from the Actions menu (`/`). Theme and UI changes apply right
away. Most agent settings apply when a session starts or resumes, but effort and speed can also change
mid-session. Changing the workspace needs a restart.

### Instructions

Tact loads global instructions from `AGENTS.override.md` or `AGENTS.md` in `CODEX_HOME` (default
`~/.codex`), then project instructions from the Git repository root down to the workspace. Use
`agent.instructions` to replace the base instructions and `agent.append_instructions` to add your own.
New sessions also get short built-in guidance on using code mode and, when subagents are on, on
delegation. Your appended instructions come after it.

## Web interface

Every running Tact serves a web interface in the background. It's a second front end on the same
process, not a separate client. The terminal and browser share the active session and each
session's draft, queue, settings, and transcript. Either one can start, switch, fork, and close
sessions. There is no headless mode, so the terminal has to stay open.

Open the login URL that Tact shows. The token in its fragment is stored in `~/.tact/web/token` (mode
0600) and shared by all of your Tact instances. **It grants the same access as a shell**, so treat
the URL like a password.

### Remote access

The server listens on `127.0.0.1:7878` and picks the next free port if that one is taken. To reach it
from another device, you have two options.

- Set `web.tailscale = true`. The first time you ask for a QR code, Tact publishes the interface to
  your tailnet with `tailscale serve`. It checks that Tailscale is online each time, and the
  local interface works either way.
- For an SSH forward or any other tunnel, set `web.public_url` to the tunnel's address.

You can't set both.

### Code review

The side panel has a review tool. By default it shows the whole branch from trunk. You can narrow
the range, watch the live diff, and leave overall or inline comments. **Send to chat** writes the
review as Markdown into the session's draft so you can edit it before sending. You can also select
lines and open a private question thread with the agent, or ask for an agent-written visual
overview of the range. Overviews, AI reviews, and question threads belong to a session, run as
clean-context prompts on its worker, and stop when the session closes. The review panel can target
any checkout of the session's repository. See [workspaces](docs/workspaces.md) for how that works.

### Settings and assets

Configure the server under `[web]` (`enabled`, `bind`, `port`, `public_url`, `tailscale`,
`max_live_sessions`), or turn it off with `--web=false` or `TACT_WEB`.

The browser files ship as a separate bundle (`tact-web-v<version>.tar.gz`). Releases and pre-releases
download and verify the matching bundle in the background on first start and install it under
`~/.tact/web/assets/`. Until it's there, the server shows a page explaining how to install it, and it
picks the bundle up without a restart. Source builds read `~/.tact/web/assets/development` instead. See
[Developing the web interface](#developing-the-web-interface) for setup.

The full design is in [docs/web.md](docs/web.md).

## Subagents

Subagents are on by default. An agent can hand focused tasks to clean child sessions, message
them, and collect typed results, without sharing its own conversation history. For each task it
picks a model and a `thinking` effort. Tact's built-in instructions include a guide for making that
choice.

A few limits apply. A Codex child can't use a higher tier than its Codex parent (Luna < Sol <
Astra). No spawn can exceed the current `agent.thinking` setting, and nested children can't exceed
their parent's effort. `agent.max_subagents` caps concurrency (default 32), and you can also adjust it with
`-` and `+` in the `/subagents` panel.

To turn subagents off, along with their tools and delegation instructions, set this.

```toml
[subagents]
enabled = false
```

This applies when a session starts or resumes. The runtime is also published as the
`tact-subagents` crate. See the [subagent design](docs/subagents.md) for details.

## Memory

Memory lets agents keep a small set of durable conclusions across sessions, such as your
preferences or hard-won facts about a codebase. It's off by default, so turn it on like this.

```toml
[memory]
enabled = true
```

Like other agent settings, turning memory on or off affects new and resumed sessions. The memory
browser updates as soon as you reload config.

Agents read and write memory only through explicit tool calls. Tact never pastes the memory corpus
into prompts. With each later user message or steer, Tact adds a short, fixed reminder asking the agent to
review the conversation and save anything durable.

Local memory is global to your Tact config, not per workspace. It lives in `memory/v1.sqlite3` next
to `config.toml`. By default it holds up to 512 records, 1 KiB each, and 256 KiB total. Change these
with `memory.local.max_records`, `max_record_bytes`, and `max_total_bytes`.

### Team memory

To share memory with a team, point Tact at a remote backend. Each person gets their own namespace
and either writer or read-only credentials.

```toml
[memory.remote]
endpoint = "https://memory.example.com/"
namespace = "alice"
bearer_token = "replace-with-a-secret-token"
workspace_roots = ["/path/to/team-projects"]
```

Each session uses exactly one backend. It's remote inside a listed workspace root (or a linked
worktree of one) and local everywhere else. If the remote fails, the error goes back to the agent,
and Tact never quietly falls back to local memory. Remote limits are set by the service, and the
Cloudflare example applies the same three limits per namespace. A direct bearer token currently
requires Unix, so Tact can check the file's permissions.

These commands move records between backends.

```sh
tact memory push --dry-run       # show what a push would send
tact memory push                 # make your remote namespace match local memory
tact memory pull --all           # merge remote records into local memory
tact memory pull --namespace bob # or pick namespaces
```

Push needs a writer credential and treats local memory as the source of truth. It deletes records
in your namespace that don't exist locally, so run `--dry-run` first. Pull only merges and never
deletes local records. Normal agent operations never push. The [memory guide](docs/memory.md)
covers backend selection, the HTTP contract, privacy, and a local server walkthrough.

## Decisions

Decisions give agents a `decide` tool backed by OpenAI's
[Decisions API](https://developers.openai.com/api/docs/guides/decisions). Instead of writing prose,
it answers typed questions about text or images: yes/no probabilities, a pick from a fixed set of
options, or a score on a rubric. Agents use it for quick classification, routing, and triage.
Decisions are off until you turn them on.

```toml
[decisions]
enabled = true

[openai]
decisions_api_key = "sk-..." # only needed with a ChatGPT login and no API key
```

The Decisions API takes an OpenAI API key, and a ChatGPT subscription can't pay for it. Tact uses
`OPENAI_API_KEY` or `openai.api_key` when either is set, whatever `auth.mode` says. Otherwise it uses
`openai.decisions_api_key`. With decisions on and no key available, sessions fail to start and the
error names the settings to fix.

The tool works in Codex and Claude sessions, including subagents and code mode. Changes apply when a
session starts or resumes.

## Models, effort, and speed

New sessions use Sol unless you configure a model. Pick one with `agent.model`, `--model`,
`TACT_MODEL`, or the model picker. `--model` takes either the short name or the full ID. The
picker filters as you type by name, provider, or ID. Use Up and Down to select, Enter to apply, and
Esc to cancel.

| Model | ID | Default effort |
| --- | --- | --- |
| `luna` | `gpt-6-luna` | medium |
| `sol` | `gpt-6.1-sol` | low |
| `astra` | `gpt-6-astra` | low |
| `haiku-5.5` | `claude-haiku-5-5` | medium |
| `sonnet-5.5` | `claude-sonnet-5-5` | high |
| `opus-5.5` | `claude-opus-5-5` | medium |
| `fable-5.1` | `claude-fable-5-1` | high |

Claude models need Claude enabled (see below). Every model supports effort from `low` through `max`,
and a configured `agent.thinking` overrides the defaults. Resumed sessions keep the model they started
with. Sessions on retired model IDs can't be resumed.

Astra, Sol, and Luna also support **Pro** mode, independent of effort. Open `/effort`, press `p` to toggle
Pro, and press Enter to save. Pro applies to new sessions.

### Speed

Click the speed icon or choose **Change speed** to switch between Standard, Fast, and Ultrafast (the
turtle, rabbit, and rocket icons, if you use a Nerd Font). Speed is separate from effort and sticks
when you change models. Ultrafast picks the fastest tier each model supports.

| Model | Ultrafast means |
| --- | --- |
| Astra, Sol | Ultrafast |
| Luna, Opus 5.5 | Fast |
| Haiku 5.5, Sonnet 5.5, Fable 5.1 | Standard |

Astra and Sol send `service_tier = "ultrafast"`, and access and pricing depend on your account. See
[OpenAI ultrafast mode](https://developers.openai.com/api/docs/guides/ultrafast-mode) for details.
Opus fast mode uses Anthropic's [premium fast mode](https://platform.claude.com/docs/en/build-with-claude/fast-mode),
which your API account needs access to. `agent.speed` stores your preference even when the current
model runs slower. The older `agent.fast_mode` boolean still loads, but `agent.speed` wins, and saving a
speed choice replaces it.

### Prompt caching

Both providers cache prompts automatically, and Tact keeps request prefixes stable across turns,
children, and resumed sessions so the cache gets reused. Hits depend on the provider and matching
context. GPT-6 caches last at least 30 minutes after a write or hit (see
[OpenAI prompt caching](https://developers.openai.com/api/docs/guides/prompt-caching)). Claude cache
entries last one hour and cover the system prompt, tools, and conversation prefix. Each hit resets
the timer. One-hour cache writes cost twice the base input rate, and cache reads are discounted. An
expired cache costs money and latency, not conversation history.

### Claude

Claude needs an explicit opt-in and an Anthropic API key.

```toml
[claude]
enabled = true
api_key = "sk-ant-api03-..."

[agent]
model = "sonnet-5.5"
thinking = "medium"
```

Check the key with `tact auth --provider claude status`. A nonblank `ANTHROPIC_API_KEY` overrides
`claude.api_key`. On platforms where Tact can't verify file privacy, use the environment variable.

Claude support is API-key only. Tact doesn't log into Claude subscriptions or read their
credentials, and it rejects keys that don't start with `sk-ant-api` or `sk-ant-usr-` (a format check,
not a validity check). Keys never go into prompts, session checkpoints, or shell and MCP
environments. Codex can still use a ChatGPT subscription in the same task tree via
`tact auth --provider codex login`.

The `[claude]` section accepts `enabled`, `api_key`, `api_base_url` (Tact appends `/messages`), and
`workspace_id`. Any other field is an error, including the retired `auth` and `subscription_store`
settings, so remove those if you're upgrading. If your key isn't scoped to a workspace, set
`workspace_id` to the ID from [Console workspace settings](https://platform.claude.com/settings/workspaces),
and Tact sends it as `anthropic-workspace-id`. Start a new session after changing it.

Claude sessions use the same code mode runtime as Codex, with nested tools, memory, MCP, and
mixed-provider subagents. Web search and image generation still use your OpenAI credentials, as do
Codex children. For an Anthropic-only setup, set `agent.web_search = false` and
`agent.image_generation = false`.

Claude has a few limits for now. It always uses standard reasoning mode, can't fork a conversation, and can't
change effort after the first prompt (start a new session instead). There's no dollar estimate for
Claude usage. See [Claude integration](docs/claude-integration.md) for more.

## Sessions

Tact saves a checkpoint after each successful turn and keeps an append-only transcript in
`sessions/v2.sqlite3` next to your config. A failed turn leaves the last good checkpoint in place. If a
provider stops a session for policy reasons, the session can't be resumed, but you can still read
its transcript. Checkpoints hold the full conversation unredacted, so treat them as private.

- **Resume.** Choose **Resume session** in the Actions menu to search this workspace's sessions, or
  run `tact --resume SESSION_ID`. Tact prints the resume command when it exits.
- **Fork.** Press `Ctrl+T` or choose **Fork session** to open an independent copy next to the current
  one. You can have one fork open at a time, and forks are saved and resumable like any session.
- **Compact.** Enter `/compact` or choose **Compact context** while idle. Tact uses the provider's native
  compaction and saves the result, so resuming picks up the compacted context. A failed compaction
  leaves the saved session alone.
- **Context.** Choose **Debug context** in the Actions menu, or **Context diagnostics** in the web
  command palette, to see what fills the window: the active size against the auto-compact limit,
  a breakdown by source (instructions and tools, prompts, assistant text, reasoning, tool calls and
  output, compacted history), the tools and single items that cost the most, context growth over
  recent calls with compactions marked, and prompt-cache hits. The total is the server's exact
  count; the split among sources is an estimate. It holds no conversation text.
- **Copy.** `/copy` copies the latest finished assistant message as raw Markdown, and `/copy N` copies
  the Nth most recent. It skips reasoning, tool output, empty messages, and anything still
  streaming, and works mid-turn. **Copy response** in the Actions menu does the same.

### Reflection

Choose **Reflect on session** while idle to have the agent review what it has learned. You can type
extra instructions to narrow or widen the scope, press Enter with an empty composer for the default,
or press Esc to cancel. The agent starts from the current conversation, finds related past sessions
with `find_sessions`, and reads the most promising ones with `read_session`. It uses session lineage so
forks don't count as independent evidence.

The report checks each lesson against existing memories and instructions and recommends whether it
belongs in memory, in always-on config, or nowhere. It also states how much evidence it looked at
and how sure it is. Reflection is read-only. It won't update memory or change anything until you
ask in a later message. The transcript shows a **Reflection started** marker instead of the
internal prompt.

### Messaging other sessions

Every session has the `find_sessions`, `read_session`, and `message_session` tools. `find_sessions` reports
each session's model and effort, whether it is `live` (open in any Tact process, detected through
its session lock), and whether it is `messageable` (open in this process). `live_only` limits a search to
live sessions. A session meant to watch and orchestrate others can combine them: find live
sessions, read their transcripts, and steer them.

`message_session` takes a target session ID and a message of at most 16 KiB. It always steers. The
message joins the target's running turn at its next safe boundary, or starts a new turn when the
target is idle. The target sees the text prefixed with the sending session's ID, and its transcript
records the message like a steer. The result says whether the target was `steered` or `started`.
It is not a reply. Read the target's transcript to see what it did next.

Only sessions open in the same Tact process can be messaged, including panes and forks beside the
current one. A session that is live in another process shows `live: true` but fails with a
not-live error. A session cannot message itself.

Tact doesn't prompt agents to use `message_session`. The tool description tells the model to use it
only when you explicitly ask it to message or steer another session. In the TUI and web UI the call
appears as a **Message** row reading `me → <target>` with the delivery outcome, styled like
subagent messages. `me` is the session whose transcript shows the call, which is always the sender.
The target's transcript records each message it receives as a **Message** entry reading
`<sender> → me`, with the message body and the sender's full session ID, whether the message
joined a running turn or started one.

## Skills

Skills are local `SKILL.md` files with instructions the model can choose to follow. They're off by default
because each session carries the skill catalog in its context. Skills can also tell the agent to
run tools and shell commands, so only enable directories you trust.

```toml
[skills]
enabled = true
roots = ["skills", "/path/to/shared-skills"]
```

When enabled, Tact also searches `$CODEX_HOME/skills` (or `~/.codex/skills`) and `~/.agents/skills`.
Type `$` in the composer to search skills, and press Enter or Tab to insert one. New sessions pick up
the current skills, and resumed sessions keep the catalog they started with.

## MCP servers

Tact supports local stdio servers and remote Streamable HTTP servers. Each server starts
independently, so one broken server doesn't take down the session or the others.

Here's how to add a local server.

```sh
tact mcp add filesystem -- \
  npx -y @modelcontextprotocol/server-filesystem /path/to/workspace
```

Use `--cwd PATH` to set its working directory. To pass a secret from your environment, put `--env NAME`
before `--`. Tact copies the value into the config file without putting it in shell history or process
arguments.

```sh
tact mcp add --env API_TOKEN private-server -- command --flag
```

`tact config show` redacts the value, but protect the file like any other credential.

Remote servers store environment variable names, not values.

```sh
tact mcp add docs --url https://example.com/mcp \
  --bearer-token-env-var DOCS_MCP_TOKEN \
  --header-env X-Tenant-ID=DOCS_TENANT_ID
```

Remote URLs must be HTTP or HTTPS and can't contain embedded credentials.

## Scripting

`tact run` sends one prompt and streams Nanocodex events as JSONL.

```sh
tact run "inspect the workspace"
tact --model luna run "inspect the workspace"
```

Most options have environment variable equivalents, such as `TACT_WORKSPACE`, `TACT_THINKING`, and
`TACT_RESUME`. `TACT_PROMPT` supplies the prompt for `tact run`. Run `tact --help` for the full list.

Set `agent.completion_hook` to a shell command to run after each turn, for example to send a
notification. It runs in the workspace, and Tact ignores its output and exit status. The terminal UI
runs it in the background, and `tact run` waits for it before exiting.

## Configuration reference

Run `tact config show` to see your effective settings. The defaults look like this (paths depend on
your machine).

<details>
<summary>Default configuration</summary>

```toml
[auth]
mode = "auto" # auto, chatgpt, or api-key
file = "/path/to/.codex/auth.json"

[openai]
api_key = ""
decisions_api_key = ""

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

[decisions]
enabled = false

[web]
enabled = true
bind = "127.0.0.1"
port = 7878
public_url = ""
tailscale = false
max_live_sessions = 8

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
model_haiku = "blue"
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
model_haiku = "blue"
model_sonnet = "green"
model_opus = "red"
model_fable = "cyan"
```

</details>

The workspace defaults to the directory you start Tact in.

### Themes

`theme.mode` is `auto`, `light`, or `dark`. Auto follows the OS theme while Tact runs. Put color
options directly under `[theme]` to set both palettes, or under `[theme.light]` or `[theme.dark]` to
set one. Colors can be Ratatui names, indexed values like `239`, or RGB values like `"#AABBCC"`.

The `model_*` colors are used in the model picker, composer, and subagent views. By default they use your
terminal's foreground (Luna) and its blue, yellow, magenta, green, red, and cyan slots, so your
terminal theme controls them. Set an RGB value to pin one.

### Scrolling

`tui.mouse_scroll_lines` sets how many transcript rows one mouse-wheel step scrolls, from `1` to `65535`
(default `3`). It applies to main, fork, and subagent transcripts.

### Custom endpoints

For advanced deployments, set `agent.websocket_url` and `agent.api_base_url` (or `--websocket-url`
and `--api-base-url`). Leave them unset to use Nanocodex's defaults for your auth method. If a proxy
doesn't accept Responses WebSocket connections, set `agent.transport = "https"` (or
`--transport https`) to stream over HTTPS only.

## Developing the web interface

Source builds don't download browser assets. Install [Bun](https://bun.sh), then build and link the
assets into your development Tact directory.

```sh
cd web
bun install --frozen-lockfile
just install-dev
```

Run `just dev` to work on the interface in a browser with sample data. The dev server watches the
sources, rebuilds, and reloads connected pages. `TACT_WEB_ASSETS=/absolute/path/to/web/dist` overrides the
asset directory by hand.
