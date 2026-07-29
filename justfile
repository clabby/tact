build *args='':
    cargo build {{args}}

# Build an optimized binary; pass --official to mark it as an official release build.
release *args='':
    #!/usr/bin/env bash
    set -euo pipefail
    release_build=0
    cargo_args=()
    set -- {{args}}
    for arg in "$@"; do
        if [[ "$arg" == "--official" ]]; then
            release_build=1
        else
            cargo_args+=("$arg")
        fi
    done
    TACT_RELEASE_BUILD="$release_build" cargo build --release "${cargo_args[@]}"

check-fmt:
    just fmt --check

fmt *args='':
    cargo +nightly fmt --all -- {{args}}

clippy *args='':
    cargo +stable clippy --all-targets {{args}} -- -D warnings

lint: check-fmt clippy

test *args='':
    cargo nextest run {{args}}

test-docs:
    rustdoc --test README.md --edition 2024

check-docs:
    cargo doc --no-deps

bench *args='':
    cargo bench {{args}}

# Install the pinned, development-only Harbor environment.
harbor-bootstrap:
    uv sync --project evals --frozen

_local-docker-context:
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ -n "${DOCKER_HOST:-}" ]]; then
        echo 'unset DOCKER_HOST and select a local Docker context explicitly' >&2
        exit 1
    fi
    docker_context=$(docker context show)
    docker_endpoint=$(docker context inspect "$docker_context" --format '{{ "{{.Endpoints.docker.Host}}" }}')
    case "$docker_endpoint" in
        unix://*|/*) printf '%s\n' "$docker_context" ;;
        *)
            echo "Harbor requires a local Docker socket: $docker_endpoint" >&2
            exit 1
            ;;
    esac

_terminal-bench-platform:
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ "$(uname -s)" != "Darwin" || "$(uname -m)" != "arm64" ]]; then
        exit 0
    fi
    settings="$HOME/Library/Group Containers/group.com.docker/settings-store.json"
    rosetta=$(/usr/bin/plutil \
        -extract UseVirtualizationFrameworkRosetta raw -o - "$settings" 2>/dev/null || true)
    if [[ "$rosetta" == "true" ]]; then
        exit 0
    fi
    echo 'Terminal-Bench requires Docker Desktop Rosetta support on Apple Silicon.' >&2
    echo 'Enable Rosetta for amd64 emulation in Docker Desktop, then restart Docker.' >&2
    exit 1

# Export a static Linux Tact binary with benchmark instrumentation enabled.
build-harbor-agent platform='':
    #!/usr/bin/env bash
    set -euo pipefail
    docker_context=$(just --quiet _local-docker-context)
    test -z "$(find src -type l -print -quit)" || {
        echo 'refusing to build Harbor agent with symlinks below src/' >&2
        exit 1
    }
    test -z "$(find src -type f ! -name '*.rs' -print -quit)" || {
        echo 'refusing to send non-Rust files below src/ to the Harbor build' >&2
        exit 1
    }
    build_context=$(mktemp -d)
    trap 'rm -rf -- "$build_context"' EXIT
    cp Cargo.toml Cargo.lock build.rs README.md LICENSE.md "$build_context/"
    cp -R src "$build_context/src"
    platform_args=()
    if [[ -n "{{platform}}" ]]; then
        platform_args=(--platform "{{platform}}")
    fi
    docker --context "$docker_context" buildx build \
        "${platform_args[@]}" \
        --file evals/harbor_adapter/tact.Dockerfile \
        --target artifact \
        --output type=local,dest=.tact/installed \
        "$build_context"

# Validate the adapter and resolved Harbor configuration without running a task.
check-harbor:
    uv sync --project evals --frozen
    PYTHONDONTWRITEBYTECODE=1 uv run --project evals python -m unittest harbor_adapter.test_agent -v
    cargo test --locked --features harbor-evals core::orchestration
    uv run --project evals harbor run \
        --config evals/terminal-bench.yaml \
        --dataset terminal-bench/terminal-bench-2-1@6 \
        --include-task-name terminal-bench/openssl-selfsigned-cert \
        --print-config >/dev/null

# Run the pinned Terminal-Bench configuration. Additional Harbor flags are forwarded.
harbor-eval *args='':
    #!/usr/bin/env bash
    set -euo pipefail
    just --quiet _local-docker-context >/dev/null
    just --quiet _terminal-bench-platform
    test -x .tact/installed/tact || {
        echo 'missing .tact/installed/tact; run `just build-harbor-agent`' >&2
        exit 1
    }
    if [[ -n "${TACT_CODEX_AUTH_FILE:-}" ]]; then
        auth_file="$TACT_CODEX_AUTH_FILE"
    elif [[ -n "${CODEX_HOME:-}" ]]; then
        auth_file="$CODEX_HOME/auth.json"
    else
        auth_file="$HOME/.codex/auth.json"
    fi
    test -f "$auth_file" || {
        echo "missing Codex auth file: $auth_file" >&2
        echo 'configure Codex file credential storage and log in again' >&2
        exit 1
    }
    uv run --project evals harbor run \
        --config evals/terminal-bench.yaml \
        --dataset terminal-bench/terminal-bench-2-1@6 \
        --agent-kwarg "auth_file=$auth_file" \
        {{args}}

# Browse retained Harbor jobs and Tact trajectories.
harbor-view *args='':
    uv run --project evals harbor view .tact/harbor/jobs --jobs {{args}}
