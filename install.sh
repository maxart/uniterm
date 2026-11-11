#!/bin/sh
set -eu

REPO=${UNITERM_REPO:-maxart/uniterm}
VERSION=${UNITERM_VERSION:-latest}
INSTALL_DIR=${UNITERM_INSTALL_DIR:-}
DOWNLOAD_ROOT=${UNITERM_DOWNLOAD_ROOT:-https://github.com/${REPO}/releases}
TMP_DIR=
SYSTEM_INSTALL=0
USE_SUDO=0

fail() {
    echo "uniterm installer: $*" >&2
    exit 1
}

cleanup() {
    [ -z "$TMP_DIR" ] || rm -rf -- "$TMP_DIR"
}

detect_platform() {
    os=$(uname -s)
    arch=$(uname -m)
    case "$os:$arch" in
        Darwin:arm64 | Darwin:aarch64)
            echo macos-arm64
            ;;
        Darwin:*)
            fail "macOS releases require Apple Silicon; Intel macOS is not supported"
            ;;
        Linux:x86_64 | Linux:amd64)
            if ldd --version 2>&1 | grep -qi musl; then
                fail "prebuilt Linux releases require glibc; build from source on musl systems"
            fi
            echo linux-x86_64
            ;;
        Linux:aarch64 | Linux:arm64)
            if [ -n "${TERMUX_VERSION:-}" ]; then
                echo android-aarch64
            else
                case "${PREFIX:-}" in
                    /data/data/com.termux/*) echo android-aarch64 ;;
                    *)
                        if ldd --version 2>&1 | grep -qi musl; then
                            fail "prebuilt Linux releases require glibc; build from source on musl systems"
                        fi
                        echo linux-aarch64
                        ;;
                esac
            fi
            ;;
        *)
            fail "unsupported platform: ${os} ${arch}"
            ;;
    esac
}

release_url() {
    case "$VERSION" in
        latest)
            echo "${DOWNLOAD_ROOT}/latest/download"
            ;;
        *[!A-Za-z0-9._-]* | '')
            fail "UNITERM_VERSION contains unsafe characters"
            ;;
        *)
            echo "${DOWNLOAD_ROOT}/download/${VERSION}"
            ;;
    esac
}

download() {
    url=$1
    output=$2
    case "$url" in
        https://*) ;;
        file://*)
            [ "${UNITERM_INSTALLER_TESTING:-}" = 1 ] \
                || fail "refusing a non-HTTPS download URL"
            ;;
        *)
            fail "refusing a non-HTTPS download URL"
            ;;
    esac

    if command -v curl >/dev/null 2>&1; then
        if [ "${url#file://}" != "$url" ]; then
            curl -fsSL "$url" -o "$output"
        else
            curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fsSL --retry 3 "$url" -o "$output"
        fi
    elif command -v wget >/dev/null 2>&1; then
        wget --https-only -q "$url" -O "$output"
    else
        fail "curl or wget is required"
    fi
}

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        fail "sha256sum or shasum is required to verify downloads"
    fi
}

verify() {
    file=$1
    manifest=$2
    name=$(basename "$file")
    expected=$(awk -v name="$name" '$2 == name { print $1; exit }' "$manifest")
    [ -n "$expected" ] || fail "SHA256SUMS has no entry for ${name}"
    actual=$(sha256 "$file")
    [ "$actual" = "$expected" ] \
        || fail "checksum mismatch for ${name}"
}

usage() {
    cat <<'EOF'
Usage: install.sh [--system]

Update the existing ut/uniterm installation, or install to ~/.local/bin.
Termux uses $PREFIX/bin for a fresh installation.
--system selects /usr/local/bin and permits sudo when needed.
UNITERM_INSTALL_DIR overrides the destination, including with --system.
UNITERM_VERSION selects a release (default: latest).
EOF
}

# Search executable files rather than shell aliases, and normalize directories
# so relative PATH entries and symlinked directories compare consistently.
find_on_path() {
    search_path=${PATH:-}:
    while [ -n "$search_path" ]; do
        search_dir=${search_path%%:*}
        search_path=${search_path#*:}
        search_dir=${search_dir:-.}
        if [ -f "$search_dir/$1" ] && [ -x "$search_dir/$1" ]; then
            (CDPATH='' cd -- "$search_dir" && pwd -P)
            return
        fi
    done
}

directory_on_path() {
    search_path=${PATH:-}:
    while [ -n "$search_path" ]; do
        search_dir=${search_path%%:*}
        search_path=${search_path#*:}
        search_dir=${search_dir:-.}
        if [ -d "$search_dir" ] \
            && [ "$(CDPATH='' cd -- "$search_dir" && pwd -P)" = "$1" ]; then
            return 0
        fi
    done
    return 1
}

select_install_dir() {
    if [ -n "$INSTALL_DIR" ]; then
        echo "$INSTALL_DIR"
        return
    fi
    if [ "$SYSTEM_INSTALL" = 1 ]; then
        echo /usr/local/bin
        return
    fi
    # Prefer the recommended short command when both names are installed.
    for installed_name in ut uniterm; do
        installed_dir=$(find_on_path "$installed_name")
        if [ -n "$installed_dir" ]; then
            echo "$installed_dir"
            return
        fi
    done
    if [ -n "${TERMUX_VERSION:-}" ]; then
        [ -n "${PREFIX:-}" ] || fail "Termux PREFIX is unset"
        echo "${PREFIX}/bin"
        return
    fi
    case "${PREFIX:-}" in
        /data/data/com.termux/*)
            echo "${PREFIX}/bin"
            return
            ;;
    esac
    [ -n "${HOME:-}" ] || fail "HOME is unset; set UNITERM_INSTALL_DIR explicitly"
    # Also recognize user installations whose directory is not yet on PATH.
    for installed_dir in "${HOME}/.local/bin" "${CARGO_HOME:-${HOME}/.cargo}/bin"; do
        for installed_name in ut uniterm; do
            if [ -f "$installed_dir/$installed_name" ] && [ -x "$installed_dir/$installed_name" ]; then
                echo "$installed_dir"
                return
            fi
        done
    done
    echo "${HOME}/.local/bin"
}

prepare_install() {
    case "$install_dir" in
        /*) ;;
        *) install_dir="$(pwd -P)/$install_dir" ;;
    esac
    # Check the nearest existing parent before downloading or changing files.
    install_parent=$install_dir
    while [ ! -d "$install_parent" ]; do
        [ ! -e "$install_parent" ] || fail "not a directory: $install_parent"
        install_parent=$(dirname "$install_parent")
    done
    if [ ! -w "$install_parent" ] && [ "$(id -u)" -ne 0 ]; then
        [ "$SYSTEM_INSTALL" = 1 ] \
            || fail "${install_dir} needs elevated permissions. Rerun with --system (and UNITERM_INSTALL_DIR for a custom destination), or choose a writable UNITERM_INSTALL_DIR. No files were changed"
        command -v sudo >/dev/null 2>&1 || fail "sudo is required to install to ${install_dir}"
        USE_SUDO=1
    fi
    for installed_name in uniterm ut; do
        [ ! -d "$install_dir/$installed_name" ] \
            || fail "refusing to replace a directory: $install_dir/$installed_name"
    done
}

install_one() {
    install_one_source=$1
    install_one_destination=$2
    install_one_directory=$(dirname "$install_one_destination")
    if [ "$USE_SUDO" = 1 ]; then
        sudo mkdir -p "$install_one_directory"
        sudo install -m 0755 "$install_one_source" "$install_one_destination"
    else
        mkdir -p "$install_one_directory"
        install -m 0755 "$install_one_source" "$install_one_destination"
    fi
}

main() {
    for option in "$@"; do
        case "$option" in
            --system) SYSTEM_INSTALL=1 ;;
            --help | -h) usage; return ;;
            *) fail "unknown option: $option (use --help)" ;;
        esac
    done
    command -v install >/dev/null 2>&1 || fail "the install command is required"
    platform=$(detect_platform)
    base_url=$(release_url)
    install_dir=$(select_install_dir)
    prepare_install
    TMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/uniterm-install.XXXXXX")
    trap cleanup EXIT
    trap 'exit 129' HUP
    trap 'exit 130' INT
    trap 'exit 143' TERM

    manifest="${TMP_DIR}/SHA256SUMS"
    uniterm_asset="uniterm-${platform}"
    ut_asset="ut-${platform}"
    download "${base_url}/SHA256SUMS" "$manifest"
    download "${base_url}/${uniterm_asset}" "${TMP_DIR}/${uniterm_asset}"
    download "${base_url}/${ut_asset}" "${TMP_DIR}/${ut_asset}"
    verify "${TMP_DIR}/${uniterm_asset}" "$manifest"
    verify "${TMP_DIR}/${ut_asset}" "$manifest"

    install_one "${TMP_DIR}/${uniterm_asset}" "${install_dir}/uniterm"
    install_one "${TMP_DIR}/${ut_asset}" "${install_dir}/ut"
    "${install_dir}/ut" --version >/dev/null 2>&1 \
        || fail "the installed binary could not run on this system"

    echo "Installed uniterm and ut to ${install_dir}"
    install_dir=$(CDPATH='' cd -- "$install_dir" && pwd -P)
    if ! directory_on_path "$install_dir"; then
        echo "Add ${install_dir} to PATH, then run: ut"
    fi
    for installed_name in ut uniterm; do
        installed_dir=$(find_on_path "$installed_name")
        if [ -n "$installed_dir" ] && [ "$installed_dir" != "$install_dir" ]; then
            echo "Warning: ${installed_dir}/${installed_name} takes precedence on PATH. Put ${install_dir} first to use the new installation."
        fi
    done
}

main "$@"
