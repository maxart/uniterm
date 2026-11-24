//! A local-to-remote round trip must keep the physical terminal and both shells alive.
mod common;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use uniterm_core::machine::{MachineEvent, MachineProfile};
use uniterm_proto::ControlCommand;
use uniterm_server::{pty::PtyProcess, Terminal};

const BIN: &str = env!("CARGO_BIN_EXE_ut");

struct Hosts {
    root: PathBuf,
    children: Vec<Child>,
}

impl Drop for Hosts {
    fn drop(&mut self) {
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

impl Hosts {
    fn start(&mut self, workspace: &str, shell: &Path) -> PathBuf {
        let child = Command::new(BIN)
            .args(["serve", workspace])
            .env("XDG_RUNTIME_DIR", &self.root)
            .env("XDG_STATE_HOME", &self.root)
            .env("XDG_CONFIG_HOME", &self.root)
            .env("SHELL", shell)
            .env_remove("UNITERM_SOCKET")
            .env_remove("UNITERM_PANE_ID")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        self.children.push(child);
        let socket = self.root.join("uniterm").join(format!("{workspace}.sock"));
        common::wait_for_socket(&socket);
        socket
    }
}

struct Attach {
    pty: PtyProcess,
    screen: Terminal,
    transcript: Vec<u8>,
}

impl Drop for Attach {
    fn drop(&mut self) {
        let _ = self.pty.kill();
    }
}

impl Attach {
    fn wait_for(&mut self, text: &str) {
        self.wait_screen(text, |frame| frame.contains(text));
    }
    fn wait_screen(&mut self, expected: &str, matches: impl Fn(&str) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut buf = [0; 8192];
        while Instant::now() < deadline {
            match self.pty.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    self.transcript.extend_from_slice(&buf[..n]);
                    self.screen.feed(&buf[..n]);
                    if matches(&self.screen.dump_text()) {
                        return;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => panic!("terminal: {e}"),
            }
        }
        panic!("missing {expected:?}: {}", self.screen.dump_text());
    }

    fn key(&mut self, bytes: &[u8]) {
        assert_eq!(self.pty.write_some(bytes).unwrap(), bytes.len());
    }
}

fn script(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn open_servers(socket: &Path) {
    let reply = uniterm_client::control_request(socket, ControlCommand::ServersOpen).unwrap();
    assert!(reply.error.is_none(), "{:?}", reply.error);
}

#[test]
fn server_switch_keeps_one_terminal_and_returns_to_the_original_shell() {
    common::isolate_state();
    let mut hosts = Hosts {
        root: common::temp_dir("switch"),
        children: Vec::new(),
    };
    let bin = hosts.root.join("bin");
    fs::create_dir(&bin).unwrap();
    fs::create_dir_all(hosts.root.join("uniterm")).unwrap();
    fs::write(
        hosts.root.join("uniterm/uniterm.conf"),
        "sidebar = true\nauto-install-connectors = false\nauto-update-connectors = false\n",
    )
    .unwrap();
    let local = common::unique_workspace_name();
    let remote = common::unique_workspace_name();
    let local_shell = bin.join("local-shell");
    let remote_shell = bin.join("remote-shell");
    script(
        &local_shell,
        "printf 'LOCAL-SHELL-%s-ready\\n' \"$$\"; exec /bin/cat",
    );
    script(
        &remote_shell,
        "printf 'REMOTE-SHELL-%s\\n' \"$$\"; exec /bin/cat",
    );
    let local_socket = hosts.start(&local, &local_shell);
    let remote_socket = hosts.start(&remote, &remote_shell);
    script(
        &bin.join("ssh"),
        r#"
for arg in "$@"; do
    case "$arg" in
        slow) exec sleep 60 ;;
        dropped) printf 'UNITERM-REMOTE/1\n'; dd bs=1 count=8 of=/dev/null 2>/dev/null; exit 0 ;;
        denied) printf 'Permission denied (fixture).\n' >&2; exit 255 ;;
    esac
done
exec "$UT_TEST_BINARY" remote-bridge --protocol "$UT_TEST_PROTOCOL" "$UT_TEST_REMOTE"
"#,
    );
    let catalog = hosts.root.join("uniterm/machines.jsonl");
    for (label, target) in [
        ("Build", "builder"),
        ("Unavailable", "denied"),
        ("Slow", "slow"),
        ("Dropped", "dropped"),
    ] {
        uniterm_client::machines::update_at(&catalog, |_, id| {
            Ok(MachineEvent::Saved(MachineProfile {
                id,
                label: label.into(),
                target: target.into(),
                workspace: remote.clone(),
                enabled: true,
            }))
        })
        .unwrap();
    }

    let path = format!("{}:/usr/bin:/bin", bin.display());
    let protocol = uniterm_proto::WIRE_PROTOCOL_VERSION.to_string();
    let root = hosts.root.to_str().unwrap();
    let pty = PtyProcess::spawn(
        BIN,
        &["attach", &local],
        110,
        30,
        None,
        &[
            ("XDG_RUNTIME_DIR", root),
            ("XDG_STATE_HOME", root),
            ("XDG_CONFIG_HOME", root),
            ("UT_TEST_BINARY", BIN),
            ("UT_TEST_PROTOCOL", &protocol),
            ("UT_TEST_REMOTE", &remote),
            ("PATH", &path),
            ("TERM", "xterm-256color"),
        ],
    )
    .unwrap();
    pty.set_nonblocking().unwrap();
    let mut client = Attach {
        pty,
        screen: Terminal::new(110, 30),
        transcript: Vec::new(),
    };
    client.wait_for("-ready");
    let screen = client.screen.dump_text();
    let (_, suffix) = screen.split_once("LOCAL-SHELL-").unwrap();
    let (pid, _) = suffix.split_once("-ready").unwrap();
    assert!(pid.parse::<u32>().is_ok());
    let original_marker = format!("LOCAL-SHELL-{pid}-ready");
    let local_pane = |socket: &Path| uniterm_client::pane_list(socket).unwrap().1[0].clone();
    let original = local_pane(&local_socket);
    open_servers(&local_socket);
    client.wait_for("Connected to Local");
    client.key(b"\r");
    client.wait_for("REMOTE-SHELL-");
    open_servers(&remote_socket);
    client.wait_for(&format!("Connected to builder / {remote}"));

    // A failed remote-to-remote switch keeps the active endpoint available.
    client.key(b"\x1b[B\r");
    client.wait_for("Permission denied");
    assert!(client.screen.dump_text().contains("Connected to builder"));
    // A text readiness banner alone cannot replace a healthy remote session.
    client.key(b"\x1b[B\x1b[B\r");
    client.wait_screen("dropped bridge after readiness", |frame| {
        frame.contains("Attention: SSH bridge closed") && !frame.contains("Permission denied")
    });
    assert!(client.screen.dump_text().contains("Connected to builder"));
    client.key(b"l");
    client.wait_for(&original_marker);

    // Cancel a pending handshake. Input typed during it belongs to neither shell.
    open_servers(&local_socket);
    client.wait_for("Connected to Local");
    client.key(b"\x1b[B\x1b[B\r");
    client.wait_for("Connecting to slow");
    client.key(b"discard-me\x1b");
    client.wait_for("cancelled");
    client.key(b"\x1b");
    client.wait_screen("local Workspace after dismissal", |frame| {
        frame.contains("LOCAL-SHELL-")
            && !frame.contains("Connected to Local")
            && !frame.contains("Connecting to")
    });
    client.key(b"AFTER-RETURN\r");
    client.wait_for("AFTER-RETURN");
    assert!(!client.screen.dump_text().contains("discard-me"));
    assert_eq!(local_pane(&local_socket).id, original.id);
    assert!(uniterm_client::query_info(&remote_socket).is_ok());
    for (sequence, count) in [(b"\x1b[?1049h".as_slice(), 1), (b"\x1b[?1049l", 0)] {
        assert_eq!(
            client
                .transcript
                .windows(sequence.len())
                .filter(|w| *w == sequence)
                .count(),
            count
        );
    }
}
