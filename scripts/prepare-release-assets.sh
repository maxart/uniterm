#!/bin/sh
set -eu

if [ "$#" -ne 1 ]; then
    echo "usage: scripts/prepare-release-assets.sh OUTPUT_DIR" >&2
    exit 2
fi

repo=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
output=$1
mkdir -p "$output"

for existing in "$output"/*; do
    [ -e "$existing" ] || continue
    echo "refusing to mix release assets with existing path: $existing" >&2
    exit 1
done

copy_asset() {
    platform=$1
    published=$2
    source="$repo/target/dist/$platform"
    [ -f "$source/uniterm" ] || {
        echo "missing artifact: $source/uniterm" >&2
        exit 1
    }
    [ -f "$source/ut" ] || {
        echo "missing artifact: $source/ut" >&2
        exit 1
    }
    install -m 0755 "$source/uniterm" "$output/uniterm-$published"
    install -m 0755 "$source/ut" "$output/ut-$published"
}

copy_asset macos-arm64 macos-arm64
copy_asset ubuntu-x86_64 linux-x86_64
copy_asset ubuntu-aarch64 linux-aarch64
copy_asset android-aarch64 android-aarch64

(
    cd "$output"
    sha256sum uniterm-* ut-* > SHA256SUMS
)
