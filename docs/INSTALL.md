# Installing Uniterm

This page covers every way to get the `uniterm` and `ut` binaries onto a machine, where Uniterm keeps its state, and how release binaries are built.
For the two-line quick start, see the [README](../README.md).

## Install a release

Install the latest release with one command:

```sh
curl -fsSL https://raw.githubusercontent.com/maxart/uniterm/main/install.sh | sh
```

The installer downloads `uniterm` and its recommended `ut` alias from the latest GitHub release, verifies both against the release's SHA-256 manifest, and installs them in `/usr/local/bin` when possible.
It falls back to `~/.local/bin` when privilege escalation is unavailable and uses `$PREFIX/bin` on Termux.
Set `UNITERM_INSTALL_DIR` to choose another destination or `UNITERM_VERSION=v1.0.0` to pin a release.

Prebuilt releases support Apple Silicon macOS, glibc Linux on x86-64 and ARM64, x86-64 or ARM64 WSL, and AArch64 Android Termux.
Windows users should run the command inside WSL.
Intel macOS and native Windows are not supported.

```sh
curl -fsSL https://raw.githubusercontent.com/maxart/uniterm/main/install.sh | UNITERM_INSTALL_DIR="$HOME/.local/bin" sh
```

## Build from source

Building requires Rust 1.96 or newer and a C toolchain: Xcode Command Line Tools on macOS or `build-essential`/`gcc` on Linux.

```sh
git clone https://github.com/maxart/uniterm.git
cd uniterm
cargo build --release --workspace
cargo install --path crates/uniterm-cli --bins
```

`cargo install` places the `uniterm` and `ut` binaries in `~/.cargo/bin`, which should be on your `PATH` (rustup adds it).
`ut` is a real second binary, not a shell alias, and is the recommended way to invoke Uniterm.

Note: if you previously had Uniterm Desktop installed, its `uniterm` executable may shadow this one on your `PATH`.
Prefer `ut`, or remove the old binary, so you are running this build.

State is stored under the XDG directories: Workspace snapshots and event logs live in `$XDG_STATE_HOME/uniterm/` (default `~/.local/state/uniterm/`), and config is read from `$XDG_CONFIG_HOME/uniterm/uniterm.conf` (default `~/.config/uniterm/uniterm.conf`).
Runtime and state directories are owner-only (0700), and Workspace sockets and state files are owner-only (0600).
Snapshots and event logs can contain terminal output and project metadata, so treat the state directory as sensitive local data even though it is not sent anywhere or encrypted at rest.

## Reproducible release builds

Cross-platform release binaries always go under `target/dist/<os>-<arch>/`.
Each platform folder contains only the executable `uniterm` and `ut` binaries, with no archives, versioned folders, checksums, or build caches.
Use the canonical builder instead of invoking `cargo zigbuild` directly:

```sh
scripts/build-dist.sh macos-arm64
scripts/build-dist.sh ubuntu-x86_64
scripts/build-dist.sh ubuntu-aarch64
scripts/build-dist.sh arch-x86_64
scripts/build-dist.sh fedora-x86_64
scripts/build-dist.sh android-aarch64
```

The macOS target is Apple Silicon only.
Intel and universal macOS builds are intentionally unsupported.
The Android target is AArch64 Linux for Termux and uses API level 24 by default.
Native Termux builds use Cargo directly.
Cross-builds accept `ANDROID_NDK_HOME`, `ANDROID_NDK_ROOT`, or the legacy `ANDROID_NDK` variable; set `ANDROID_API_LEVEL` to override the minimum API level.
The legacy NDK r10e package automatically falls back to API 21 when no API level is explicitly requested.

