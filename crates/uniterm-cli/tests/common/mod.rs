//! Isolation helpers shared by the integration tests that bind a Workspace.
//!
//! A server owns the durable state of whatever Workspace its socket names: it
//! reads that snapshot and event stream at bind, and deletes them on a clean
//! stop. A test that binds against the real state directory under a name a
//! human also uses therefore destroys real work, so every test binary here
//! points `XDG_STATE_HOME` at a private directory and binds a name no human
//! would type. See `docs/05-session-persistence.md`.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// A Workspace name that cannot collide with anything a user runs: `ut-`
/// followed by the process id and a base-36 nonce. Callers keep the socket
/// file name at `<name>.sock`, because the server takes the Workspace name
/// from the socket's file stem.
pub fn unique_workspace_name() -> String {
    format!("ut-{}-{}", std::process::id(), base36(nonce()))
}

/// Twelve characters instead of nineteen digits: socket paths built from a
/// Workspace name sit near the 104-byte macOS limit, and every character
/// counts.
fn base36(mut value: u128) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    while value > 0 {
        out.push(DIGITS[(value % 36) as usize]);
        value /= 36;
    }
    if out.is_empty() {
        out.push(b'0');
    }
    out.reverse();
    String::from_utf8(out).unwrap()
}

/// Point durable state at a directory private to this test binary and return
/// it. Every test in a file calls this first; the path is computed once, so
/// repeated calls from tests running in parallel set the same value.
/// Provider homes are isolated once too, because startup installs connectors by default.
pub fn isolate_state() -> PathBuf {
    static STATE: OnceLock<PathBuf> = OnceLock::new();
    let state = STATE.get_or_init(|| {
        let state = std::env::temp_dir().join(format!(
            "uniterm-test-state-{}-{}",
            std::process::id(),
            nonce()
        ));
        let home = state.with_extension("provider-home");
        std::fs::create_dir_all(&home).unwrap();
        std::env::set_var("HOME", &home);
        std::env::set_var("XDG_CONFIG_HOME", home.join(".config"));
        std::env::set_var("CODEX_HOME", home.join(".codex"));
        std::env::set_var("GROK_HOME", home.join(".grok"));
        std::env::set_var("PI_CODING_AGENT_DIR", home.join(".pi/agent"));
        std::env::remove_var("CLAUDE_CONFIG_DIR");
        state
    });
    std::fs::create_dir_all(state).unwrap();
    std::env::set_var("XDG_STATE_HOME", state);
    state.clone()
}

/// A root for socket paths short enough for `sockaddr_un` on every host.
///
/// macOS puts `$TMPDIR` under `/var/folders/<..>/T/`, and with a test's own
/// directory plus a `ut-<pid>-<nonce>.sock` file name that lands within a
/// byte or two of the 104-byte limit, so whether a bind succeeded depended
/// on how many digits the pid had. `/tmp` is short on every supported host.
pub fn socket_root() -> PathBuf {
    let dir = std::env::temp_dir();
    if dir.as_os_str().len() <= 16 {
        dir
    } else {
        PathBuf::from("/tmp")
    }
}

/// A directory private to one test, named for `tag` so a failure is traceable.
pub fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "uniterm-test-{tag}-{}-{}",
        std::process::id(),
        nonce()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Wait for a server to publish its socket, panicking rather than hanging.
pub fn wait_for_socket(path: &Path) {
    for _ in 0..300 {
        if path.exists() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("server socket never appeared at {}", path.display());
}

/// Unique within this process even when two tests start in the same clock
/// tick: macOS reports the wall clock in microseconds, and three tests
/// spawned together once drew the same Workspace name and shared a server.
/// The counter takes the nanosecond digits, so the value stays as long as a
/// nanosecond timestamp and paths built from two nonces still fit a socket.
/// The same nonce for tests that build their own unique directories.
pub fn unique_nonce() -> u128 {
    nonce()
}

fn nonce() -> u128 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros();
    micros * 1000 + u128::from(COUNTER.fetch_add(1, Ordering::Relaxed) % 1000)
}

/// Failure-only subprocess diagnostics must not hang a test or fill a pipe.
/// File-backed output also lets the deadline apply when the child is silent.
pub fn bounded_diagnostic(
    command: &mut std::process::Command,
    label: &str,
    timeout: std::time::Duration,
) -> String {
    bounded_diagnostic_with_limit(command, label, timeout, 64 * 1024)
}

fn bounded_diagnostic_with_limit(
    command: &mut std::process::Command,
    label: &str,
    timeout: std::time::Duration,
    limit: u64,
) -> String {
    use std::io::Read as _;
    use std::process::{Child, Stdio};
    use std::time::Instant;

    struct Capture {
        child: Option<Child>,
        directory: PathBuf,
    }
    impl Drop for Capture {
        fn drop(&mut self) {
            if let Some(child) = &mut self.child {
                let _ = child.kill();
                let _ = child.wait();
            }
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    let mut capture = Capture {
        child: None,
        directory: temp_dir("diagnostic"),
    };
    let output = capture.directory.join("output");
    let file = match std::fs::File::create(&output) {
        Ok(file) => file,
        Err(error) => return format!("{label}: capture failed: {error}"),
    };
    let stderr = match file.try_clone() {
        Ok(file) => file,
        Err(error) => return format!("{label}: capture clone failed: {error}"),
    };
    match command
        .stdin(Stdio::null())
        .stdout(file)
        .stderr(stderr)
        .spawn()
    {
        Ok(child) => capture.child = Some(child),
        Err(error) => return format!("{label}: spawn failed: {error}"),
    }
    let child = capture.child.as_mut().unwrap();
    let pid = child.id();
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.to_string(),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Ok(None) => {
                let _ = child.kill();
                break format!("timed out after {timeout:?}; kill/reap: {:?}", child.wait());
            }
            Err(error) => break format!("try_wait failed: {error}"),
        }
    };
    let mut bytes = Vec::new();
    let read =
        std::fs::File::open(&output).and_then(|file| file.take(limit).read_to_end(&mut bytes));
    format!(
        "{label}: pid={pid} status={status}; output read={read:?} (at most {} KiB)\n{}",
        limit / 1024,
        String::from_utf8_lossy(&bytes)
    )
}

/// Fixture logs can contain a backtrace; bound their contribution to CI output.
pub fn diagnostic_file(path: &Path) -> String {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    match std::fs::File::open(path).and_then(|file| file.take(64 * 1024).read_to_end(&mut bytes)) {
        Ok(_) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(error) => format!("{}: {error}", path.display()),
    }
}

/// Sample only the failed fixture and its immediate children, once on failure.
/// In particular, a child stalled after fork can reveal allocator activity in
/// a PTY pre-exec hook without inspecting unrelated processes on the runner.
pub fn startup_diagnostics(server_pid: u32) -> String {
    #[cfg(target_os = "macos")]
    {
        use std::process::Command;
        use std::time::Duration;

        let snapshot = bounded_diagnostic(
            Command::new("/bin/ps").args(["-axo", "pid=,ppid="]),
            "process parent snapshot",
            Duration::from_secs(1),
        );
        let mut children: Vec<u32> = snapshot
            .lines()
            .filter_map(|line| {
                let mut columns = line.split_whitespace();
                let pid = columns.next()?.parse::<u32>().ok()?;
                let parent = columns.next()?.parse::<u32>().ok()?;
                (parent == server_pid).then_some(pid)
            })
            .collect();
        children.sort_unstable();
        let mut report =
            format!("fixture server pid={server_pid}; immediate children={children:?}\n");
        // Bound both diagnostic duration and log volume if the fixture leaked
        // children; all child ids remain visible even when samples are capped.
        for pid in std::iter::once(server_pid).chain(children.into_iter().take(3)) {
            report.push_str(&bounded_diagnostic_with_limit(
                Command::new("/usr/bin/sample").args([
                    &pid.to_string(),
                    "1",
                    "1",
                    "-file",
                    "/dev/stdout",
                ]),
                &format!("fixture sample {pid}"),
                Duration::from_secs(3),
                256 * 1024,
            ));
            report.push('\n');
        }
        report
    }
    #[cfg(not(target_os = "macos"))]
    {
        format!("fixture server pid={server_pid}; process samples are macOS-only")
    }
}
