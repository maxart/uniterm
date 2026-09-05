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
pub fn isolate_state() -> PathBuf {
    static STATE: OnceLock<PathBuf> = OnceLock::new();
    let state = STATE.get_or_init(|| {
        std::env::temp_dir().join(format!(
            "uniterm-test-state-{}-{}",
            std::process::id(),
            nonce()
        ))
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
