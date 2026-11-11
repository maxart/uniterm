//! A burst of client resizes settles with one relayout, not one per size.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant};

use uniterm_proto::{encode_frame, ClientMessage, FrameDecoder, ServerMessage};
use uniterm_server::Server;

mod common;

use common::{isolate_state, temp_dir, unique_workspace_name, wait_for_socket};

fn attach(path: &std::path::Path, cols: u16, rows: u16) -> UnixStream {
    let mut stream = UnixStream::connect(path).unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::Attach {
            term: "xterm-256color".into(),
            cols,
            rows,
        }))
        .unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    stream
}

/// Render frames received within `for_time`, as text.
fn frames(stream: &mut UnixStream, for_time: Duration) -> Vec<String> {
    let mut decoder = FrameDecoder::new();
    let mut output = Vec::new();
    let mut buf = [0; 64 * 1024];
    let deadline = Instant::now() + for_time;
    while Instant::now() < deadline {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                decoder.push(&buf[..n]);
                while let Ok(Some(message)) = decoder.decode::<ServerMessage>() {
                    if let ServerMessage::RenderOps(ops) = message {
                        output.push(String::from_utf8_lossy(&ops).into_owned());
                    }
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => break,
        }
    }
    output
}

fn is_full_frame(frame: &str) -> bool {
    frame.contains("\x1b[r\x1b[2J")
}

/// The largest row a frame addresses with `CUP row;1`, which for a full
/// repaint is the bottom row of the geometry it was painted for.
fn max_row_addressed(frame: &str) -> u16 {
    frame
        .split("\x1b[")
        .filter_map(|part| {
            let (row, rest) = part.split_once(';')?;
            rest.starts_with("1H").then(|| row.parse::<u16>().ok())?
        })
        .max()
        .unwrap_or(0)
}

#[test]
fn a_resize_burst_in_one_batch_relayouts_once_at_the_final_size() {
    isolate_state();
    let dir = temp_dir("resize-storm");
    let socket = dir.join(format!("{}.sock", unique_workspace_name()));
    let server_socket = socket.clone();
    let server = thread::spawn(move || {
        let (mut server, mut poll) =
            Server::bind(&server_socket, "/bin/sh", &["-c", "sleep 3"], 80, 24).unwrap();
        server.run(&mut poll).unwrap();
    });
    wait_for_socket(&socket);

    let mut client = attach(&socket, 80, 24);
    let initial = frames(&mut client, Duration::from_millis(300));
    assert!(
        initial.iter().any(|frame| frame.contains("\x1b[24;1H")),
        "no initial frame at 24 rows"
    );

    // Twenty distinct intermediate geometries, none equal to the final one,
    // delivered in a single write so the server decodes them as one batch.
    let mut burst = Vec::new();
    for step in 1..=20u16 {
        burst.extend(encode_frame(&ClientMessage::Resize {
            cols: 80 + step,
            rows: 24 + step,
        }));
    }
    burst.extend(encode_frame(&ClientMessage::Resize {
        cols: 120,
        rows: 60,
    }));
    client.write_all(&burst).unwrap();

    let settled = frames(&mut client, Duration::from_millis(500));
    let full: Vec<&String> = settled
        .iter()
        .filter(|frame| is_full_frame(frame))
        .collect();
    assert!(
        !full.is_empty() && full.last().unwrap().contains("\x1b[60;1H"),
        "the final size was lost: {} full frames, last = {:?}",
        full.len(),
        full.last()
    );
    assert!(
        full.len() <= 2,
        "a coalesced batch must not repaint per intermediate size, got {} full frames",
        full.len()
    );
    for frame in &full {
        assert_eq!(
            max_row_addressed(frame),
            60,
            "a full frame was painted at an intermediate size"
        );
    }

    client
        .write_all(&encode_frame(&ClientMessage::Detach))
        .unwrap();
    drop(client);
    server.join().unwrap();
}
