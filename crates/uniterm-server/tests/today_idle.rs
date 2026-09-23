//! Opt-in five-minute idle budget for the actual attached Today surface.
#[cfg(target_os = "linux")]
mod common;

#[test]
#[cfg(target_os = "linux")]
#[ignore = "five-minute performance budget; run explicitly in release mode"]
fn five_minute_today_idle_budget() {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::time::{Duration, Instant};
    use uniterm_proto::{encode_frame, ClientMessage, Command};
    common::isolate_state();
    let runtime = common::socket_root().join(format!("ut-idle-{}", std::process::id()));
    std::fs::create_dir_all(&runtime).unwrap();
    std::env::set_var("XDG_RUNTIME_DIR", &runtime);
    std::env::set_current_dir(&runtime).unwrap();
    let workspace = common::unique_workspace_name();
    let socket = runtime.join(format!("{workspace}.sock"));
    let server_socket = socket.clone();
    let server = std::thread::spawn(move || {
        let (mut server, mut poll) =
            uniterm_server::Server::bind(&server_socket, "/bin/sh", &[], 140, 30).unwrap();
        server.run(&mut poll).unwrap();
    });
    common::wait_for_socket(&socket);
    let mut client = UnixStream::connect(&socket).unwrap();
    client
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    client
        .write_all(&encode_frame(&ClientMessage::Attach {
            term: "xterm-256color".into(),
            cols: 140,
            rows: 30,
        }))
        .unwrap();
    client
        .write_all(&encode_frame(&ClientMessage::Command(Command::Today)))
        .unwrap();
    let settle = Instant::now();
    let mut bytes = [0; 65536];
    let mut initial_bytes = 0;
    while settle.elapsed() < Duration::from_secs(3) {
        if let Ok(count) = client.read(&mut bytes) {
            initial_bytes += count;
        }
    }
    fn cpu_seconds() -> f64 {
        let mut time = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: initialized, writable timespec for the process CPU clock.
        assert_eq!(
            unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut time) },
            0
        );
        time.tv_sec as f64 + time.tv_nsec as f64 / 1_000_000_000.0
    }
    client
        .set_read_timeout(Some(Duration::from_secs(300)))
        .unwrap();
    let start = Instant::now();
    let cpu_start = cpu_seconds();
    let idle_read = client.read(&mut bytes);
    let seconds = start.elapsed().as_secs_f64();
    let cpu_percent = (cpu_seconds() - cpu_start) * 100.0 / seconds;
    let rss = std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find(|s| s.starts_with("VmRSS:"))
        .unwrap()
        .to_string();
    client
        .write_all(&encode_frame(&ClientMessage::KillServer))
        .unwrap();
    server.join().unwrap();
    eprintln!("Today idle: {seconds:.1}s, {cpu_percent:.4}% CPU, {rss}, initial bytes {initial_bytes}, idle read {idle_read:?}");
    assert!(initial_bytes > 0);
    assert!(seconds >= 299.0, "idle view unexpectedly woke its client");
    assert!(
        matches!(idle_read, Err(ref e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)),
        "idle view emitted bytes"
    );
    assert!(cpu_percent < 0.5, "idle CPU budget regressed");
}
