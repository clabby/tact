#!/usr/bin/env bash
set -euo pipefail

readonly workspace="${TACT_WORKSPACE:-/workspace}"
readonly proxy_host="${TACT_PROXY_HOST:-127.0.0.1}"
readonly proxy_port="${TACT_PROXY_PORT:-8080}"
readonly ca_file="${TACT_PROXY_CA_FILE:-/run/tact-public/ca.crt}"
readonly wait_seconds="${TACT_PROXY_WAIT_SECONDS:-30}"

if ! user_name="$(id -un 2>/dev/null)"; then
  readonly nss_directory="$(mktemp -d /tmp/tact-nss.XXXXXX)"
  cp /etc/passwd "$nss_directory/passwd"
  cp /etc/group "$nss_directory/group"
  printf 'tact-host:x:%s:%s:Tact developer:%s:/bin/bash\n' \
    "$(id -u)" "$(id -g)" "$HOME" >>"$nss_directory/passwd"
  printf 'tact-host:x:%s:\n' "$(id -g)" >>"$nss_directory/group"
  export NSS_WRAPPER_PASSWD="$nss_directory/passwd"
  export NSS_WRAPPER_GROUP="$nss_directory/group"
  export LD_PRELOAD="/usr/local/lib/libnss_wrapper.so${LD_PRELOAD:+:$LD_PRELOAD}"
  user_name=tact-host
fi
export USER="$user_name"
export LOGNAME="$user_name"

if [[ ! -d "$workspace" ]]; then
  echo "tact-dev: workspace does not exist: $workspace" >&2
  exit 1
fi

if [[ ! -w "$workspace" ]]; then
  echo "tact-dev: workspace is not writable: $workspace" >&2
  exit 1
fi

workspace_probe="$(mktemp "$workspace/.tact-dev-write-test.XXXXXX")"
rm -f -- "$workspace_probe"

if [[ ! -r "$ca_file" ]]; then
  echo "tact-dev: proxy CA is not readable: $ca_file" >&2
  exit 1
fi

readonly ca_bundle="$(mktemp /tmp/tact-ca-bundle.XXXXXX)"
cat /etc/ssl/certs/ca-certificates.crt "$ca_file" >"$ca_bundle"
chmod 0444 "$ca_bundle"

if [[ ! "$wait_seconds" =~ ^[0-9]+$ ]] || (( wait_seconds == 0 )); then
  echo "tact-dev: TACT_PROXY_WAIT_SECONDS must be a positive integer" >&2
  exit 1
fi

proxy_ready=false
for ((attempt = 0; attempt < wait_seconds * 10; attempt++)); do
  if (exec 3<>"/dev/tcp/$proxy_host/$proxy_port") 2>/dev/null; then
    proxy_ready=true
    break
  fi
  sleep 0.1
done

if [[ "$proxy_ready" != true ]]; then
  echo "tact-dev: proxy did not become ready at $proxy_host:$proxy_port" >&2
  exit 1
fi

export SSL_CERT_FILE="$ca_bundle"
export REQUESTS_CA_BUNDLE="$ca_bundle"
export CURL_CA_BUNDLE="$ca_bundle"
export GIT_SSL_CAINFO="$ca_bundle"
export CARGO_HTTP_CAINFO="$ca_bundle"
export NODE_EXTRA_CA_CERTS="$ca_file"

if [[ "${TACT_DEV_SHELL:-}" == "1" ]]; then
  exec /bin/bash
fi

exec tact "$@"
