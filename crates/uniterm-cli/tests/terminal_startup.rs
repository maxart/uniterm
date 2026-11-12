//! Exercise the real attach binary through a PTY with a poisoned outer screen.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixListener;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use uniterm_proto::{encode_frame, ClientMessage, FrameDecoder, ServerMessage};
use uniterm_server::Terminal;

mod common;

#[test]
fn attach_repairs_inherited_terminal_state_and_preserves_the_shell_screen() {
    let state = common::isolate_state();
    let runtime = common::socket_root().join(format!("ut-tty-{}", std::process::id()));
    std::fs::create_dir_all(&runtime).unwrap();
    std::env::set_var("XDG_RUNTIME_DIR", &runtime);
    let workspace = common::unique_workspace_name();
    let socket = uniterm_server::server::default_socket_path(&workspace);
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let listener = UnixListener::bind(&socket).unwrap();
    let (rendered_tx, rendered_rx) = std::sync::mpsc::channel();
    // A protocol fixture makes the first render deterministic, with no shell
    // startup repaint that could accidentally repair the outer terminal.
    let fixture = thread::spawn(move || {
        for _ in 0..4 {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut decoder = FrameDecoder::new();
            let mut buffer = [0; 4096];
            loop {
                let n = stream.read(&mut buffer).unwrap();
                assert_ne!(n, 0);
                decoder.push(&buffer[..n]);
                if let Some(message) = decoder.decode::<ClientMessage>().unwrap() {
                    assert!(matches!(message, ClientMessage::Attach { .. }));
                    break;
                }
            }
            stream
                .write_all(&encode_frame(&ServerMessage::RenderOps(
                    b"\x1b[1;1Huniterm qxlkm 123\x1b[24;1Hstatus qxlkm".to_vec(),
                )))
                .unwrap();
            rendered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            stream
                .write_all(&encode_frame(&ServerMessage::Detached))
                .unwrap();
        }
    });

    // The empty case covers a healthy terminal; repeated connections cover
    // reattach. The exact reported reproducer must fail before the fix.
    for poison in [
        &b""[..],
        &b"\x1b(0"[..],
        &b"\x1b(0\x1b[3;20r\x1b[?6h\x1b[4h\x1b[31;7m"[..],
        &b"\x1b(0"[..],
    ] {
        let mut master = 0;
        let mut slave = 0;
        let mut size = libc::winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: openpty initializes both descriptors; the winsize is live.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &raw mut size,
                )
            },
            0
        );
        // SAFETY: each newly opened descriptor gets exactly one File owner.
        let (mut master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
        // SAFETY: change only the nonblocking flag on our owned PTY master.
        unsafe {
            let flags = libc::fcntl(master.as_raw_fd(), libc::F_GETFL);
            assert_eq!(
                libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK),
                0
            );
        }
        let mut child = Command::new(env!("CARGO_BIN_EXE_ut"))
            .args(["attach", &workspace])
            .env("XDG_STATE_HOME", &state)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("XDG_CONFIG_HOME", &runtime)
            .env("TERM", "xterm-256color")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave.try_clone().unwrap()))
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut output = Vec::new();
        loop {
            let mut buffer = [0; 4096];
            match master.read(&mut buffer) {
                Ok(n) => output.extend_from_slice(&buffer[..n]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                other => panic!("PTY read failed: {other:?}"),
            }
            if output.windows(12).any(|w| w == b"status qxlkm") {
                break;
            }
            assert!(Instant::now() < deadline, "first frame never arrived");
            thread::sleep(Duration::from_millis(10));
        }
        let first_frame_end = output.len();
        rendered_tx.send(()).unwrap();
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("attach did not finish");
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(child.wait().unwrap().success());
        // Keep our slave open until the final bytes are drained: macOS can
        // discard unread output when the child closes the last slave handle.
        // The nonblocking master reports WouldBlock once the queue is empty.
        if let Err(error) = master.read_to_end(&mut output) {
            assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        }
        drop(slave);
        let mut screen = Terminal::new(80, 24);
        screen.feed(b"shell history\r\n");
        screen.feed(poison);
        screen.feed(&output[..first_frame_end]);
        let first: String = (0..17).map(|x| screen.grid().get(x, 0).ch).collect();
        let status: String = (0..12).map(|x| screen.grid().get(x, 23).ch).collect();
        assert_eq!(first, "uniterm qxlkm 123", "poison: {poison:?}");
        assert_eq!(status, "status qxlkm", "poison: {poison:?}");
        assert_eq!(screen.grid().get(0, 0).fg, uniterm_core::Color::DEFAULT);
        assert_eq!(screen.grid().get(0, 0).attrs, uniterm_core::Attrs::NONE);
        screen.feed(&output[first_frame_end..]);
        assert!(
            screen.dump_text().contains("shell history"),
            "poison: {poison:?}, output: {output:?}"
        );
        screen.feed(b"\x1b[2;1Hshell qxlkm");
        assert!(screen.dump_text().contains("shell qxlkm"));
        assert!(
            !output.windows(2).any(|w| w == b"\x1bc"),
            "hard reset loses shell history"
        );
    }
    fixture.join().unwrap();
    std::fs::remove_dir_all(runtime).unwrap();
}
