//! Drive real nonblocking client output in tests that do not run the mio loop.
//!
//! A peer reaching WouldBlock does not mean the server finished its frame.
//! In particular, macOS Unix sockets can fill during the first full repaint.
//! Alternate reads with the production writable handler until both the queue
//! and any collapsed repaint have drained, then require a complete wire frame.

use super::*;
use std::os::unix::net::UnixStream as Peer;
use std::time::{Duration, Instant};

/// Make partial writes routine on every platform instead of depending on its
/// default socket capacity. This changes only this test attachment's socket.
pub(super) fn small_send_buffer(server: &Server, token: Token) {
    let size: libc::c_int = 2048;
    // SAFETY: the client owns this live descriptor; size has the exact type and
    // length required for SO_SNDBUF and remains alive for the synchronous call.
    let result = unsafe {
        libc::setsockopt(
            server.clients[&token].stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            (&size as *const libc::c_int).cast(),
            std::mem::size_of_val(&size) as libc::socklen_t,
        )
    };
    assert_eq!(result, 0, "SO_SNDBUF: {}", std::io::Error::last_os_error());
}

/// Drain an entire send transaction through actual socket reads and writable
/// handling. Never discard a partial decoder or inspect only remaining outbuf.
pub(super) fn drain(server: &mut Server, reg: &Registry, token: Token, peer: &mut Peer) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut bytes = Vec::new();
    let mut buf = [0; 65536];
    loop {
        loop {
            match peer.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => bytes.extend_from_slice(&buf[..n]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => panic!("client read: {error}"),
            }
        }
        let pending = server.clients.get(&token).is_some_and(|client| {
            assert!(!client.dead, "test attachment failed while draining");
            client.wants_write() || client.repaint_pending || client.final_frame.is_some()
        });
        if !pending {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "test attachment failed to drain pending output"
        );
        // Reading the peer frees socket capacity. This is the same callback
        // the real mio loop runs on its next WRITABLE event, including deferred
        // full-repaint delivery; calling Client::flush alone would omit it.
        server.on_client(reg, token, false, true);
    }
    let mut remaining = bytes.as_slice();
    while !remaining.is_empty() {
        assert!(
            remaining.len() >= 4,
            "partial frame header after transport drained"
        );
        let payload = u32::from_be_bytes(remaining[..4].try_into().unwrap()) as usize;
        assert!(
            payload <= MAX_SERVER_FRAME as usize,
            "invalid frame length {payload}"
        );
        let total = 4 + payload;
        assert!(
            remaining.len() >= total,
            "partial frame payload after transport drained"
        );
        remaining = &remaining[total..];
    }
    bytes
}

#[test]
fn partial_render_frames_drain_before_the_collapsed_repaint_and_then_stay_quiet() {
    use super::reaper::common;
    common::isolate_state();
    let name = common::unique_workspace_name();
    let root = common::socket_root().join(&name);
    std::fs::create_dir_all(&root).unwrap();
    let (mut server, mut poll) = Server::bind_internal(
        &root.join(format!("{name}.sock")),
        "/bin/sh",
        &[],
        80,
        24,
        false,
    )
    .unwrap();
    let mut peer = Peer::connect(&server.sock_path).unwrap();
    peer.set_nonblocking(true).unwrap();
    server.on_accept(poll.registry());
    let token = *server.clients.keys().next().unwrap();
    small_send_buffer(&server, token);
    drain(&mut server, poll.registry(), token, &mut peer);

    let original = vec![b'x'; 128 * 1024];
    let first = encode_frame(&ServerMessage::RenderOps(original.clone()));
    let client = server.clients.get_mut(&token).unwrap();
    client.attached = true;
    client.queue_render(&first);
    client.flush();
    assert!(client.out_offset > 0 && client.out_offset < first.len());
    assert!(client.render_end.is_some());
    assert!(
        client.wants_write(),
        "small SO_SNDBUF must exercise a partial frame"
    );

    server.full_repaint_client(poll.registry(), token);
    assert!(server.clients[&token].repaint_pending);
    let bytes = drain(&mut server, poll.registry(), token, &mut peer);
    let mut decoder = FrameDecoder::new();
    let mut renders = Vec::new();
    let mut saw_nested_input = false;
    for chunk in bytes.chunks(17) {
        decoder.push(chunk);
        while let Some(message) = decoder.decode::<ServerMessage>().unwrap() {
            match message {
                ServerMessage::RenderOps(ops) => renders.push(ops),
                ServerMessage::NestedInput { .. } => saw_nested_input = true,
                _ => {}
            }
        }
    }
    assert_eq!(
        renders.len(),
        2,
        "original frame then one authoritative repaint"
    );
    assert_eq!(
        renders[0], original,
        "partial writes must preserve the first frame"
    );
    assert_eq!(renders[1], server.build_full_frame());
    assert!(
        saw_nested_input,
        "non-render replies must survive the same drain"
    );
    assert!(server.clients[&token].outbuf.is_empty());
    assert!(server.clients[&token].render_end.is_none());
    assert!(!server.clients[&token].repaint_pending);
    assert!(drain(&mut server, poll.registry(), token, &mut peer).is_empty());
    server.retire_all_ptys(&mut poll);
}

#[test]
fn stalled_phone_releases_only_its_viewport_and_rejoins_after_drain() {
    use super::reaper::common;
    common::isolate_state();
    let name = common::unique_workspace_name();
    let root = common::temp_dir("phone-stall");
    let (mut server, mut poll) = Server::bind_internal(
        &root.join(format!("{name}.sock")),
        "/bin/sh",
        &["-c", "exec sleep 12"],
        164,
        30,
        false,
    )
    .unwrap();
    let reg = poll.registry();
    let connect = |server: &mut Server, cols, rows| {
        let mut peer = Peer::connect(&server.sock_path).unwrap();
        peer.set_nonblocking(true).unwrap();
        server.on_accept(reg);
        let token = Token(server.next_token - 1);
        server.handle_msg(
            reg,
            token,
            ClientMessage::Attach {
                term: "xterm".into(),
                cols,
                rows,
            },
            &mut false,
        );
        drain(server, reg, token, &mut peer);
        (token, peer)
    };
    let (desktop, mut desk_peer) = connect(&mut server, 164, 30);
    let (tablet, mut tablet_peer) = connect(&mut server, 110, 22);
    let (phone, mut phone_peer) = connect(&mut server, 40, 12);
    drain(&mut server, reg, desktop, &mut desk_peer);
    drain(&mut server, reg, tablet, &mut tablet_peer);
    assert_eq!(
        server.client_deadlines(&mut Vec::new()).1,
        None,
        "quiet clients never arm a timer"
    );

    small_send_buffer(&server, phone);
    let client = server.clients.get_mut(&phone).unwrap();
    client.queue_render(&encode_frame(&ServerMessage::RenderOps(vec![
        b'x';
        128 * 1024
    ])));
    client.flush();
    let due = server
        .client_deadlines(&mut Vec::new())
        .1
        .expect("kernel backpressure");
    server.full_repaint_client(reg, phone);
    assert!(server.clients[&phone].repaint_pending);
    server.expire_blocked_viewports(reg, due - Duration::from_millis(1));
    assert_eq!((server.cols, server.rows), (40, 12));
    assert_eq!(server.client_deadlines(&mut Vec::new()).1, Some(due));

    // No FIN, error, or Detach: leave the phone socket entirely open. Only
    // the client unable to receive real pending bytes loses its size vote.
    server.expire_blocked_viewports(reg, due);
    assert_eq!((server.cols, server.rows), (110, 22));
    drain(&mut server, reg, desktop, &mut desk_peer);
    drain(&mut server, reg, tablet, &mut tablet_peer);
    assert!(server.clients[&phone].attached && !server.clients[&phone].dead);
    assert_eq!(
        server.client_deadlines(&mut Vec::new()).1,
        None,
        "no repeated probing"
    );
    server.expire_blocked_viewports(reg, due + VIEWPORT_WRITE_TIMEOUT);
    assert!(drain(&mut server, reg, desktop, &mut desk_peer).is_empty());

    // Writable progress alone rejoins and restores the authoritative frame.
    drain(&mut server, reg, phone, &mut phone_peer);
    assert_eq!((server.cols, server.rows), (40, 12));
    drain(&mut server, reg, desktop, &mut desk_peer);
    drain(&mut server, reg, tablet, &mut tablet_peer);
    assert_eq!(server.client_deadlines(&mut Vec::new()).1, None);

    // A partly drained queue must get a fresh minute on actual write progress.
    let client = server.clients.get_mut(&phone).unwrap();
    client.queue_render(&encode_frame(&ServerMessage::RenderOps(vec![
        b'y';
        128 * 1024
    ])));
    client.flush();
    assert!(server.client_deadlines(&mut Vec::new()).1.is_some());
    let old_due = Instant::now() + VIEWPORT_WRITE_TIMEOUT / 2;
    server.clients.get_mut(&phone).unwrap().viewport_delivery = ViewportDelivery::Waiting(old_due);
    let mut chunk = [0; 65536];
    assert!(phone_peer.read(&mut chunk).unwrap() > 0);
    server.on_client(reg, phone, false, true);
    assert!(server.client_deadlines(&mut Vec::new()).1.unwrap() > old_due);
    server.expire_blocked_viewports(reg, old_due);
    assert_eq!((server.cols, server.rows), (40, 12));

    // Run the real poll loop with a short remaining wait. The desktop sends
    // nothing until it sees the wider canvas (or its own test timeout fires).
    server.clients.get_mut(&phone).unwrap().viewport_delivery =
        ViewportDelivery::Waiting(Instant::now() + Duration::from_millis(100));
    let observer = std::thread::spawn(move || {
        desk_peer.set_nonblocking(false).unwrap();
        desk_peer
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut decoder = FrameDecoder::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut restored = false;
        while !restored && Instant::now() < deadline {
            let Ok(n) = desk_peer.read(&mut chunk) else {
                break;
            };
            if n == 0 {
                break;
            }
            decoder.push(&chunk[..n]);
            while let Some(message) = decoder.decode::<ServerMessage>().unwrap() {
                if let ServerMessage::RenderOps(ops) = message {
                    restored |= ops.windows(b"\x1b[22;1H".len()).any(|s| s == b"\x1b[22;1H");
                }
            }
        }
        desk_peer
            .write_all(&encode_frame(&ClientMessage::KillServer))
            .unwrap();
        restored
    });
    server.run(&mut poll).unwrap();
    assert!(
        observer.join().unwrap(),
        "poll did not expire the stalled phone"
    );
}
