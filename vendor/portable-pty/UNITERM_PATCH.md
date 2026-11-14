# Uniterm portable-pty patch

This directory contains the crates.io `portable-pty` 0.9.0 sources, examples, manifests, and original MIT license.
The upstream crate checksum is `b4a596a2b3d2752d94f51fac2d4a96737b8705dddd311a32b9af47211f08671e`.
Upstream is [wezterm/wezterm](https://github.com/wezterm/wezterm/tree/main/pty).
Registry cache metadata and the upstream package's standalone lockfile are omitted; Uniterm's root lockfile controls dependency resolution.
Keep `LICENSE.md` verbatim and retain its copyright and permission notice when distributing this code.

The upstream launch-site change is in `src/unix.rs`; `src/unix_cloexec.rs` contains the local descriptor helper.
PTY launch captures a finite descriptor bound in the parent, after preparing stdio and before `Command::spawn`.
The bound combines the current `RLIMIT_NOFILE` soft limit with the highest existing descriptor plus one, including descriptors retained after a previous limit reduction.
Linux and Android enumerate `/proc/self/fd`; other Unix systems enumerate `/dev/fd`.
Enumeration errors, an infinite limit, and descriptor-range overflow fail the launch clearly in the parent.
On Linux and Android, the child first uses the raw `close_range` syscall with `CLOSE_RANGE_CLOEXEC`; using the syscall avoids a new glibc symbol or Android API requirement.
`ENOSYS`, `EINVAL`, and `EPERM` select the portable fallback for older kernels, unsupported flags, or syscall filters; other failures abort the launch.
On older Linux/Android kernels, the fallback opens `/proc/self/fd` in the child and enumerates its actual descriptors using raw `getdents64` into an 8 KiB stack buffer.
Its byte parser checks record bounds, name termination, numeric names, and overflow before applying `fcntl(F_GETFD/F_SETFD)` to mark open descriptors `FD_CLOEXEC`.
The helper never invokes libc directory traversal or allocates in the child.
If child procfs enumeration is unavailable, it uses the captured finite descriptor range.
On macOS, the child scans the captured finite range using `fcntl(F_GETFD/F_SETFD)` directly.
This includes descriptors opened after the parent's enumeration, not just those found in the snapshot.
Interrupted operations retry; closed descriptor slots (`EBADF`) are harmless; other failures return through Rust's exec-error pipe.
Signals, session/process-group creation, controlling-terminal setup, and umask retain their upstream behavior.
The public `close_random_fds` API remains unchanged, but PTY launch no longer invokes it after fork.

This removes libc/standard-library directory iteration and allocation from the PTY child before exec, as required by [Rust's pre_exec safety contract](https://doc.rust-lang.org/std/os/unix/process/trait.CommandExt.html#tymethod.pre_exec).
Marking descriptors instead of closing them preserves Rust's private exec-error pipe even if it reuses a descriptor number from the parent snapshot.
The original blanket close can report a successful spawn when exec actually fails; see [upstream issue 7893](https://github.com/wezterm/wezterm/issues/7893).
This independently reproducible bug does not establish the cause of any particular CI startup timeout.

The kernel range operation and raw child enumeration cover the child's complete descriptor table.
The portable bound covers all currently permitted descriptor numbers plus existing higher descriptors without changing parent flags.
It assumes the parent does not concurrently raise `RLIMIT_NOFILE` after capture; Uniterm does not change that limit in production.
No process-wide resource limit is changed by this patch or its concurrent tests.
The fallback's cost is proportional to the captured bound, so native macOS launch measurements remain part of qualification; Linux/Android only incur that range cost when both the kernel range operation and child procfs enumeration are unavailable.
Never replace this patch with blanket `closefrom(3)`: that would again destroy the exec-error pipe.

## Linux PTY allocation

glibc and musl `openpty` create both descriptors without `O_CLOEXEC`, and upstream marks them only afterwards.
A child that another thread forks and execs in that window, through an ordinary `std::process::Command`, keeps the slave for its whole life.
The master then never reports EOF after the pane's own process exits.
On Linux, `src/unix_openpty.rs` therefore allocates the pair itself, following modern glibc `openpty`, with `O_CLOEXEC` on every descriptor it creates.
The steps are `posix_openpt(O_RDWR | O_NOCTTY | O_CLOEXEC)`, `grantpt` and `unlockpt`.
The slave comes from `TIOCGPTPEER` with the same flags; like glibc, any failure falls back to `ptsname_r` and `open` of the devpts path, for kernels before 4.13.
The initial window size is applied with `TIOCSWINSZ`, whose result glibc and musl also ignore; no termios is applied, as before.
Owned descriptors close the master and slave on every error path, and the error message keeps upstream's `failed to openpty` form.
Linux sets `FD_CLOEXEC` atomically at allocation; the existing idempotent post-construction `cloexec` check is retained unchanged.
Android keeps Uniterm's Bionic `openpty` shim and macOS keeps libc `openpty`, both unchanged.
A descriptor marked close-on-exec can still be held briefly by a concurrently forked child until that child execs; only lifetime retention is removed.

Uniterm regressions live in `crates/uniterm-server/src/pty.rs`:

- `executable_with_missing_shebang_interpreter_reports_spawn_error` checks a real executable whose interpreter cannot be executed, and safely reaps an unexpected successful spawn.
- `inherited_descriptor_is_closed_in_child_without_changing_parent` checks a deliberately inheritable descriptor and unchanged parent flags.
- `descriptor_opened_after_capture_is_marked_close_on_exec`, `fallback_covers_descriptors_opened_after_capture`, and `range_covers_descriptors_opened_after_capture` open a descriptor in the child after parent capture and verify the kernel, raw directory, and final range paths.
- `fallback_preserves_the_exec_error_pipe` checks errno propagation after the forced fallback; `getdents_records_reject_malformed_lengths_names_and_overflow` checks the raw directory parser.
- `linux_pty_pair_is_close_on_exec_from_allocation` checks `FD_CLOEXEC` on the master, the `TIOCGPTPEER` slave and the devpts fallback slave with no later `fcntl` pass, plus peer identity and the applied size.
- `linux_pty_allocation_failure_releases_the_master` fails the slave step after the master exists and requires the master to be closed, observed as hangup on a separately opened slave.
- `pty_slaves_are_not_retained_by_concurrently_spawned_children` allocates PTYs continuously while ordinary children list the descriptors they kept across exec, and requires none of the allocated slaves.
  Spawning starts only after the first allocation, and a drop guard stops the allocator on every exit, including a panic.
- Existing PTY tests cover controlling-terminal behavior, foreground process groups, resizing, and termination/reaping.
- `pty_spawn_latency_measurement` is an explicit ignored measurement: run serially with `--ignored --nocapture --test-threads=1` to measure 200 launches at each of 0 and 64 additional descriptors without changing process-wide limits.
On Linux/Android it additionally measures 200 forced fallback child launches, even on a new kernel.
All measured p95 launch times must remain below 50 ms.

## macOS notification initialization before fork

`unix::prepare_process_spawn` completes libnotify's lazy global allocation once in the parent, using the public `notify_is_valid_token(0)` query.
Every Uniterm fork-based launch calls this shared helper, including PTYs, detached servers, model discovery and standalone test launchers.
Other platforms compile it to a no-op.
The query initializes internal tables and locks without registering notifications or creating descriptors, threads or timers; see Apple's [implementation](https://github.com/apple-oss-distributions/Libnotify/blob/main/notify_client.c) and [public declaration](https://github.com/apple-oss-distributions/Libnotify/blob/main/notify.h).
Subsequent launches only check the completed `Once`; no render, input or idle path calls the helper.

Native production-default qualification captured a PTY child abort in `_notify_fork_child -> _os_alloc_once -> _os_once_gate_corruption_abort`, reporting `os_once_t is corrupt` and `crashed on child side of fork pre-exec`.
Fork can copy a libnotify initialization gate while another parent thread owns it; the child's libSystem atfork handler cannot finish that initialization after the owning thread disappears.
Waiting for initialization in the parent prevents that copied in-progress state, matching the independently documented [Go runtime investigation](https://github.com/golang/go/issues/56784).
Running initialization inside `pre_exec` would be too late because libSystem's child handlers execute first.
This addresses the captured notification initialization crash, not every possible macOS fork failure or the separate unattributed control timeout.
