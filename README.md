# `tact`

<!-- [![GitHub Actions Workflow Status](https://img.shields.io/github/actions/workflow/status/clabby/tact/ci.yaml?style=for-the-badge&label=CI)](https://github.com/clabby/tact/actions/workflows/ci.yaml) -->
<!-- [![Crates.io License](https://img.shields.io/crates/l/tact?style=for-the-badge)](https://crates.io/crates/tact) -->
<!-- [![Crates.io MSRV](https://img.shields.io/crates/msrv/tact?style=for-the-badge)](https://crates.io/crates/tact) -->
<!-- [![Crates.io Version](https://img.shields.io/crates/v/tact?style=for-the-badge)](https://crates.io/crates/tact) -->
<!-- [![docs.rs](https://img.shields.io/docsrs/tact?style=for-the-badge)](https://docs.rs/tact) -->

`tact` is a terminal interface for [Nanocodex](https://github.com/clabby/nanocodex).
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

> [!WARNING]
> `tact` is experimental and currently relies on a fork of Nanocodex. Install it from source
> rather than crates.io.

```sh
git clone https://github.com/clabby/tact.git
cd tact
cargo install --path .
```

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

Theme values accept Ratatui color names, indexed colors such as `239`, and RGB colors such as
`"#AABBCC"`. Colors directly under `[theme]` apply to both palettes, while values under
`[theme.light]` or `[theme.dark]` override one palette. Configurable colors are `text`, `border`,
`muted`, `accent`, `code_text`, `code_background`, and `thinking_low` through `thinking_max`. Auto
mode follows the operating-system preference while the TUI is running.

Use **Reload config** in the Actions menu to validate and reload the selected file. Theme changes
apply immediately. Authentication and agent settings (including reasoning effort, instructions,
tools, and endpoint overrides) apply when a new session is started. In an active session, use
**Change effort** or `Ctrl+S` to change the effort for subsequently accepted turns without resetting
the conversation. Workspace changes require a process restart because the terminal, shell, and
transcript paths are bound to the startup workspace. Command-line and environment overrides keep
their original precedence when reloading.

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
