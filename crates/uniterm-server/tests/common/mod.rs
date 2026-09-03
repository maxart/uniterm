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
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// A Workspace name that cannot collide with anything a user runs: `ut-`
/// followed by the process id and a nanosecond nonce. Callers keep the socket
/// file name at `<name>.sock`, because the server takes the Workspace name
/// from the socket's file stem.
pub fn unique_workspace_name() -> String {
    format!("ut-{}-{}", std::process::id(), nonce())
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

fn nonce() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}
