# Installing Uniterm

This page covers every way to get the `uniterm` and `ut` binaries onto a machine, where Uniterm keeps its state, and how release binaries are built.
For the two-line quick start, see the [README](../README.md).

## Install a release

Install the latest release with one command:

```sh
curl -fsSL https://raw.githubusercontent.com/maxart/uniterm/main/install.sh | sh
```

The installer downloads `uniterm` and its recommended `ut` alias from the latest GitHub release and verifies both against the release's SHA-256 manifest before installing.
It reuses the directory of `ut` on `PATH`, or `uniterm` if only the long command is present.
Outside Termux, it also recognizes existing binaries in `~/.local/bin` and `$CARGO_HOME/bin` (default `~/.cargo/bin`) when those directories are not on `PATH`.
A fresh installation uses `~/.local/bin`, or `$PREFIX/bin` on Termux, without invoking sudo.
If an existing installation needs elevated permissions, the installer stops with rerun instructions instead of creating a second copy or invoking sudo automatically.
If the two commands resolve to different directories, `ut` determines the destination and the installer reports any command still shadowing the new installation.
Executable symlinks are replaced in the selected directory without changing their targets.
Set `UNITERM_INSTALL_DIR` to choose another destination or `UNITERM_VERSION=v1.0.0` to pin a release.

For a system installation in `/usr/local/bin`, explicitly allow sudo with `--system`:

```sh
curl -fsSL https://raw.githubusercontent.com/maxart/uniterm/main/install.sh | sh -s -- --system
```

`UNITERM_INSTALL_DIR` takes priority over `--system`, so both can be used together to update a protected custom installation.
Only the final directory creation and binary installation use sudo when needed; downloading and checksum verification run as the invoking user.

Prebuilt releases support Apple Silicon macOS, glibc Linux on x86-64 and ARM64, x86-64 or ARM64 WSL, and AArch64 Android Termux.
Windows users should run the command inside WSL.
Intel macOS and native Windows are not supported.

```sh
curl -fsSL https://raw.githubusercontent.com/maxart/uniterm/main/install.sh | UNITERM_INSTALL_DIR="$HOME/.local/bin" sh
```

If the destination is not on `PATH`, the installer prints the directory to add before running `ut`.

## Update an existing installation

Run `ut update --check` to check the public release, then `ut update` to install it.
Use `ut update --version v1.2.1` to select a specific stable release, including an intentional downgrade.
The updater requires `curl`, verifies both assets against the release's SHA-256 manifest, checks that they run and report the selected version, and replaces `uniterm` and `ut` beside the executable you invoked.
It uses atomic renames so already-running programs keep their existing executable, and rolls back the first replacement if replacing the second binary fails.
It never invokes sudo automatically; protected installations require an account with write access to that directory.
If GitHub release metadata is unavailable or rate limited, the updater resolves the public latest stable release and verifies the same versioned assets.
An explicitly selected older version still requires release metadata to confirm that it is stable.
Download failures report the requested endpoint and HTTP status so network restrictions can be distinguished from missing releases.

About > Update and Settings > Update Uniterm run the same updater after returning the terminal to normal input mode.
The menu checks once, asks before installing, and reattaches to the selected Workspace with the new client when its attach protocol is compatible.
If another terminal already installed that release, the updater verifies the installed pair against the manifest and reuses it.
If activation fails before switching executables, the current client can reattach.
On a remote attach this updates the local client installation; run `ut update` on the remote host to update that host.
There are no background update checks.

Installing a release and switching a running server are separate operations.
After installation, run `ut update --activate-server --workspace NAME` on the owning host to transfer a compatible Workspace to that release.
Use `--version vX.Y.Z` when the installed release differs from the CLI invoking the command.
The serving engine verifies both binaries against the public release manifest before starting the replacement.
Release executables and the installation directory must be protected against writes by other accounts.
The installer creates missing installation directories without granting group or other write access, including when the shell uses umask 002.
A group-writable installation under a private, owner-only home can qualify.
For a pre-existing group-writable directory under a searchable home, remove group write access from the path named in the update diagnostic if it is intended for personal use, or choose a protected installation path.
A shared prefix that another user can modify, such as a staff-writable `/usr/local`, requires installing in a protected directory before live activation.
On Termux the Android system account controls the package-data ancestors of the application's private prefix.
Repeating activation on the same verified running image avoids a second transfer.
Shells, foreground jobs and attached clients keep their existing connections.
If an operation prevents transfer, the command reports the reason and the original engine continues serving.
Run `ut update --status` in a pane or `ut update --status --workspace NAME --json` to see the CLI version and the running server's version and attach protocol.
This query works offline.
Older servers report an unknown build.
When first upgrading from a client that predates menu activation, detach and reattach once to start the new client while the Workspace keeps running.
Servers that predate live activation pick up server fixes on their next normal start.

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
Sockets normally live under `$XDG_RUNTIME_DIR/uniterm/`.
If the variable points to a missing, relative, non-directory, foreign-owned, shared-writable, or unwritable directory, Uniterm warns once per process and uses its private `/tmp/uniterm-<uid>/uniterm/` fallback, also used when the variable is unset.
Android instead uses the existing Termux temporary-directory fallback.
Uniterm does not recreate or change permissions on the session manager's runtime directory.
This handles stale WSL or SSH environments where the variable survives without a corresponding login session.
Launch, attach, discovery, and stop share the same resolver; durable-state locks still prevent duplicate servers when sessions resolve different socket directories.

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
On Apple Silicon the canonical builder uses Cargo and the native Apple SDK.
Cross-building macOS on Linux requires `SDKROOT` pointing to a macOS SDK containing CoreServices and CoreFoundation for the native FSEvents watcher.
Public releases build the Mac pair on an Apple Silicon runner.
Intel and universal macOS builds are intentionally unsupported.
The Android target is AArch64 Linux for Termux and uses API level 24 by default.
Native Termux builds use Cargo directly.
Cross-builds accept `ANDROID_NDK_HOME`, `ANDROID_NDK_ROOT`, or the legacy `ANDROID_NDK` variable; set `ANDROID_API_LEVEL` to override the minimum API level.
The legacy NDK r10e package automatically falls back to API 21 when no API level is explicitly requested.

