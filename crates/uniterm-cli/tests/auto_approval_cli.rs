//! CLI mode changes use explicit Workspace scope and the shared control path.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::process::Command;
use uniterm_proto::{encode_frame, ClientMessage};

mod common;

#[test]
fn auto_approval_cli_requires_a_valid_explicit_mode() {
    let state = common::isolate_state();
    let runtime = common::socket_root().join(format!("ut-ap-cli-{}", std::process::id()));
    std::fs::create_dir_all(&runtime).unwrap();
    std::env::set_var("XDG_RUNTIME_DIR", &runtime);
    let name = common::unique_workspace_name();
    let socket = uniterm_server::server::default_socket_path(&name);
    let server_socket = socket.clone();
    let server = std::thread::spawn(move || {
        let (mut server, mut poll) = uniterm_server::Server::bind(
            &server_socket,
            "/bin/sh",
            &["-c", "read answer"],
            100,
            24,
        )
        .unwrap();
        server.run(&mut poll).unwrap();
    });
    common::wait_for_socket(&socket.with_extension("control.sock"));
    let run = |mode: &str| {
        Command::new(env!("CARGO_BIN_EXE_ut"))
            .args(["agent", "auto-approve", mode, "-w", &name])
            .env("XDG_STATE_HOME", &state)
            .env("XDG_RUNTIME_DIR", &runtime)
            .output()
            .unwrap()
    };
    let results = [
        run("status"),
        run("unknown"),
        run("on"),
        run("status"),
        run("off"),
    ];
    let mut stream = UnixStream::connect(&socket).unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::KillServer))
        .unwrap();
    server.join().unwrap();
    assert!(results[0].status.success());
    assert!(String::from_utf8_lossy(&results[0].stdout).contains("auto-approve: off"));
    assert_eq!(results[1].status.code(), Some(2));
    for output in &results[2..4] {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("auto-approve: on"));
    }
    assert!(results[4].status.success());
    assert!(String::from_utf8_lossy(&results[4].stdout).contains("auto-approve: off"));
    std::fs::remove_dir_all(runtime).unwrap();
}
