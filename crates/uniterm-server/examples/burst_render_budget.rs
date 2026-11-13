//! Count real server burst frames with default chrome, retained history and a PTY shell.
//! Run in release mode in a quiet window; this is not the external comparison harness.
//! Five indexed 10,000-line bursts must each fit in 128 KiB of client output,
//! retain their complete visible tail, and stop producing bytes once settled.
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant};
use uniterm_proto::{encode_frame, ClientMessage, FrameDecoder, ServerMessage};
use uniterm_server::{Server, Terminal};

#[path = "../tests/common/mod.rs"]
mod common;

struct Observer {
    stream: UnixStream,
    decoder: FrameDecoder,
    frames: Vec<Vec<u8>>,
    screen: Terminal,
}

impl Observer {
    fn send(&mut self, message: ClientMessage) {
        self.stream.write_all(&encode_frame(&message)).unwrap();
    }

    fn read(&mut self) -> bool {
        let mut buf = [0; 65536];
        match self.stream.read(&mut buf) {
            Ok(0) => panic!("server closed early"),
            Ok(n) => {
                self.decoder.push(&buf[..n]);
                while let Some(message) = self.decoder.decode::<ServerMessage>().unwrap() {
                    if let ServerMessage::RenderOps(ops) = message {
                        self.frames.push(ops);
                    }
                }
                true
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                false
            }
            Err(e) => panic!("read: {e}"),
        }
    }

    fn until(&mut self, marker: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut checked = 0;
        loop {
            self.read();
            // Uniform ASCII markers remain contiguous in a row's render run.
            if self.frames[checked..]
                .iter()
                .any(|ops| ops.windows(marker.len()).any(|w| w == marker.as_bytes()))
            {
                return;
            }
            checked = self.frames.len();
            assert!(Instant::now() < deadline, "missing {marker}");
        }
    }

    fn settle(&mut self) {
        while self.read() {}
    }

    fn apply(&mut self) {
        for ops in &self.frames {
            self.screen.feed(ops);
        }
    }
}

fn main() {
    common::isolate_state();
    let dir = common::temp_dir("burst-render");
    std::env::set_var("XDG_RUNTIME_DIR", &dir);
    let socket = dir.join(format!("{}.sock", common::unique_workspace_name()));
    let server_socket = socket.clone();
    let server = thread::spawn(move || {
        let (mut server, mut poll) = Server::bind(
            &server_socket,
            "/bin/sh",
            &["-c", "stty -echo; printf 'BURST_READY\\n'; exec /bin/sh"],
            160,
            50,
        )
        .unwrap();
        server.run(&mut poll).unwrap();
    });
    common::wait_for_socket(&socket);
    let stream = UnixStream::connect(&socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let mut observer = Observer {
        stream,
        decoder: FrameDecoder::new(),
        frames: Vec::new(),
        screen: Terminal::new(160, 50),
    };
    observer.send(ClientMessage::Attach {
        term: "xterm-256color".into(),
        cols: 160,
        rows: 50,
    });
    observer.until("BURST_READY");
    observer.settle();
    observer.apply();
    observer.frames.clear();
    let slow = std::env::args().any(|arg| arg == "--slow");
    for iteration in 0..5 {
        let payload: String = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ"
            .iter()
            .cycle()
            .skip(iteration)
            .take(62)
            .map(|b| char::from(*b))
            .collect();
        let marker = format!("BURST_END_{iteration:02}");
        let command = format!("i=0; while [ \"$i\" -lt 10000 ]; do printf '%08d {payload}\\n' \"$i\"; i=$((i+1)); done; printf '{marker}\\n'\n");
        let start = Instant::now();
        observer.send(ClientMessage::Input(command.into_bytes()));
        if slow {
            thread::sleep(Duration::from_millis(100));
        }
        observer.until(&marker);
        let elapsed = start.elapsed();
        observer.settle();
        let bytes: usize = observer.frames.iter().map(Vec::len).sum();
        let clears: usize = observer
            .frames
            .iter()
            .map(|ops| ops.windows(4).filter(|w| *w == b"\x1b[2J").count())
            .sum();
        observer.apply();
        let screen = observer.screen.dump_text();
        let mut previous = 0;
        for line in 9990..10000 {
            let expected = format!("{line:08} {payload}");
            let position = screen.find(&expected).expect("intact ordered burst tail");
            assert!(position > previous);
            previous = position;
        }
        assert!(screen.find(&marker).unwrap() > previous);
        println!(
            "burst {iteration}: {:.3} ms, {} frames, {bytes} bytes, {clears} full clears",
            elapsed.as_secs_f64() * 1000.0,
            observer.frames.len()
        );
        assert!(
            bytes <= 128 * 1024,
            "burst output exceeded 128 KiB: {bytes} bytes"
        );
        observer.frames.clear();
        assert!(!observer.read(), "output continued after the settled burst");
    }
    observer.send(ClientMessage::KillServer);
    drop(observer);
    server.join().unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}
