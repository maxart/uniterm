//! Keyboard ownership between the Files sidebar and terminal Panes.
//!
//! Files owns typing while it is focused, so ordinary navigation keys never
//! reach a shell. Any explicit Pane or Tab focus command, and Escape from
//! plain Files navigation, must hand typing back to the active Pane while the
//! sidebar stays open. Escape inside a Files prompt only cancels the prompt.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use uniterm_proto::{
    encode_frame, ClientMessage, Command, ControlCommand, ControlRequest, FocusDir, FrameDecoder,
    ServerMessage, SplitAxis, CONTROL_API_VERSION,
};
use uniterm_server::Server;

mod common;

use common::{isolate_state, unique_workspace_name};

struct Harness {
    client: UnixStream,
    decoder: FrameDecoder,
    workspace: String,
    control: UnixStream,
    reader: BufReader<UnixStream>,
    next_id: u64,
    files_shown: bool,
}

impl Harness {
    fn start() -> Self {
        isolate_state();
        let base =
            common::socket_root().join(format!("uniterm-files-focus-{}", std::process::id()));
        let root = base.join("Project");
        std::fs::create_dir_all(&root).unwrap();
        for name in ["alpha.txt", "beta.txt"] {
            std::fs::write(root.join(name), "x\n").unwrap();
        }
        let workspace = unique_workspace_name();
        let socket = base.join(format!("{workspace}.sock"));
        let server_socket = socket.clone();
        thread::spawn(move || {
            let (mut server, mut poll) =
                Server::bind(&server_socket, "/bin/sh", &[], 120, 20).unwrap();
            let _ = server.run(&mut poll);
        });
        let control_path = socket.with_extension("control.sock");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !control_path.exists() {
            assert!(Instant::now() < deadline, "server never started");
            thread::sleep(Duration::from_millis(10));
        }
        let client = UnixStream::connect(&socket).unwrap();
        client
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let control = UnixStream::connect(&control_path).unwrap();
        control
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut harness = Harness {
            client,
            decoder: FrameDecoder::new(),
            workspace,
            reader: BufReader::new(control.try_clone().unwrap()),
            control,
            next_id: 1,
            files_shown: false,
        };
        harness.send(ClientMessage::Attach {
            term: "xterm-256color".into(),
            cols: 120,
            rows: 20,
        });
        harness.send(ClientMessage::ProjectCreate {
            name: "FocusProject".into(),
            root: root.to_string_lossy().into_owned(),
        });
        harness
    }

    fn send(&mut self, message: ClientMessage) {
        self.client.write_all(&encode_frame(&message)).unwrap();
        // Drain render output so the server never blocks on this client.
        let mut buffer = [0u8; 65_536];
        while let Ok(size) = self.client.read(&mut buffer) {
            if size == 0 {
                break;
            }
            self.decoder.push(&buffer[..size]);
            while let Ok(Some(_)) = self.decoder.decode::<ServerMessage>() {}
        }
    }

    fn type_bytes(&mut self, bytes: &[u8]) {
        self.send(ClientMessage::Input(bytes.to_vec()));
    }

    fn command(&mut self, command: Command) {
        self.send(ClientMessage::Command(command));
    }

    fn request(&mut self, command: ControlCommand) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let request = ControlRequest {
            version: CONTROL_API_VERSION,
            id,
            workspace: self.workspace.clone(),
            command,
        };
        serde_json::to_writer(&mut self.control, &request).unwrap();
        self.control.write_all(b"\n").unwrap();
        loop {
            let mut line = String::new();
            self.reader.read_line(&mut line).unwrap();
            let frame: Value = serde_json::from_str(&line).unwrap();
            if frame["frame"] == "response" && frame["id"] == id {
                return frame["result"]["data"].clone();
            }
        }
    }

    /// Every Pane's recent output, concatenated.
    fn output(&mut self) -> String {
        let panes = self.request(ControlCommand::PaneList)["panes"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let mut text = String::new();
        for pane in panes {
            let id = pane["id"].as_u64().unwrap();
            let read = self.request(ControlCommand::PaneRead {
                pane: uniterm_core::PaneId(id),
                lines: 200,
            });
            text.push_str(read["text"].as_str().unwrap_or_default());
        }
        text
    }

    /// Type a shell line whose output only exists if the shell ran it, then
    /// wait for that output (`echo MARK_$((40+2))` prints `MARK_42`).
    fn shell_runs(&mut self, mark: &str) {
        self.type_bytes(format!("echo {mark}_$((40+2))\r").as_bytes());
        let expected = format!("{mark}_42");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let output = self.output();
            if output.lines().any(|line| line.trim() == expected) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "typing did not reach the terminal for {mark}:\n{output}"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Give the Files sidebar keyboard focus (opening it the first time;
    /// later visits scroll it, which focuses without toggling it closed),
    /// then press a navigation key that must stay inside Files.
    fn go_to_files(&mut self) {
        if self.files_shown {
            self.send(ClientMessage::Mouse {
                x: 118,
                y: 10,
                kind: uniterm_proto::MouseKind::WheelDown,
            });
        } else {
            self.command(Command::FileSidebarToggle);
            self.files_shown = true;
        }
        self.type_bytes(b"j");
    }

    fn files_open(&mut self) -> bool {
        self.request(ControlCommand::Capabilities);
        // The sidebar state is chrome; read it from a fresh full frame.
        self.client
            .write_all(&encode_frame(&ClientMessage::Refresh))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut buffer = [0u8; 65_536];
        while Instant::now() < deadline {
            if let Ok(size) = self.client.read(&mut buffer) {
                self.decoder.push(&buffer[..size]);
            }
            while let Ok(Some(message)) = self.decoder.decode::<ServerMessage>() {
                if let ServerMessage::RenderOps(ops) = message {
                    let frame = String::from_utf8_lossy(&ops).into_owned();
                    if frame.contains("\x1b[2J") || frame.contains("\x1b[H\x1b[J") {
                        return frame.contains("FILES");
                    }
                }
            }
        }
        panic!("no full frame arrived");
    }
}

#[test]
fn explicit_pane_and_tab_focus_return_typing_to_the_terminal() {
    let mut harness = Harness::start();
    harness.shell_runs("READY");

    harness.go_to_files();
    harness.command(Command::Focus(FocusDir::Left));
    harness.shell_runs("FOCUS");

    harness.go_to_files();
    harness.command(Command::Split(SplitAxis::LeftRight));
    harness.shell_runs("SPLIT");

    harness.go_to_files();
    harness.command(Command::LastPane);
    harness.shell_runs("LAST");

    harness.go_to_files();
    harness.command(Command::NewWindow);
    harness.shell_runs("TAB");

    harness.go_to_files();
    harness.command(Command::PrevWindow);
    harness.shell_runs("PREV");

    // No Files key ever reached a shell.
    assert!(!harness.output().contains("jecho"));
}

#[test]
fn escape_leaves_files_navigation_but_keeps_the_sidebar_open() {
    let mut harness = Harness::start();
    harness.shell_runs("READY");

    harness.go_to_files();
    harness.type_bytes(b"\x1b");
    harness.shell_runs("ESCAPE");
    assert!(harness.files_open(), "Escape closed the sidebar");

    // Escape inside a prompt cancels the prompt only; Files keeps typing,
    // so the following `q` is a Files key (leave navigation), not shell text.
    harness.go_to_files();
    harness.type_bytes(b"n");
    harness.type_bytes(b"\x1b");
    harness.type_bytes(b"q");
    harness.shell_runs("PROMPT");
    let output = harness.output();
    assert!(!output.contains("qecho"), "{output}");
    assert!(!output.contains("jecho"), "{output}");
}
