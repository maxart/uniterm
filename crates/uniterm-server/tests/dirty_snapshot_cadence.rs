//! Crash checkpoints advance from PTY damage without a structural mutation.

use std::io::Write as _;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use uniterm_proto::{encode_frame, ClientMessage};

fn temp_root() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!("ut-dirty-snapshot-{}-{nonce}", std::process::id()))
}

fn wait_for(path: &Path) {
    for _ in 0..300 {
        if path.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("socket never appeared at {}", path.display());
}

#[test]
fn pty_output_advances_the_crash_snapshot_without_a_structural_change() {
    let root = temp_root();
    std::fs::create_dir_all(&root).unwrap();
    std::env::set_var("XDG_STATE_HOME", &root);
    let workspace = "dirty-cadence";
    let socket = root.join(format!("{workspace}.sock"));
    let server_socket = socket.clone();
    let server = thread::spawn(move || {
        let _ = uniterm_server::run_server(
            &server_socket,
            "/bin/sh",
            &["-c", "sleep 1; printf dirty-checkpoint-marker; sleep 30"],
        );
    });
    wait_for(&socket);

    let deadline = Instant::now() + Duration::from_secs(6);
    let captured = loop {
        let captured = uniterm_server::persist::load(workspace).is_some_and(|snapshot| {
            snapshot.windows.iter().any(|window| {
                window.panes.iter().any(|pane| {
                    pane.content.iter().any(|line| {
                        line.cells
                            .iter()
                            .map(|cell| cell.text.as_str())
                            .collect::<String>()
                            .contains("dirty-checkpoint-marker")
                    })
                })
            })
        });
        if captured || Instant::now() >= deadline {
            break captured;
        }
        thread::sleep(Duration::from_millis(50));
    };

    let mut client = UnixStream::connect(&socket).unwrap();
    let _ = client.write_all(&encode_frame(&ClientMessage::KillServer));
    let _ = client.flush();
    let _ = server.join();
    let _ = std::fs::remove_dir_all(&root);

    assert!(
        captured,
        "PTY output did not reach the damage-armed crash checkpoint"
    );
}
