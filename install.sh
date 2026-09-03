#!/bin/sh
set -eu

REPO=${UNITERM_REPO:-maxart/uniterm}
VERSION=${UNITERM_VERSION:-latest}
INSTALL_DIR=${UNITERM_INSTALL_DIR:-}
DOWNLOAD_ROOT=${UNITERM_DOWNLOAD_ROOT:-https://github.com/${REPO}/releases}
TMP_DIR=

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

select_install_dir() {
    if [ -n "$INSTALL_DIR" ]; then
        echo "$INSTALL_DIR"
        return
    fi
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
    if [ "$(id -u)" -eq 0 ] || [ -w /usr/local/bin ] || command -v sudo >/dev/null 2>&1; then
        echo /usr/local/bin
        return
    fi
    [ -n "${HOME:-}" ] || fail "HOME is unset; set UNITERM_INSTALL_DIR explicitly"
    echo "${HOME}/.local/bin"
}

install_one() {
    install_one_source=$1
    install_one_destination=$2
    install_one_directory=$(dirname "$install_one_destination")
    if [ -d "$install_one_directory" ] && [ -w "$install_one_directory" ]; then
        install -m 0755 "$install_one_source" "$install_one_destination"
    elif [ "$(id -u)" -eq 0 ]; then
        mkdir -p "$install_one_directory"
        install -m 0755 "$install_one_source" "$install_one_destination"
    elif [ "$install_one_directory" = /usr/local/bin ] \
        && command -v sudo >/dev/null 2>&1; then
        sudo mkdir -p "$install_one_directory"
        sudo install -m 0755 "$install_one_source" "$install_one_destination"
    else
        mkdir -p "$install_one_directory"
        install -m 0755 "$install_one_source" "$install_one_destination"
    fi
}

main() {
    command -v install >/dev/null 2>&1 || fail "the install command is required"
    platform=$(detect_platform)
    base_url=$(release_url)
    install_dir=$(select_install_dir)
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
    case ":${PATH}:" in
        *:"${install_dir}":*) ;;
        *) echo "Add ${install_dir} to PATH, then run: ut" ;;
    esac
}

main "$@"
