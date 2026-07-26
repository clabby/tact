# syntax=docker/dockerfile:1.7

# Docker resolves FROM before it can read the tool manifest from the build context.
FROM debian:bookworm-slim@sha256:7b140f374b289a7c2befc338f42ebe6441b7ea838a042bbd5acbfca6ec875818 AS toolchain

ARG TARGETARCH

ENV DEBIAN_FRONTEND=noninteractive \
    RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/opt/cargo \
    UV_PYTHON_INSTALL_DIR=/opt/python \
    BINSTALL_DISABLE_TELEMETRY=true \
    PATH=/opt/cargo/bin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

COPY docker/dev/tools.toml /usr/local/share/tact-dev/tools.toml

RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt,sharing=locked \
    apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates curl gnupg \
    && llvm_key_url=$(awk -F' = ' '/^llvm_signing_key =/ { gsub(/"/, "", $2); print $2 }' \
        /usr/local/share/tact-dev/tools.toml) \
    && llvm_key_sha256=$(awk -F' = ' '/^llvm_signing_key_sha256 =/ { gsub(/"/, "", $2); print $2 }' \
        /usr/local/share/tact-dev/tools.toml) \
    && llvm_repository=$(awk -F' = ' '/^llvm_repository =/ { gsub(/"/, "", $2); print $2 }' \
        /usr/local/share/tact-dev/tools.toml) \
    && llvm_suite=$(awk -F' = ' '/^llvm_suite =/ { gsub(/"/, "", $2); print $2 }' \
        /usr/local/share/tact-dev/tools.toml) \
    && curl --fail --location --show-error "$llvm_key_url" --output /tmp/llvm.asc \
    && echo "$llvm_key_sha256  /tmp/llvm.asc" | sha256sum --check --strict \
    && gpg --batch --dearmor --output /usr/share/keyrings/llvm-archive-keyring.gpg \
        /tmp/llvm.asc \
    && rm /tmp/llvm.asc \
    && echo "deb [signed-by=/usr/share/keyrings/llvm-archive-keyring.gpg] $llvm_repository $llvm_suite main" \
        >/etc/apt/sources.list.d/llvm.list \
    && printf 'Package: clang-21 llvm-21 llvm-21-tools lld-21 lldb-21\nPin: origin apt.llvm.org\nPin-Priority: 1001\n' \
        >/etc/apt/preferences.d/llvm \
    && apt-get update \
    && apt-get install --yes --no-install-recommends \
        binutils \
        build-essential \
        ca-certificates \
        clang-21 \
        curl \
        elfutils \
        file \
        gdb \
        git \
        libdw1 \
        libelf1 \
        libnss-wrapper \
        libssl3 \
        libunwind8 \
        lld-21 \
        lldb-21 \
        llvm-21 \
        llvm-21-tools \
        pkg-config \
        procps \
        strace \
        unzip \
        xz-utils \
    && update-alternatives --install /usr/bin/clang clang /usr/bin/clang-21 210 \
    && update-alternatives --install /usr/bin/clang++ clang++ /usr/bin/clang++-21 210 \
    && update-alternatives --install /usr/bin/llvm-config llvm-config /usr/bin/llvm-config-21 210 \
    && update-alternatives --install /usr/bin/ld.lld ld.lld /usr/bin/ld.lld-21 210 \
    && update-alternatives --install /usr/bin/lld lld /usr/bin/lld-21 210 \
    && update-alternatives --install /usr/bin/lldb lldb /usr/bin/lldb-21 210 \
    && nss_wrapper=$(find /usr/lib -name libnss_wrapper.so -print -quit) \
    && test -n "$nss_wrapper" \
    && ln --symbolic "$nss_wrapper" /usr/local/lib/libnss_wrapper.so

RUN <<'EOF'
set -eu

manifest=/usr/local/share/tact-dev/tools.toml
manifest_value() {
    section="$1"
    key="$2"
    awk -v wanted_section="$section" -v wanted_key="$key" '
        $0 == "[" wanted_section "]" { in_section = 1; next }
        /^\[/ { in_section = 0 }
        in_section && $1 == wanted_key {
            sub(/^[^=]+=[[:space:]]*/, "")
            gsub(/^"|"$/, "")
            print
            exit
        }
    ' "$manifest"
}

case "$TARGETARCH" in
    amd64|arm64) architecture="$TARGETARCH" ;;
    *) echo "unsupported architecture: $TARGETARCH" >&2; exit 1 ;;
esac

uv_version=$(manifest_value uv version)
uv_target=$(manifest_value uv "${architecture}_target")
uv_sha256=$(manifest_value uv "${architecture}_sha256")
uv_archive="uv-${uv_target}.tar.gz"
curl --fail --location --show-error \
    "https://github.com/astral-sh/uv/releases/download/${uv_version}/${uv_archive}" \
    --output "/tmp/${uv_archive}"
echo "${uv_sha256}  /tmp/${uv_archive}" | sha256sum --check --strict
tar --extract --gzip --file "/tmp/${uv_archive}" --strip-components=1 \
    --directory /usr/local/bin
rm "/tmp/${uv_archive}"

python_version=$(manifest_value python version)
uv python install "$python_version"
python=$(find /opt/python -path '*/bin/python3.13' -print -quit)
test -n "$python"
ln --symbolic "$python" /usr/local/bin/python3
ln --symbolic "$python" /usr/local/bin/python

node_version=$(manifest_value node version)
node_target=$(manifest_value node "${architecture}_target")
node_sha256=$(manifest_value node "${architecture}_sha256")
node_archive="node-v${node_version}-linux-${node_target}.tar.xz"
curl --fail --location --show-error \
    "https://nodejs.org/dist/v${node_version}/${node_archive}" \
    --output "/tmp/${node_archive}"
echo "${node_sha256}  /tmp/${node_archive}" | sha256sum --check --strict
tar --extract --xz --file "/tmp/${node_archive}" --strip-components=1 \
    --directory /usr/local
rm "/tmp/${node_archive}"

bun_version=$(manifest_value bun version)
bun_target=$(manifest_value bun "${architecture}_target")
bun_sha256=$(manifest_value bun "${architecture}_sha256")
bun_archive="bun-linux-${bun_target}.zip"
curl --fail --location --show-error \
    "https://github.com/oven-sh/bun/releases/download/bun-v${bun_version}/${bun_archive}" \
    --output "/tmp/${bun_archive}"
echo "${bun_sha256}  /tmp/${bun_archive}" | sha256sum --check --strict
unzip -q "/tmp/${bun_archive}" -d /tmp/bun
install "/tmp/bun/bun-linux-${bun_target}/bun" /usr/local/bin/bun
ln --symbolic bun /usr/local/bin/bunx
rm --recursive /tmp/bun "/tmp/${bun_archive}"

rustup_version=$(manifest_value rustup version)
rustup_target=$(manifest_value rustup "${architecture}_target")
rustup_sha256=$(manifest_value rustup "${architecture}_sha256")
curl --fail --location --show-error \
    "https://static.rust-lang.org/rustup/archive/${rustup_version}/${rustup_target}/rustup-init" \
    --output /tmp/rustup-init
echo "${rustup_sha256}  /tmp/rustup-init" | sha256sum --check --strict
chmod +x /tmp/rustup-init
/tmp/rustup-init --no-modify-path --profile minimal --default-toolchain none -y
rm /tmp/rustup-init

stable=$(manifest_value rust stable)
nightly=$(manifest_value rust nightly)
rustup toolchain install "$stable" \
    --component clippy,llvm-tools-preview,rustfmt
rustup toolchain install "$nightly" \
    --component clippy,llvm-tools-preview,rust-src,rustfmt
rustup default "$stable"

binstall_version=$(manifest_value cargo_binstall version)
binstall_target=$(manifest_value cargo_binstall "${architecture}_target")
binstall_sha256=$(manifest_value cargo_binstall "${architecture}_sha256")
binstall_archive="cargo-binstall-${binstall_target}.tgz"
curl --fail --location --show-error \
    "https://github.com/cargo-bins/cargo-binstall/releases/download/v${binstall_version}/${binstall_archive}" \
    --output "/tmp/${binstall_archive}"
echo "${binstall_sha256}  /tmp/${binstall_archive}" | sha256sum --check --strict
tar --extract --gzip --file "/tmp/${binstall_archive}" \
    --directory /opt/cargo/bin
rm "/tmp/${binstall_archive}"
EOF

RUN --mount=type=cache,target=/opt/cargo/registry,sharing=locked \
    --mount=type=cache,target=/opt/cargo/git,sharing=locked <<'EOF'
set -eu

python3 - <<'PY' >/tmp/cargo-tools
import tomllib

with open("/usr/local/share/tact-dev/tools.toml", "rb") as manifest_file:
    manifest = tomllib.load(manifest_file)

for tool in manifest["tools"].values():
    print(f'{tool["package"]}@{tool["version"]}')
PY

while IFS= read -r package; do
    cargo binstall \
        --no-confirm \
        --force \
        --no-discover-github-token \
        --maximum-resolution-timeout 20 \
        --strategies quick-install,compile \
        "$package"
done </tmp/cargo-tools
rm /tmp/cargo-tools

# rustup proxies need their invoked names, while installed CLI binaries are
# ordinary files. Expose both from a system path before CARGO_HOME becomes the
# writable per-user cache at runtime.
cp --archive /opt/cargo/bin/. /usr/local/bin/
EOF

FROM toolchain AS tact-builder

WORKDIR /source
COPY . .
RUN --mount=type=cache,target=/opt/cargo/registry,sharing=locked \
    --mount=type=cache,target=/opt/cargo/git,sharing=locked \
    --mount=type=cache,target=/source/target,sharing=locked \
    cargo build --locked --release \
    && install target/release/tact /tmp/tact

FROM toolchain AS tact-dev

ARG TACT_UID=1000
ARG TACT_GID=1000

ENV HOME=/home/tact \
    CARGO_HOME=/home/tact/.cargo \
    RUSTUP_HOME=/usr/local/rustup \
    UV_CACHE_DIR=/home/tact/.cache/uv \
    NPM_CONFIG_CACHE=/home/tact/.npm \
    BUN_INSTALL_CACHE_DIR=/home/tact/.bun/install/cache \
    PATH=/home/tact/.cargo/bin:/home/tact/.local/bin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

COPY --from=tact-builder /tmp/tact /usr/local/bin/tact
COPY --chmod=0755 docker/dev/entrypoint.sh /usr/local/bin/tact-entrypoint

RUN groupadd --gid "$TACT_GID" tact \
    && useradd --uid "$TACT_UID" --gid tact --create-home --shell /bin/bash tact \
    && mkdir -p /workspace /home/tact/.cargo /home/tact/.cache/uv \
        /home/tact/.npm /home/tact/.bun/install/cache \
    && chown -R tact:tact /workspace /home/tact \
    && chmod 0777 /home/tact

USER tact
WORKDIR /workspace
ENTRYPOINT ["/usr/local/bin/tact-entrypoint"]
