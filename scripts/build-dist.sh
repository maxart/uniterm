#!/bin/sh
set -eu

usage() {
    echo "usage: scripts/build-dist.sh <macos-arm64|ubuntu-x86_64|ubuntu-aarch64|arch-x86_64|fedora-x86_64|android-aarch64>" >&2
    exit 2
}

[ "$#" -eq 1 ] || usage
platform=$1

case "$platform" in
    macos-arm64)
        rust_target=aarch64-apple-darwin
        output_target=aarch64-apple-darwin
        build_backend=zig
        ;;
    macos-*)
        echo "Uniterm never builds non-ARM macOS artifacts; use macos-arm64" >&2
        exit 2
        ;;
    ubuntu-x86_64)
        rust_target=x86_64-unknown-linux-gnu.2.17
        output_target=x86_64-unknown-linux-gnu
        build_backend=zig
        ;;
    ubuntu-aarch64)
        rust_target=aarch64-unknown-linux-gnu.2.17
        output_target=aarch64-unknown-linux-gnu
        build_backend=zig
        ;;
    arch-x86_64)
        rust_target=x86_64-unknown-linux-gnu.2.17
        output_target=x86_64-unknown-linux-gnu
        build_backend=zig
        ;;
    fedora-x86_64)
        rust_target=x86_64-unknown-linux-gnu.2.28
        output_target=x86_64-unknown-linux-gnu
        build_backend=zig
        ;;
    android-aarch64)
        rust_target=aarch64-linux-android
        output_target=aarch64-linux-android
        build_backend=android
        ;;
    *)
        echo "unsupported distribution platform: $platform" >&2
        echo "add an explicit <os>-<arch> mapping to scripts/build-dist.sh first" >&2
        exit 2
        ;;
esac

if [ "$build_backend" = zig ]; then
    command -v cargo-zigbuild >/dev/null 2>&1 || {
        echo "cargo-zigbuild is required" >&2
        exit 1
    }
    command -v zig >/dev/null 2>&1 || {
        echo "zig is required" >&2
        exit 1
    }
fi

repo=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
build_root=$(mktemp -d /tmp/uniterm-dist-build.XXXXXX)

cleanup() {
    case "$build_root" in
        /tmp/uniterm-dist-build.*) rm -rf -- "$build_root" ;;
        *) echo "refusing to remove unexpected build directory: $build_root" >&2 ;;
    esac
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

cd "$repo"
if [ "$build_backend" = zig ]; then
    env \
        XDG_CACHE_HOME="$build_root/xdg-cache" \
        CARGO_TARGET_DIR="$build_root/target" \
        ZIG_GLOBAL_CACHE_DIR="$build_root/zig-global-cache" \
        ZIG_LOCAL_CACHE_DIR="$build_root/zig-local-cache" \
        cargo zigbuild \
            --locked \
            --release \
            -p uniterm-cli \
            --bins \
            --target "$rust_target"
else
    host_target=$(rustc -vV | sed -n 's/^host: //p')
    if [ "$host_target" = "$rust_target" ]; then
        env \
            CARGO_TARGET_DIR="$build_root/target" \
            cargo build \
                --locked \
                --release \
                -p uniterm-cli \
                --bins \
                --target "$rust_target"
    else
        android_ndk=${ANDROID_NDK_HOME:-${ANDROID_NDK_ROOT:-${ANDROID_NDK:-}}}
        [ -n "$android_ndk" ] || {
            echo "ANDROID_NDK_HOME, ANDROID_NDK_ROOT, or ANDROID_NDK must point to an Android NDK" >&2
            exit 1
        }
        android_api=${ANDROID_API_LEVEL:-24}
        android_api_explicit=0
        [ -z "${ANDROID_API_LEVEL:-}" ] || android_api_explicit=1
        android_linker=
        android_rustflags=
        for candidate in "$android_ndk"/toolchains/llvm/prebuilt/*/bin/aarch64-linux-android"$android_api"-clang; do
            [ -x "$candidate" ] || continue
            android_linker=$candidate
            break
        done
        if [ -z "$android_linker" ]; then
            if [ "$android_api_explicit" -eq 0 ] \
                && [ ! -d "$android_ndk/platforms/android-$android_api/arch-arm64" ] \
                && [ -d "$android_ndk/platforms/android-21/arch-arm64" ]
            then
                android_api=21
                echo "using legacy Android NDK API 21 fallback" >&2
            fi
            android_sysroot="$android_ndk/platforms/android-$android_api/arch-arm64"
            android_unwind="$android_ndk/sources/android/gccunwind/libs/arm64-v8a/libgccunwind.a"
            for candidate in "$android_ndk"/toolchains/aarch64-linux-android-4.9/prebuilt/*/bin/aarch64-linux-android-gcc; do
                [ -x "$candidate" ] || continue
                [ -d "$android_sysroot" ] || continue
                [ -f "$android_unwind" ] || continue
                android_linker=$candidate
                mkdir -p "$build_root/android-libs"
                install -m 0644 "$android_unwind" "$build_root/android-libs/libunwind.a"
                android_rustflags="-L native=$build_root/android-libs -C link-arg=--sysroot=$android_sysroot"
                break
            done
        fi
        [ -n "$android_linker" ] || {
            echo "missing AArch64 Android API $android_api linker under $android_ndk" >&2
            exit 1
        }
        if [ -n "$android_rustflags" ]; then
            env \
                CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$android_linker" \
                CARGO_TARGET_AARCH64_LINUX_ANDROID_RUSTFLAGS="$android_rustflags" \
                CARGO_TARGET_DIR="$build_root/target" \
                cargo build \
                    --locked \
                    --release \
                    -p uniterm-cli \
                    --bins \
                    --target "$rust_target"
        else
            env \
                CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$android_linker" \
                CARGO_TARGET_DIR="$build_root/target" \
                cargo build \
                    --locked \
                    --release \
                    -p uniterm-cli \
                    --bins \
                    --target "$rust_target"
        fi
    fi
fi

source_dir="$build_root/target/$output_target/release"
dist_dir="$repo/target/dist/$platform"
mkdir -p "$dist_dir"

for existing in "$dist_dir"/*; do
    [ -e "$existing" ] || continue
    case "$(basename -- "$existing")" in
        uniterm | ut) ;;
        *)
            echo "refusing to overwrite unexpected distribution artifact: $existing" >&2
            exit 1
            ;;
    esac
done

install -m 0755 "$source_dir/uniterm" "$dist_dir/uniterm"
install -m 0755 "$source_dir/ut" "$dist_dir/ut"

echo "$dist_dir/uniterm"
echo "$dist_dir/ut"
