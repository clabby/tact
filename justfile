build *args='':
    cargo build $@

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
    cargo +nightly fmt --all $@

clippy *args='':
    cargo +stable clippy --all-targets $@ -- -D warnings

lint: check-fmt clippy

test *args='':
    cargo nextest run $@

test-docs:
    rustdoc --test README.md --edition 2024

check-docs:
    cargo doc --no-deps

bench *args='':
    cargo bench $@
