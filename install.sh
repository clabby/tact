#!/bin/sh

set -eu

repository_url="https://github.com/clabby/tact"
install_dir=${TACT_INSTALL_DIR:-}
if [ -z "$install_dir" ]; then
    if [ -z "${HOME:-}" ]; then
        echo "error: HOME is not set; set TACT_INSTALL_DIR to an absolute directory" >&2
        exit 1
    fi
    install_dir="$HOME/.local/bin"
fi
case "$install_dir" in
    /*) ;;
    *)
        echo "error: TACT_INSTALL_DIR must be an absolute path" >&2
        exit 1
        ;;
esac

operating_system=$(uname -s)
architecture=$(uname -m)
if [ "$operating_system" = Linux ]; then
    libc=$(getconf GNU_LIBC_VERSION 2>/dev/null || true)
    case "$libc" in
        glibc\ *) ;;
        *)
            echo "error: tact releases require a glibc-based Linux system" >&2
            exit 1
            ;;
    esac
fi
case "$operating_system:$architecture" in
    Linux:x86_64 | Linux:amd64) target="x86_64-unknown-linux-gnu" ;;
    Linux:aarch64 | Linux:arm64) target="aarch64-unknown-linux-gnu" ;;
    Darwin:x86_64 | Darwin:amd64) target="x86_64-apple-darwin" ;;
    Darwin:arm64 | Darwin:aarch64) target="aarch64-apple-darwin" ;;
    *)
        echo "error: tact releases do not support $operating_system $architecture" >&2
        exit 1
        ;;
esac

temporary_dir=$(mktemp -d "${TMPDIR:-/tmp}/tact-install.XXXXXX")
staged_binary=
cleanup() {
    if [ -n "$staged_binary" ]; then
        rm -f "$staged_binary"
    fi
    rm -rf "$temporary_dir"
}
trap cleanup EXIT HUP INT TERM

latest_url=$(curl --proto '=https' --tlsv1.2 -LsSf \
    -o /dev/null -w '%{url_effective}' "$repository_url/releases/latest")
version=${latest_url##*/}
case "$version" in
    v[0-9]* ) ;;
    *)
        echo "error: could not determine the latest tact release from $latest_url" >&2
        exit 1
        ;;
esac
case "$version" in
    */* | *\?* | *\#*)
        echo "error: GitHub returned an invalid tact release version: $version" >&2
        exit 1
        ;;
esac

archive_name="tact-$target-$version.tar.gz"
checksum_name="$archive_name.sha256"
download_url="$repository_url/releases/download/$version"
archive="$temporary_dir/$archive_name"
checksum="$temporary_dir/$checksum_name"

curl --proto '=https' --tlsv1.2 -LsSf -o "$archive" "$download_url/$archive_name"
curl --proto '=https' --tlsv1.2 -LsSf -o "$checksum" "$download_url/$checksum_name"

listed_name=$(awk 'NF == 2 && NR == 1 { name = $2 } END { if (NR != 1 || name == "") exit 1; print name }' "$checksum")
listed_name=${listed_name#\*}
if [ "$listed_name" != "$archive_name" ]; then
    echo "error: checksum file names $listed_name instead of $archive_name" >&2
    exit 1
fi

(
    cd "$temporary_dir"
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum -c "$checksum_name"
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 -c "$checksum_name"
    else
        echo "error: install sha256sum or shasum to verify the tact archive" >&2
        exit 1
    fi
)

package="tact-$target-$version"
expected_binary="$package/tact"
entries="$temporary_dir/archive-entries"
tar -tzf "$archive" >"$entries"
if ! awk -v root="$package/" '
    index($0, root) != 1 || $0 ~ /(^|\/)\.\.(\/|$)/ { exit 1 }
' "$entries"; then
    echo "error: release archive contains an unexpected path" >&2
    exit 1
fi
binary_count=$(awk -v expected="$expected_binary" '$0 == expected { count++ } END { print count + 0 }' "$entries")
if [ "$binary_count" -ne 1 ]; then
    echo "error: release archive does not contain exactly one $expected_binary" >&2
    exit 1
fi

extract_dir="$temporary_dir/extracted"
mkdir "$extract_dir"
tar -xzf "$archive" -C "$extract_dir" "$expected_binary"
binary="$extract_dir/$expected_binary"
if [ ! -f "$binary" ] || [ -L "$binary" ]; then
    echo "error: release archive's tact entry is not a regular file" >&2
    exit 1
fi

mkdir -p "$install_dir"
destination="$install_dir/tact"
if [ -d "$destination" ]; then
    echo "error: installation destination is a directory: $destination" >&2
    exit 1
fi
staged_binary=$(mktemp "$install_dir/.tact.XXXXXX")
install -m 755 "$binary" "$staged_binary"
if ! "$staged_binary" --version >/dev/null 2>&1; then
    echo "error: downloaded tact binary cannot run on this system" >&2
    exit 1
fi
mv -f "$staged_binary" "$destination"
staged_binary=

echo "Installed tact $version to $destination"
case ":${PATH:-}:" in
    *":$install_dir:"*) ;;
    *) echo "Add $install_dir to PATH to run tact." ;;
esac
