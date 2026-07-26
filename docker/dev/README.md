# Tact development container

This example builds a local, batteries-included Tact image and runs it against a writable host
workspace. The development container receives only placeholder credentials. A separate
[`iron-proxy`](https://docs.iron.sh/) container holds the real credential and replaces the
placeholder on matching OpenAI requests.

Use `run.py`; do not invoke Compose directly with real credentials.

## Requirements

- Docker with Buildx and Compose v2
- Python 3.11 or newer
- `just` for the convenience recipes (optional)
- A local Docker context backed by a Unix socket

Remote Docker contexts are rejected before credentials are read. Otherwise, the secret bind mount
and shared network namespace could be created on another machine.

## Build and run

From the repository root:

```sh
just -f docker/dev/justfile build
just -f docker/dev/justfile run
```

Arguments after `run` go to Tact. You can also use the launcher directly:

```sh
python3 docker/dev/run.py --workspace /path/to/project -- --help
python3 docker/dev/run.py --workspace /path/to/project
```

The workspace is mounted read-write at `/workspace`. `just shell` opens Bash through the same
launcher. The container runs as the host UID/GID and synthesizes a local username when necessary.
The Bake target creates only `tact-dev:local`; it has no registry or push configuration.

## Authentication

`--auth auto` first tries a valid ChatGPT subscription credential and then `OPENAI_API_KEY` or
`OPENAI_API_KEY_FILE`. Select a mode explicitly with:

```sh
OPENAI_API_KEY='...' python3 docker/dev/run.py --auth api-key
python3 docker/dev/run.py --auth chatgpt
```

API keys are replaced only in the `Authorization` header for OpenAI `/v1` requests. ChatGPT mode
loads `$CODEX_HOME/auth.json`, then `~/.codex/auth.json`; it extracts only the access token and
account ID. The proxy replaces their fake counterparts only for `GET` and `POST` requests under
`https://chatgpt.com/backend-api/codex`.

The launcher rejects a selected credential file inside the workspace or Tact state directory,
because either writable mount would expose the original file to the development container.

The generated proxy policy uses iron-proxy's CONNECT tunnel, explicit TLS MITM, file-backed secret
sources, and required placeholder replacement. There is deliberately no allowlist transform:
egress is unrestricted, while real credentials remain confined to requests matching the selected
auth route. See iron-proxy's [tunnel guide](https://docs.iron.sh/guides/socks5-connect) and
[configuration reference](https://docs.iron.sh/reference/configuration).

## Host state and local agent files

The launcher bind-mounts the parent directory of the selected host Tact config read-write at
`/run/tact-state`. Tact uses the same directory for `config.toml`, transcripts, checkpoints, and
transcript projections, so reads and writes pass directly through to the host. `TACT_CONFIG` and
`TACT_HOME` select the host location; otherwise the launcher uses `~/.tact/config.toml`.
It creates a missing config as an empty private file and rejects config symlinks, whose targets
would not reliably remain inside the mounted state directory.

Because the entire directory is visible inside the development container, keep credentials and
literal MCP secrets out of it. Authentication is forced to the generated placeholder auth file;
the launcher does not mount `~/.codex/auth.json` or expose `OPENAI_API_KEY`.

The launcher separately copies one winning `AGENTS.override.md` or `AGENTS.md` and the default
`~/.codex/skills` and `~/.agents/skills` roots into the read-only public projection. It skips
symlinks while copying skills. Reduce or extend those copies with:

```sh
python3 docker/dev/run.py --no-instructions --no-skills
python3 docker/dev/run.py --skill-root /path/to/skills
```

Relative skill roots inside the Tact state directory remain reachable directly. Use `--skill-root`
for roots elsewhere on the host. Language and package caches in `/home/tact`
persist in the external `tact-dev-state` volume. Avoid concurrent runs that mutate the same host
state or cache volume. `just clean` removes the local image and cache volume, but never host Tact
state.

## Isolation model

The launcher creates two temporary directories:

- Private state contains `proxy.yaml`, the ephemeral CA private key, and only the selected real
  credential. It is mounted read-only into `iron-proxy` and nowhere else.
- Public state contains the CA certificate, fake auth, copied global instructions, and copied
  skills. It is mounted read-only into the development container.
- Host Tact state and the workspace are mounted read-write into the development container.

The services share only a network namespace. The CONNECT tunnel listens on `127.0.0.1:8080`; no
host port is published. Proxy-aware HTTP clients use it, while `NO_PROXY` preserves ordinary
localhost development. Unused iron-proxy HTTP, HTTPS, and metrics listeners bind only to ephemeral
loopback ports.

The entrypoint waits for the tunnel listener, verifies the workspace, and constructs a trust bundle
containing both the system roots and the ephemeral proxy CA. It configures Rust/Cargo, curl, Git,
Python, Node, and Bun before executing `tact`. Listener readiness does not probe an upstream API;
a later proxy failure causes proxy-aware requests to fail but does not terminate Tact.

Both containers drop all capabilities, prohibit privilege gains, and publish no ports. This is
credential isolation, not a filesystem or network firewall: code can read and modify the mounted
workspace, inspect host Tact state and public projections, and use direct egress. `iron-proxy`
caches resolved credentials in its own process; launcher-owned buffers and temporary files are
cleared on exit, but the launcher cannot promise zeroization inside that dependency.

## Customization and troubleshooting

Tool versions and executable mappings live in `tools.toml`; build packages and Rust components
remain beside their installers in `docker/development.dockerfile`. Pin changes and rebuild. Add
project tools to that Dockerfile instead of mounting host package caches, sockets, or home
directories.

Run every launcher and image test with:

```sh
just -f docker/dev/justfile test
```

- **Remote Docker context:** switch to a local Unix-socket context.
- **Workspace is not writable:** make the host directory writable by the current UID/GID.
- **Tact state is not writable:** make the selected config directory writable by the current
  UID/GID. Tact needs to create sibling temporary files for atomic config updates.
- **Proxy listener is not ready:** inspect both service logs and the generated file permissions.
- **TLS errors:** verify `/run/tact-public/ca.crt`; the entrypoint exports the common CA variables.
- **Authentication rejected:** select `--auth api-key` or `--auth chatgpt`; refresh an expiring
  ChatGPT login.
- **Old state:** run `just -f docker/dev/justfile clean` after active runs stop.
