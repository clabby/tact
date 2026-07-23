# `tact`

<!-- [![GitHub Actions Workflow Status](https://img.shields.io/github/actions/workflow/status/clabby/tact/ci.yaml?style=for-the-badge&label=CI)](https://github.com/clabby/tact/actions/workflows/ci.yaml) -->
<!-- [![Crates.io License](https://img.shields.io/crates/l/tact?style=for-the-badge)](https://crates.io/crates/tact) -->
<!-- [![Crates.io MSRV](https://img.shields.io/crates/msrv/tact?style=for-the-badge)](https://crates.io/crates/tact) -->
<!-- [![Crates.io Version](https://img.shields.io/crates/v/tact?style=for-the-badge)](https://crates.io/crates/tact) -->
<!-- [![docs.rs](https://img.shields.io/docsrs/tact?style=for-the-badge)](https://docs.rs/tact) -->

`tact` is a terminal interface for [Nanocodex](https://github.com/gakonst/nanocodex).
Run it without a subcommand to open the interactive terminal interface:

```sh
tact
```

It provides a keyboard-driven coding-agent experience with multiple sessions, queued prompts,
workspace file mentions, shell commands, image attachments, configurable reasoning effort, and
persistent transcripts. For non-interactive use, `tact run` submits one prompt and streams
Nanocodex events as JSONL:

```sh
tact run "inspect the workspace"
```

https://github.com/user-attachments/assets/5c634ae8-5c74-47c9-bb8c-9c18cb7fc97d

## Installation

Install the latest release on x86-64 or ARM64 Linux and macOS with:

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://raw.githubusercontent.com/clabby/tact/main/install.sh | sh
```

The installer verifies the release's SHA-256 checksum and writes `tact` to
`${TACT_INSTALL_DIR:-$HOME/.local/bin}` without requiring `sudo`. Set `TACT_INSTALL_DIR` to another
absolute directory if needed.

To build and install the current source instead:

```sh
git clone https://github.com/clabby/tact.git
cd tact
cargo install --path .
```

Official release archives are published for x86-64 and ARM64 Linux GNU systems and for Intel and
Apple Silicon Macs. After installing one of those binaries, update it to the latest signed release
with:

```sh
tact update
```

For standalone binaries, the command verifies the release checksum and its ephemeral minisign
signature against the public key published in that exact version's immutable crates.io package
metadata before replacing the current executable. If Cargo installed `tact` from crates.io, the
command instead recommends updating through `cargo install` so Cargo's ownership records remain
accurate. Source and Cargo builds do not display automatic update notifications; only official
release binaries check in the background.

## Authentication

The default `auto` mode uses a shared ChatGPT session from `$CODEX_HOME/auth.json` or
`~/.codex/auth.json`. If that file does not exist, it falls back to `OPENAI_API_KEY`.

Sign in with a ChatGPT subscription and inspect the selected credential source with:

```sh
tact auth login
tact auth status
```

Login starts Nanocodex's browser-based OAuth flow. To remove the shared credentials:

```sh
tact auth logout
```

Because the credential file is shared, logging out also logs Codex out. To authenticate with an API
key instead, export it before running `tact`:

```sh
export OPENAI_API_KEY="your-api-key"
tact --auth api-key
```

API keys are accepted only through `OPENAI_API_KEY`; they are never written to the configuration
file or included in status output.

## Configuration

By default, `tact` reads `$TACT_HOME/config.toml`, or `~/.tact/config.toml` when `TACT_HOME` is
unset. The file is optional. Pass `--config <PATH>` or set `TACT_CONFIG` to select another file.

Inspect the selected path and the complete effective configuration, including defaults, with:

```sh
tact config path
tact config show
```

The complete configuration schema is:

```toml
[auth]
mode = "auto" # auto, chatgpt, or api-key
file = "/path/to/.codex/auth.json"

[agent]
workspace = "/path/to/workspace"
thinking = "medium" # low, medium, high, xhigh, or max
web_search = true
image_generation = true
# instructions = "Replace the standard Nanocodex instructions"
# websocket_url = "wss://example.com/v1/responses"
# api_base_url = "https://example.com/v1"

[skills]
enabled = false
# roots = ["skills", "/path/to/shared-skills"]

[mcp_servers.filesystem]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/path/to/workspace"]
# cwd = "/path/to/server"

# Environment values are passed only to this server process.
[mcp_servers.filesystem.env]
# API_TOKEN = "secret"

# Remote servers use the Streamable HTTP transport. Authentication values are
# resolved from the named environment variables and are never stored here.
[mcp_servers.docs]
url = "https://example.com/mcp"
bearer_token_env_var = "DOCS_MCP_TOKEN"

[mcp_servers.docs.header_env]
X-Tenant-ID = "DOCS_TENANT_ID"

[theme]
mode = "auto" # auto, light, or dark

[theme.light]
code_text = "#262626"
code_background = "#EEEEEE"

[theme.dark]
code_text = "#D7D7D7"
code_background = "#262626"
```

Command-line options override values from the file. Relative paths supplied on the command line are
resolved from the current working directory; relative paths in the configuration file are resolved
from the file's directory. The workspace defaults to the current working directory. Endpoint
overrides are optional so Nanocodex can choose the appropriate defaults for ChatGPT or API-key
authentication.

MCP servers use either local stdio or remote Streamable HTTP transport. A stdio server accepts
`command`, `args`, `env`, and `cwd`; relative working directories are resolved from the
configuration file's directory. Stdio environment values are explicit credentials: `tact config
show` and debug output replace them with `[REDACTED]`, but the configuration file itself still
contains the original values. Nanocodex retains non-zeroizing copies while the server is active
because those copies are outside tact's memory ownership.

A remote server accepts `url`, `bearer_token_env_var`, and a `header_env` table mapping HTTP header
names to environment variable names. Only the variable names are persisted; tact snapshots valid
values into Nanocodex when constructing the provider. Nanocodex retains its own non-zeroizing
copies while the server is active. Missing or non-Unicode values fail only the affected server
without exposing their contents. Remote URLs must use HTTP or HTTPS and must not contain userinfo;
put credentials in the environment-backed authentication fields instead. Transport fields cannot
be mixed. MCP startup and discovery run
independently for each server, so one failed server does not prevent healthy servers or the session
from continuing.

Add a server without editing TOML by hand:

```sh
tact mcp add filesystem -- npx -y @modelcontextprotocol/server-filesystem /path/to/workspace
```

Use `--env NAME` one or more times to copy values from tact's environment into the server-specific
configuration, and `--cwd PATH` to set its working directory. Both options go before the `--` that
separates tact's options from the server command. Reading values from the environment keeps secrets
out of shell history and process arguments. Relative `--cwd` paths are resolved from the directory
where tact is invoked. Server names must be unique; adding an existing name leaves the configuration
unchanged.

Add a remote server with environment-backed authentication:

```sh
tact mcp add docs --url https://example.com/mcp \
  --bearer-token-env-var DOCS_MCP_TOKEN \
  --header-env X-Tenant-ID=DOCS_TENANT_ID
```

`--url` and a command after `--` are mutually exclusive. `--bearer-token-env-var` and repeatable
`--header-env HEADER=ENV_VAR` options apply only to remote servers and persist only environment
variable names. Tact reads their values when constructing a provider for a new session.

Local skills are disabled by default. A `SKILL.md` is a set of model instructions that can direct
shell or tool execution, so enable only roots whose contents you trust. Skill information also uses
persistent model context: tact initially adds only a compact catalog of each skill's frontmatter
name and description plus its canonical path. The complete `SKILL.md` body is read from disk only
when the model selects that skill. Both the catalog and any selected body then consume persistent
context for that session.

When enabled, tact searches `$CODEX_HOME/skills` (or `~/.codex/skills` when `CODEX_HOME` is unset),
`~/.agents/skills`, and any configured `roots`. Relative extra roots are resolved from the directory
containing the configuration file. Discovery follows directory links without revisiting cycles and
requires every descendant symlink target to remain beneath its canonical root. It is limited to
depth 8, 2,048 directories, 4,096 entries per directory, 4,096 skill files, 16 KiB of frontmatter
per skill, and an 8 KiB rendered catalog. Unreadable, invalid, duplicate, out-of-root, and special
filesystem entries are omitted, as are entries beyond these limits; one omitted skill does not
prevent healthy skills from loading.

Only fresh sessions discover the current skill roots. A restored session that was created with a
skill catalog retains those exact stored instructions and catalog even if configuration or files
have since changed. A restored session created without a catalog remains catalog-free, including
after skills are enabled.

Theme values accept Ratatui color names, indexed colors such as `239`, and RGB colors such as
`"#AABBCC"`. Colors directly under `[theme]` apply to both palettes, while values under
`[theme.light]` or `[theme.dark]` override one palette. Configurable colors are `text`, `border`,
`muted`, `accent`, `code_text`, `code_background`, and `thinking_low` through `thinking_max`. Auto
mode follows the operating-system preference while the TUI is running.

Use **Reload config** in the Actions menu to validate and reload the selected file. Theme changes
apply immediately. Authentication and agent settings (including reasoning effort, instructions,
tools, MCP servers, and endpoint overrides) apply when a new or restored session is started. Skill
configuration changes apply only when a fresh session starts; reloading does not change an active
or restored session's catalog. In an active session, use **Change effort** or `Ctrl+S` to change the
effort for subsequently accepted turns without resetting the conversation. Workspace changes
require a process restart because the terminal, shell, and transcript paths are bound to the
startup workspace. Command-line and environment overrides keep their original precedence when
reloading.

Every application-defined command-line option has an environment-variable equivalent:

| Option | Environment variable |
| --- | --- |
| `--config` | `TACT_CONFIG` |
| `--auth` | `TACT_AUTH` |
| `--auth-file` | `TACT_AUTH_FILE` |
| `--workspace` | `TACT_WORKSPACE` |
| `--thinking` | `TACT_THINKING` |
| `--instructions` | `TACT_INSTRUCTIONS` |
| `--web-search` | `TACT_WEB_SEARCH` |
| `--image-generation` | `TACT_IMAGE_GENERATION` |
| `--websocket-url` | `TACT_WEBSOCKET_URL` |
| `--api-base-url` | `TACT_API_BASE_URL` |
| `--resume <SESSION_ID>` | `TACT_RESUME` |
| `run <PROMPT>` | `TACT_PROMPT` |

Run `tact --help` for the full command-line reference.

## Resuming sessions

After each completed turn, `tact` keeps one compressed checkpoint for the session alongside its
append-only transcript segments. Open the Actions menu and choose **Resume session** to search the
current workspace's sessions by session ID, prompt preview, or model. The picker shows the session
age, effort, workspace, and first prompt before restoring the conversation.

You can also resume a known session directly:

```sh
tact --resume SESSION_ID
```

On exit, `tact` prints the active session's resume command. Checkpoints contain the complete
unredacted model-visible conversation and are stored with private filesystem permissions under the
same directory as the selected configuration file. A single rolling checkpoint is retained per
session; transcripts remain segmented and are grouped by session ID when restored.
