//! Keyboard negotiation crosses the client protocol without losing modifiers.

use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};
use uniterm_core::PaneId;
use uniterm_proto::{encode_frame, ClientMessage, FrameDecoder, ServerMessage};
use uniterm_server::Server;

mod common;

struct Wire {
    stream: UnixStream,
    decoder: FrameDecoder,
}

impl Wire {
    fn send(&mut self, message: ClientMessage) {
        self.stream.write_all(&encode_frame(&message)).unwrap();
    }

    fn output(&mut self) -> String {
        self.send(ClientMessage::PaneRead {
            pane: PaneId(1),
            lines: 100,
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            while let Some(message) = self.decoder.decode::<ServerMessage>().unwrap() {
                if let ServerMessage::PaneOutput { text, .. } = message {
                    return text;
                }
            }
            assert!(Instant::now() < deadline, "server response timed out");
            let mut buffer = [0; 8192];
            let count = self.stream.read(&mut buffer).unwrap();
            assert!(count > 0);
            self.decoder.push(&buffer[..count]);
        }
    }

    fn wait_text(&mut self, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let output = self.output();
            // BSD and GNU od use different spacing and line wrapping for
            // the same bytes. Match the byte tokens, not that presentation.
            let normalized = output.split_whitespace().collect::<Vec<_>>().join(" ");
            if normalized.contains(text) {
                return;
            }
            assert!(Instant::now() < deadline, "missing {text:?}: {output}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[test]
fn shift_enter_reaches_opted_in_tui_and_shell_controls_survive_pop() {
    common::isolate_state();
    let directory = common::socket_root().join(format!("ut-key-{}", common::unique_nonce()));
    std::fs::create_dir_all(&directory).unwrap();
    let socket = directory.join(format!("{}.sock", common::unique_workspace_name()));
    let server_socket = socket.clone();
    let server = std::thread::spawn(move || {
        let (mut server, mut poll) = Server::bind(&server_socket, "/bin/sh", &["-c",
            r"stty raw -echo; printf '\033[>1uREADY'; dd bs=1 count=8 2>/dev/null | od -An -tx1; printf '\033[<u\r\nLEGACY'; dd bs=1 count=2 2>/dev/null | od -An -tx1; printf '\r\nPASTE'; dd bs=1 count=21 2>/dev/null | od -An -tx1; printf '\r\nDONE'; read hold"
        ], 100, 24).unwrap();
        server.run(&mut poll).unwrap();
    });
    common::wait_for_socket(&socket);
    let stream = UnixStream::connect(&socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut wire = Wire {
        stream,
        decoder: FrameDecoder::new(),
    };
    wire.send(ClientMessage::Resize {
        cols: 100,
        rows: 24,
    });
    wire.wait_text("READY");
    // Shift+Enter remains CSI-u, while ordinary Enter is still CR.
    wire.send(ClientMessage::Input(b"\x1b[13;2u\r".to_vec()));
    wire.wait_text("1b 5b 31 33 3b 32 75 0d");
    wire.wait_text("LEGACY");
    wire.send(ClientMessage::Input(b"\x1b[99;5u\r".to_vec()));
    wire.wait_text("03 0d");
    wire.wait_text("PASTE");
    // Delimiters and literal CSI-u content can cross independent Input frames.
    wire.send(ClientMessage::Input(b"\x1b[20".to_vec()));
    wire.send(ClientMessage::Input(b"0~\x1b[13;2u\x01d\x1b[201~".to_vec()));
    wire.wait_text("DONE");
    let output = wire.output().replace(['\n', '\r', ' '], "");
    assert!(
        output.contains("1b5b3230307e1b5b31333b327501641b5b3230317e"),
        "{output}"
    );
    wire.send(ClientMessage::KillServer);
    server.join().unwrap();
    let _ = std::fs::remove_dir_all(directory);
}
