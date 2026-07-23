# tact

`tact` is a terminal interface for [Nanocodex](https://github.com/gakonst/nanocodex).

https://github.com/user-attachments/assets/5c634ae8-5c74-47c9-bb8c-9c18cb7fc97d

## Installation

The release installer supports x86-64 and ARM64 glibc-based Linux, as well as Intel and Apple
Silicon Macs:

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://raw.githubusercontent.com/clabby/tact/main/install.sh | sh
```

It verifies the release checksum and installs `tact` in `~/.local/bin` without `sudo`. Set
`TACT_INSTALL_DIR` to another absolute directory if you prefer a different location.

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

The updater verifies both the release checksum and signature before replacing the executable. If
Cargo owns the installation, tact instead prints the appropriate `cargo install` command so Cargo's
records stay accurate. Automatic update notifications are shown only by official release builds.

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

These commands show which file is selected and the complete effective configuration, including
defaults:

```sh
tact config path
tact config show
```

A typical configuration looks like this:

```toml
[auth]
mode = "auto" # auto, chatgpt, or api-key
# file = "/path/to/.codex/auth.json"

[agent]
workspace = "/path/to/workspace"
thinking = "medium" # low, medium, high, xhigh, or max
fast_mode = false
web_search = true
image_generation = true
# instructions = "Replace the standard Nanocodex instructions"

[skills]
enabled = false
# roots = ["skills", "/path/to/shared-skills"]

[theme]
mode = "auto" # auto, light, or dark
```

The workspace defaults to the directory where tact starts. Relative paths in the configuration are
resolved from the configuration file's directory; relative command-line paths are resolved from the
current directory. Command-line options take precedence over environment variables, which take
precedence over the file.

The main agent options can also come from the environment. For example, `--workspace`,
`--thinking`, and `--resume` correspond to `TACT_WORKSPACE`, `TACT_THINKING`, and `TACT_RESUME`.
The prompt for `tact run` can be supplied through `TACT_PROMPT`. Run `tact --help` for the complete
command-line reference.

Use **Reload config** in the Actions menu after editing the file. Theme changes apply immediately.
Most agent settings apply when a session starts or is restored, while effort and fast mode can also
be changed during a session. Workspace changes require restarting tact.

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

When enabled, tact also searches `$CODEX_HOME/skills` (or `~/.codex/skills`) and
`~/.agents/skills`. A new session discovers the current set of skills. Restored sessions keep the
skill catalog they started with so their instructions remain stable.

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
