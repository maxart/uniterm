#!/bin/sh
set -eu

if [ "$#" -ne 1 ]; then
    echo "usage: scripts/verify-release-assets.sh RELEASE_DIR" >&2
    exit 2
fi

release=$1
expected='SHA256SUMS
uniterm-android-aarch64
uniterm-linux-aarch64
uniterm-linux-x86_64
uniterm-macos-arm64
ut-android-aarch64
ut-linux-aarch64
ut-linux-x86_64
ut-macos-arm64'
actual=$(find "$release" -mindepth 1 -maxdepth 1 -type f -printf '%f\n' | sort)
[ "$actual" = "$expected" ] || {
    echo "release asset inventory is not exact" >&2
    printf 'expected:\n%s\nactual:\n%s\n' "$expected" "$actual" >&2
    exit 1
}

(
    cd "$release"
    sha256sum -c SHA256SUMS
)

for binary in "$release"/uniterm-* "$release"/ut-*; do
    [ -x "$binary" ] || {
        echo "release binary is not executable: $binary" >&2
        exit 1
    }
done

file "$release"/uniterm-macos-arm64 | grep -q 'Mach-O 64-bit arm64 executable'
file "$release"/ut-macos-arm64 | grep -q 'Mach-O 64-bit arm64 executable'
file "$release"/uniterm-linux-x86_64 | grep -q 'ELF 64-bit.*x86-64'
file "$release"/ut-linux-x86_64 | grep -q 'ELF 64-bit.*x86-64'
file "$release"/uniterm-linux-aarch64 | grep -q 'ELF 64-bit.*ARM aarch64'
file "$release"/ut-linux-aarch64 | grep -q 'ELF 64-bit.*ARM aarch64'
file "$release"/uniterm-android-aarch64 | grep -q 'ELF 64-bit.*ARM aarch64'
file "$release"/ut-android-aarch64 | grep -q 'ELF 64-bit.*ARM aarch64'
for binary in "$release"/uniterm-macos-arm64 "$release"/ut-macos-arm64; do
    llvm-objdump --macho --private-headers "$binary" \
        | grep -q 'minos 13\.0'
done
readelf -l "$release/uniterm-linux-x86_64" | grep -q '/lib64/ld-linux-x86-64.so.2'
readelf -l "$release/uniterm-linux-aarch64" | grep -q '/lib/ld-linux-aarch64.so.1'
readelf -l "$release/uniterm-android-aarch64" | grep -q '/system/bin/linker64'

check_glibc_baseline() {
    binary=$1
    baseline=$2
    maximum=$(readelf --version-info --wide "$binary" \
        | sed -n 's/.*Name: GLIBC_\([0-9.]*\).*/\1/p' \
        | sort -V \
        | tail -n 1)
    [ -n "$maximum" ] || {
        echo "no GLIBC requirements found in $binary" >&2
        exit 1
    }
    highest=$(printf '%s\n%s\n' "$maximum" "$baseline" | sort -V | tail -n 1)
    [ "$highest" = "$baseline" ] || {
        echo "$binary requires GLIBC $maximum, above $baseline" >&2
        exit 1
    }
}

check_glibc_baseline "$release/uniterm-linux-x86_64" 2.17
check_glibc_baseline "$release/ut-linux-x86_64" 2.17
check_glibc_baseline "$release/uniterm-linux-aarch64" 2.17
check_glibc_baseline "$release/ut-linux-aarch64" 2.17

# A release binary reports the bare Cargo version; a build of any other
# commit carries a -dev suffix, and must not be published under this tag.
expected_version="uniterm $(cargo metadata --no-deps --format-version 1 \
    | jq -r '.packages[] | select(.name == "uniterm-cli") | .version')"
for binary in "$release"/uniterm-linux-x86_64 "$release"/ut-linux-x86_64; do
    reported=$("$binary" --version)
    [ "$reported" = "$expected_version" ] || {
        echo "$binary reports '$reported', expected '$expected_version'" >&2
        exit 1
    }
done

protocol=$(sed -n \
    's/^pub const WIRE_PROTOCOL_VERSION: u32 = \([0-9][0-9]*\);$/\1/p' \
    crates/uniterm-proto/src/lib.rs)
[ -n "$protocol" ] || {
    echo "could not resolve WIRE_PROTOCOL_VERSION" >&2
    exit 1
}
"$release/uniterm-linux-x86_64" remote-check --protocol "$protocol"
"$release/ut-linux-x86_64" remote-check --protocol "$protocol"
