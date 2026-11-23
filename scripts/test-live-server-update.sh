#!/bin/sh
# Qualify the public updater against an independently linked installation.
# An exported source tree has the public version stamp; the working checkout
# supplies the serving engine. No tag, release, or installed program is changed.
set -eu
root=$(git rev-parse --show-toplevel)
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
mkdir "$scratch/source" "$scratch/bin"
git -C "$root" archive --output="$scratch/source.tar" HEAD
tar -xf "$scratch/source.tar" -C "$scratch/source"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$root/target}"
(cd "$scratch/source" && cargo build --locked -p uniterm-cli --bins)
for name in uniterm ut; do
    cp "$CARGO_TARGET_DIR/debug/$name" "$scratch/bin/$name"
    strip "$scratch/bin/$name"
    if [ "$(uname -s)" = Darwin ]; then
        codesign --force --sign - "$scratch/bin/$name"
    fi
done
cd "$root"
UNITERM_TEST_RELEASE_DIR="$scratch/bin" cargo test --locked -p uniterm-cli --test live_server_update public_update_verifies_the_installed_pair_before_switching_engines -- --exact --ignored --nocapture
