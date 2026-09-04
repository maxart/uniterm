#!/bin/sh
set -eu

repo=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
root=$(mktemp -d "${TMPDIR:-/tmp}/uniterm-installer-test.XXXXXX")

cleanup() {
    rm -rf -- "$root"
}
trap cleanup EXIT HUP INT TERM

case "$(uname -s):$(uname -m)" in
    Linux:x86_64 | Linux:amd64) platform=linux-x86_64 ;;
    *)
        echo "installer integration test requires Linux x86-64" >&2
        exit 77
        ;;
esac

release="$root/releases/latest/download"
destination="$root/bin"
tools="$root/tools"
sudo_marker="$root/sudo-called"
mkdir -p "$release" "$tools"
printf '#!/bin/sh\necho uniterm-test\n' > "$release/uniterm-$platform"
printf '#!/bin/sh\necho ut-test\n' > "$release/ut-$platform"
printf '#!/bin/sh\n: > "%sUNITERM_TEST_SUDO_MARKER"\nexit 99\n' '$' > "$tools/sudo"
chmod 0755 "$release/uniterm-$platform" "$release/ut-$platform"
chmod 0755 "$tools/sudo"
(
    cd "$release"
    sha256sum "uniterm-$platform" "ut-$platform" > SHA256SUMS
)

UNITERM_DOWNLOAD_ROOT="file://$root/releases" \
UNITERM_INSTALL_DIR="$destination" \
UNITERM_INSTALLER_TESTING=1 \
UNITERM_TEST_SUDO_MARKER="$sudo_marker" \
PATH="$tools:$PATH" \
sh "$repo/install.sh"

[ ! -e "$sudo_marker" ]
[ "$("$destination/uniterm")" = uniterm-test ]
[ "$("$destination/ut")" = ut-test ]

printf '0  uniterm-%s\n0  ut-%s\n' "$platform" "$platform" > "$release/SHA256SUMS"
if UNITERM_DOWNLOAD_ROOT="file://$root/releases" \
    UNITERM_INSTALL_DIR="$destination" \
    UNITERM_INSTALLER_TESTING=1 \
    sh "$repo/install.sh" >/dev/null 2>&1
then
    echo "installer accepted an invalid checksum" >&2
    exit 1
fi

[ "$("$destination/uniterm")" = uniterm-test ]
[ "$("$destination/ut")" = ut-test ]
echo "installer integration test passed"
