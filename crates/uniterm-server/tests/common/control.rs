//! A control-API and attach client plus scripted-agent helpers for the
//! integration tests that drive a live Workspace. Kept out of `mod.rs`,
//! which the library's own unit tests include, so it may use the crate.

#![allow(dead_code)]

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use uniterm_proto::{ControlCommand, ControlRequest, CONTROL_API_VERSION};
use uniterm_server::Server;

use super::common::{isolate_config, isolate_state, socket_root, unique_workspace_name};

/// Poll `check` every 20 ms until it holds, failing after `timeout`.
pub fn wait_for(what: &str, timeout: Duration, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(20));
    }
}

/// Assert `check` holds throughout `window`.
pub fn holds_for(what: &str, window: Duration, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + window;
    while Instant::now() < deadline {
        assert!(check(), "{what}");
        thread::sleep(Duration::from_millis(50));
    }
}

pub const SETTLE: Duration = Duration::from_millis(800);
pub const TIMEOUT: Duration = Duration::from_secs(15);

pub struct Control {
    pub workspace: String,
    pub writer: UnixStream,
    pub reader: BufReader<UnixStream>,
    pub next_id: u64,
}

impl Control {
    pub fn connect(socket: &Path) -> Self {
        let control = socket.with_extension("control.sock");
        // The socket file appears at bind, a moment before the server listens.
        let mut stream = None;
        wait_for("the control socket", Duration::from_secs(10), || {
            stream = UnixStream::connect(&control).ok();
            stream.is_some()
        });
        let writer = stream.unwrap();
        writer
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        Control {
            workspace: socket.file_stem().unwrap().to_str().unwrap().into(),
            reader: BufReader::new(writer.try_clone().unwrap()),
            writer,
            next_id: 1,
        }
    }

    pub fn send(&mut self, id: u64, command: ControlCommand) {
        let request = ControlRequest {
            version: CONTROL_API_VERSION,
            id,
            workspace: self.workspace.clone(),
            command,
        };
        serde_json::to_writer(&mut self.writer, &request).unwrap();
        self.writer.write_all(b"\n").unwrap();
    }

    pub fn next_frame(&mut self) -> Value {
        let mut line = String::new();
        self.reader.read_line(&mut line).unwrap();
        assert!(!line.is_empty(), "control connection closed");
        serde_json::from_str(&line).unwrap()
    }

    /// One request, returning the raw response frame so error codes are
    /// observable. Event frames are skipped.
    pub fn request_raw(&mut self, command: ControlCommand) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(id, command);
        loop {
            let frame = self.next_frame();
            if frame["frame"] == "response" && frame["id"] == id {
                return frame;
            }
        }
    }

    pub fn request(&mut self, command: ControlCommand) -> Value {
        let frame = self.request_raw(command);
        assert!(frame["error"].is_null(), "control error: {frame}");
        frame["result"]["data"].clone()
    }

    pub fn agent(&mut self) -> Value {
        self.request(ControlCommand::AgentList)["entries"][0].clone()
    }

    pub fn list(&mut self, command: ControlCommand, key: &str) -> Vec<Value> {
        self.request(command)[key]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    pub fn waiting(&mut self) -> Vec<Value> {
        self.list(ControlCommand::WaitingList, "items")
    }

    pub fn delegation_status(&mut self) -> Vec<Value> {
        self.list(ControlCommand::DelegationStatus { pane: None }, "entries")
    }

    pub fn arm(&mut self, pane: u64) {
        let frame = self.request_raw(ControlCommand::DelegationArm {
            pane: uniterm_core::PaneId(pane),
            confirmed: true,
        });
        assert!(frame["error"].is_null(), "arm failed: {frame}");
    }

    pub fn wait_armed(&mut self) {
        wait_for("an arm", TIMEOUT, || !self.delegation_status().is_empty());
    }

    pub fn type_line(&mut self, pane: u64, text: &str) {
        self.request(ControlCommand::PaneSend {
            pane: uniterm_core::PaneId(pane),
            text: format!("{text}\r"),
        });
    }

    pub fn read(&mut self, pane: u64) -> String {
        self.request(ControlCommand::PaneRead {
            pane: uniterm_core::PaneId(pane),
            lines: 200,
        })["text"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    /// Poll the fleet until `check` holds for the first agent entry.
    pub fn until(&mut self, what: &str, check: impl Fn(&Value) -> bool) -> Value {
        let mut agent = Value::Null;
        wait_for(what, TIMEOUT, || {
            agent = self.agent();
            check(&agent)
        });
        agent
    }

    pub fn until_bound(&mut self, session: &str) {
        self.until(session, |agent| agent["details"]["session_id"] == session);
    }

    pub fn until_output(&mut self, pane: u64, needle: &str) {
        wait_for(needle, TIMEOUT, || self.read(pane).contains(needle));
    }

    /// The moment `needle` first shows on the pane.
    pub fn seen(&mut self, pane: u64, needle: &str) -> Instant {
        self.until_output(pane, needle);
        Instant::now()
    }

    pub fn assert_output_stays_absent(&mut self, pane: u64, needle: &str) {
        holds_for(&format!("pane showed {needle:?}"), SETTLE, || {
            !self.read(pane).contains(needle)
        });
    }
}

/// Write the envelope `provider`'s connector would print for one hook.
pub fn hook_file(dir: &Path, name: &str, provider: &str, hook: Value) -> PathBuf {
    let envelope = uniterm_server::connectors::hook_envelope(provider, hook.to_string().as_bytes())
        .unwrap_or_else(|| panic!("connector rejected {hook}"));
    let path = dir.join(name);
    std::fs::write(&path, envelope).unwrap();
    path
}

pub fn session_start(session: &str) -> Value {
    json!({
        "hook_event_name": "SessionStart",
        "source": "startup",
        "session_id": session,
        "cwd": "/tmp/demo",
    })
}

pub fn plain_file(dir: &Path, name: &str, event: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(
        &path,
        format!("\x1b]777;notify;uniterm://cli-agent;{{\"agent\":\"claude\",\"event\":\"{event}\"}}\x07"),
    )
    .unwrap();
    path
}

pub fn cat(paths: &[&PathBuf]) -> String {
    paths
        .iter()
        .map(|path| format!("cat '{}'", path.display()))
        .collect::<Vec<_>>()
        .join("; ")
}

/// `read` one line per label and echo it back as `LABEL:<line>`, on the pane
/// or appended to `out` (screen dialogs must stay byte-identical).
pub fn echo_reads(labels: &[&str], out: Option<&Path>) -> String {
    let redirect = out.map_or(String::new(), |out| format!(" >> '{}'", out.display()));
    labels
        .iter()
        .map(|label| format!("read v; echo \"{label}:$v\"{redirect}"))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Every event appended so far, read back through a cursored subscription.
pub fn events(socket: &Path) -> Vec<Value> {
    let mut control = Control::connect(socket);
    let current = control.request(ControlCommand::AgentList)["sequence"]
        .as_u64()
        .unwrap();
    control.send(99, ControlCommand::Subscribe { after_sequence: 0 });
    let mut out = Vec::new();
    loop {
        let frame = control.next_frame();
        if frame["frame"] == "event" {
            out.push(frame["event"].clone());
            if frame["sequence"].as_u64().unwrap() >= current {
                return out;
            }
        }
    }
}

pub fn count(log: &[Value], event: &str) -> usize {
    log.iter().filter(|e| e.get(event).is_some()).count()
}

pub fn position(events: &[Value], from: usize, test: impl Fn(&Value) -> bool) -> usize {
    events[from..]
        .iter()
        .position(test)
        .map(|index| index + from)
        .unwrap_or_else(|| panic!("event not found after {from}"))
}

pub fn wait_until_file_contains(path: &Path, needle: &str) -> String {
    let mut content = String::new();
    wait_for(needle, Duration::from_secs(30), || {
        content = std::fs::read_to_string(path).unwrap_or_default();
        content.contains(needle)
    });
    content
}

pub fn assert_file_stays_absent(path: &Path, needle: &str, window: Duration) {
    holds_for(&format!("answers file showed {needle:?}"), window, || {
        !std::fs::read_to_string(path)
            .unwrap_or_default()
            .contains(needle)
    });
}

/// Bind a server with `config` and return its socket, a control connection
/// and the first Pane.
pub fn start_server(
    config: uniterm_core::Config,
) -> (PathBuf, Control, u64, thread::JoinHandle<()>) {
    isolate_state();
    isolate_config();
    let dir = socket_root().join(format!("uniterm-control-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join(format!("{}.sock", unique_workspace_name()));
    let server_socket = socket.clone();
    let thread = thread::spawn(move || {
        let (mut server, mut poll) = Server::bind(&server_socket, "/bin/sh", &[], 120, 30).unwrap();
        server.set_config(config);
        let _ = server.run(&mut poll);
    });
    let mut control = Control::connect(&socket);
    let pane = control.request(ControlCommand::PaneList)["panes"][0]["id"]
        .as_u64()
        .unwrap();
    (socket, control, pane, thread)
}

/// An attach connection, the live path the client Settings surface uses.
pub struct Attach {
    pub stream: UnixStream,
    pub decoder: uniterm_proto::FrameDecoder,
}

impl Attach {
    pub fn open(socket: &Path) -> Self {
        let mut attach = Attach {
            stream: UnixStream::connect(socket).unwrap(),
            decoder: uniterm_proto::FrameDecoder::new(),
        };
        attach
            .stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        attach.send(&uniterm_proto::ClientMessage::Attach {
            term: "xterm-256color".into(),
            cols: 80,
            rows: 24,
        });
        attach
    }

    pub fn send(&mut self, message: &uniterm_proto::ClientMessage) {
        self.stream
            .write_all(&uniterm_proto::encode_frame(message))
            .unwrap();
    }

    /// The next Settings message: the snapshot, whether it acknowledges a
    /// save, and any error.
    pub fn next_settings(&mut self) -> (uniterm_proto::SettingsSnapshot, bool, Option<String>) {
        let mut buf = [0u8; 65536];
        loop {
            while let Ok(Some(message)) = self.decoder.decode::<uniterm_proto::ServerMessage>() {
                if let uniterm_proto::ServerMessage::Settings {
                    settings,
                    saved,
                    error,
                } = message
                {
                    return (*settings, saved, error);
                }
            }
            let n = std::io::Read::read(&mut self.stream, &mut buf).unwrap();
            assert!(n > 0, "server closed the attach before a Settings reply");
            self.decoder.push(&buf[..n]);
        }
    }

    /// Apply one patch and return the snapshot once the save round trips;
    /// the attach snapshot and focus pushes before it are skipped.
    pub fn apply(
        &mut self,
        patch: uniterm_proto::SettingsPatch,
    ) -> uniterm_proto::SettingsSnapshot {
        self.send(&uniterm_proto::ClientMessage::SettingsApply(patch));
        loop {
            let (settings, saved, error) = self.next_settings();
            assert!(error.is_none(), "settings apply failed: {error:?}");
            if saved {
                return settings;
            }
        }
    }
}

pub fn settings_apply(
    socket: &Path,
    patch: uniterm_proto::SettingsPatch,
) -> uniterm_proto::SettingsSnapshot {
    Attach::open(socket).apply(patch)
}
