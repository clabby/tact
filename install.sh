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
saved_stty=
download_pid=
cleanup() {
    if [ -n "$staged_binary" ]; then
        rm -f "$staged_binary"
    fi
    # An interrupted selector must not leave the terminal in raw mode with a hidden cursor.
    if [ -n "$saved_stty" ]; then
        stty "$saved_stty" </dev/tty 2>/dev/null || true
        printf '\033[?25h' >/dev/tty 2>/dev/null || true
    fi
    # A download interrupted mid-bar must not keep running or leave the terminal colored with a
    # hidden cursor.
    if [ -n "$download_pid" ]; then
        kill "$download_pid" 2>/dev/null || true
    fi
    if [ -t 2 ]; then
        printf '%s\033[?25h' "$reset" >&2
    fi
    rm -rf "$temporary_dir"
}
reset=
trap cleanup EXIT HUP INT TERM

esc=$(printf '\033')
bold= dim= cyan= green= yellow= reset=
if [ -t 1 ] && [ -z "${NO_COLOR:-}" ] && [ "${TERM:-dumb}" != dumb ]; then
    bold="$esc[1m" dim="$esc[2m" cyan="$esc[36m" green="$esc[32m" yellow="$esc[33m" reset="$esc[0m"
fi
case "${LC_ALL:-${LC_CTYPE:-${LANG:-}}}" in
    *[Uu][Tt][Ff]-8* | *[Uu][Tt][Ff]8*) pointer="❯" on="●" off="○" hint="↑/↓ move · enter select · q quit" bar_on="━" bar_off="─" ;;
    *) pointer=">" on="*" off=" " hint="up/down move, enter select, q quit" bar_on="#" bar_off="-" ;;
esac

# Draws the two channel rows. The cursor ends below them, so a redraw moves up two lines first.
draw_channels() {
    for row in 0 1; do
        if [ "$row" -eq 0 ]; then
            name=release color=$green note="the latest signed release" tag="recommended"
        else
            name=pre-release color=$yellow note="the latest build of main" tag="checksum only"
        fi
        if [ "$row" -eq "$1" ]; then
            printf '\r  %s%s %s %s%-12s%s %s %s(%s)%s%s[K\n' \
                "$color" "$pointer" "$on" "$bold" "$name" "$reset" "$note" "$dim" "$tag" "$reset" "$esc"
        else
            printf '\r    %s%s %-12s %s (%s)%s%s[K\n' \
                "$dim" "$off" "$name" "$note" "$tag" "$reset" "$esc"
        fi
    done
}

# Reads one key press from the terminal as hex: an arrow key is three bytes.
read_key() {
    key=$(dd bs=1 count=1 2>/dev/null </dev/tty | od -An -tx1 | tr -d ' \n')
    if [ "$key" = 1b ]; then
        key=$key$(dd bs=2 count=1 2>/dev/null </dev/tty | od -An -tx1 | tr -d ' \n')
    fi
}

choose_channel() {
    saved_stty=$(stty -g </dev/tty)
    stty -icanon -echo min 1 time 0 </dev/tty
    {
        printf '\n  %s%stact%s %sinstaller%s\n\n' "$bold" "$cyan" "$reset" "$dim" "$reset"
        printf '  %sSelect release channel%s  %s%s%s\n\n' "$bold" "$reset" "$dim" "$hint" "$reset"
        printf '%s[?25l' "$esc"
        draw_channels "$selected"
    } >/dev/tty
    while :; do
        read_key
        case "$key" in
            1b5b41 | 6b | 31) selected=0 ;;
            1b5b42 | 6a | 32) selected=1 ;;
            0a | 0d | "") break ;;
            71)
                stty "$saved_stty" </dev/tty
                saved_stty=
                printf '%s[?25h\n' "$esc" >/dev/tty
                echo "Installation cancelled." >&2
                exit 1
                ;;
        esac
        { printf '%s[2A' "$esc"; draw_channels "$selected"; } >/dev/tty
    done
    stty "$saved_stty" </dev/tty
    saved_stty=
    printf '%s[?25h' "$esc" >/dev/tty
}

# TACT_CHANNEL picks the channel without asking; otherwise a terminal gets the selector and
# anything else installs the latest release.
selected=0
channel=${TACT_CHANNEL:-}
case "$channel" in
    "")
        if [ -t 1 ] && [ "${TERM:-dumb}" != dumb ] && ( : </dev/tty ) 2>/dev/null; then
            choose_channel
            channel=release
            [ "$selected" -eq 1 ] && channel=pre-release
        else
            channel=release
        fi
        ;;
    release | stable) channel=release ;;
    pre-release | prerelease | dev) channel=pre-release ;;
    *)
        echo "error: TACT_CHANNEL must be release or pre-release, not $channel" >&2
        exit 1
        ;;
esac

if [ "$channel" = pre-release ]; then
    # GitHub lists releases newest first, and only pre-releases of main are tagged dev-<commit>.
    releases=$(curl --proto '=https' --tlsv1.2 -LsSf \
        "https://api.github.com/repos/clabby/tact/releases?per_page=30") || {
        echo "error: could not list tact pre-releases" >&2
        exit 1
    }
    version=$(printf '%s' "$releases" | grep -Eo '"tag_name": *"dev-[0-9a-f]{12}"' | head -n 1 |
        grep -Eo 'dev-[0-9a-f]{12}' || true)
    if [ -z "$version" ]; then
        echo "error: no tact pre-release is published; use TACT_CHANNEL=release" >&2
        exit 1
    fi
else
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
fi
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

# Redraws the progress row for the bytes of the archive written so far out of `$1`; a total of 0
# means it is not known.
draw_progress() {
    size=$({ wc -c <"$archive"; } 2>/dev/null | tr -d ' ')
    size=${size:-0}
    columns=$(stty size </dev/tty 2>/dev/null | awk '{ print $2 }')
    [ "${columns:-0}" -gt 0 ] || columns=80
    width=$(( columns - 38 ))
    [ "$width" -gt 40 ] && width=40
    [ "$width" -lt 10 ] && width=10
    mb_done=$(( size * 10 / 1048576 ))
    if [ "$1" -gt 0 ]; then
        done_cells=$(( width * size / $1 ))
        percent=$(( 100 * size / $1 ))
        mb_total=$(( $1 * 10 / 1048576 ))
        filled= empty= cell=0
        while [ "$cell" -lt "$width" ]; do
            if [ "$cell" -lt "$done_cells" ]; then filled=$filled$bar_on; else empty=$empty$bar_off; fi
            cell=$(( cell + 1 ))
        done
        printf '\r  %s%s%s%s%s %s%3d%%%s  %s%d.%d / %d.%d MB%s%s[K' \
            "$cyan" "$filled" "$reset$dim" "$empty" "$reset" "$bold" "$percent" "$reset" \
            "$dim" $(( mb_done / 10 )) $(( mb_done % 10 )) $(( mb_total / 10 )) $(( mb_total % 10 )) "$reset" "$esc" >&2
    else
        printf '\r  %s%d.%d MB downloaded%s%s[K' "$dim" $(( mb_done / 10 )) $(( mb_done % 10 )) "$reset" "$esc" >&2
    fi
}

# Downloads the archive with a progress row, the one download large enough to look like a hang.
# Without a terminal on stderr it stays silent so logs are not filled with redraws.
progress=0
if [ -t 2 ] && [ "${TERM:-dumb}" != dumb ]; then
    progress=1
    printf '\n  %sDownloading%s %s%s%s\n' "$bold" "$reset" "$dim" "$archive_name" "$reset" >&2
    # A one-byte ranged request reports the archive's total size in Content-Range.
    headers="$temporary_dir/headers"
    curl --proto '=https' --tlsv1.2 -LsSf -r 0-0 -D "$headers" -o /dev/null \
        "$download_url/$archive_name" 2>/dev/null || true
    total=$(sed -n 's/^[Cc]ontent-[Rr]ange:.*\///p' "$headers" 2>/dev/null | tail -n 1 | tr -dc '0-9')
    printf '\033[?25l' >&2
    curl --proto '=https' --tlsv1.2 -LsSf -o "$archive" "$download_url/$archive_name" &
    download_pid=$!
    while kill -0 "$download_pid" 2>/dev/null; do
        draw_progress "${total:-0}"
        sleep 0.1 2>/dev/null || sleep 1
    done
    if wait "$download_pid"; then
        downloaded=1
    else
        downloaded=0
    fi
    download_pid=
    if [ "$downloaded" -eq 1 ]; then
        draw_progress "${total:-0}"
    fi
    printf '\033[?25h\n' >&2
    if [ "$downloaded" -ne 1 ]; then
        echo "error: could not download $archive_name" >&2
        exit 1
    fi
else
    curl --proto '=https' --tlsv1.2 -LsSf -o "$archive" "$download_url/$archive_name"
fi
curl --proto '=https' --tlsv1.2 -LsSf -o "$checksum" "$download_url/$checksum_name"

listed_name=$(awk 'NF == 2 && NR == 1 { name = $2 } END { if (NR != 1 || name == "") exit 1; print name }' "$checksum")
listed_name=${listed_name#\*}
if [ "$listed_name" != "$archive_name" ]; then
    echo "error: checksum file names $listed_name instead of $archive_name" >&2
    exit 1
fi

# The checker's own "<file>: OK" adds nothing to the installer's output, so only a failure is shown.
if ! verification=$(
    cd "$temporary_dir"
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum -c "$checksum_name" 2>&1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 -c "$checksum_name" 2>&1
    else
        echo "error: install sha256sum or shasum to verify the tact archive"
        false
    fi
); then
    echo "$verification" >&2
    echo "error: the downloaded tact archive failed verification" >&2
    exit 1
fi

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

if [ "$progress" -eq 1 ]; then
    printf '\n'
fi
printf '  %sInstalled%s tact %s%s%s to %s (%s channel)\n' "$green$bold" "$reset" "$bold" "$version" "$reset" "$destination" "$channel"
if [ "$channel" = pre-release ]; then
    printf '  %sPre-releases are verified by checksum only. Run tact update to return to the latest release.%s\n' "$dim" "$reset"
fi
case ":${PATH:-}:" in
    *":$install_dir:"*) ;;
    *) echo "  Add $install_dir to PATH to run tact." ;;
esac
