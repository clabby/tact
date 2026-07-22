build *args='':
    cargo build $@

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
