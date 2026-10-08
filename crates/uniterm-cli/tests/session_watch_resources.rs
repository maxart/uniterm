//! History size and concurrent automation must not exhaust control descriptors.
use serde_json::{json, Value};
use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use uniterm_proto::{ControlCommand, ControlFrame, ControlRequest, ControlResponse, PaneId};

mod common;

struct Server {
    child: Child,
    socket: PathBuf,
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = uniterm_client::kill_server(&self.socket);
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn connect_with_timeout(
    socket: &Path,
    timeout: Duration,
) -> std::io::Result<BufReader<UnixStream>> {
    let stream = UnixStream::connect(socket.with_extension("control.sock"))?;
    stream.set_read_timeout(Some(timeout))?;
    Ok(BufReader::new(stream))
}

fn connect(socket: &Path) -> BufReader<UnixStream> {
    connect_with_timeout(socket, Duration::from_secs(10)).unwrap()
}

#[track_caller]
fn exchange(
    client: &mut BufReader<UnixStream>,
    name: &str,
    command: ControlCommand,
) -> ControlResponse {
    let request = ControlRequest {
        version: uniterm_proto::CONTROL_API_VERSION,
        id: 1,
        workspace: name.into(),
        command,
    };
    writeln!(
        client.get_mut(),
        "{}",
        serde_json::to_string(&request).unwrap()
    )
    .unwrap();
    let mut line = String::new();
    assert!(
        client.read_line(&mut line).unwrap() > 0,
        "control closed before answering {request:?}"
    );
    let ControlFrame::Response(response) = serde_json::from_str(&line).unwrap() else {
        panic!("{line}")
    };
    assert_eq!(response.id, 1);
    response
}

#[track_caller]
fn request(client: &mut BufReader<UnixStream>, name: &str, command: ControlCommand) -> Value {
    let response = exchange(client, name, command);
    assert!(response.error.is_none(), "{response:?}");
    serde_json::to_value(response.result.unwrap()).unwrap()["data"].clone()
}

fn header(id: &str, parent: Option<&str>) -> String {
    json!({"type":"session_meta","payload":{"id":id,"parent_thread_id":parent,"source":"cli"}})
        .to_string()
        + "\n"
}

fn await_session(client: &mut BufReader<UnixStream>, name: &str, id: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let agents = request(
            client,
            name,
            ControlCommand::TimelineQuery {
                date: "today".into(),
                filter: String::new(),
                before: None,
            },
        );
        if agents["day"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|agent| agent["session_id"] == id)
        {
            return;
        }
        assert!(Instant::now() < deadline, "missing session {id}: {agents}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn large_history_and_many_control_clients_work_under_low_descriptor_limit() {
    let state = common::isolate_state();
    let runtime = common::socket_root().join(format!("ut-fd-{}", std::process::id()));
    std::fs::create_dir_all(&runtime).unwrap();
    let home = runtime.join("provider");
    let history = home.join("sessions/2025/01/01");
    std::fs::create_dir_all(&history).unwrap();
    for id in 0..1024 {
        std::fs::write(
            history.join(format!("rollout-{id}.jsonl")),
            header(&format!("old-{id}"), None),
        )
        .unwrap();
    }
    let directory = home.join("sessions/2026/10/06");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("rollout-root.jsonl"), header("root", None)).unwrap();
    std::fs::write(
        directory.join("rollout-child.jsonl"),
        header("child", Some("root")),
    )
    .unwrap();
    let alias = runtime.join("provider-link");
    std::os::unix::fs::symlink(&home, &alias).unwrap();
    let name = common::unique_workspace_name();
    let socket = runtime.join("uniterm").join(format!("{name}.sock"));
    let binary = std::env::var_os("UNITERM_SESSION_WATCH_BINARY")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_ut").into());
    let mut command = Command::new(binary);
    command
        .args(["serve", &name])
        .env("XDG_STATE_HOME", &state)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("XDG_CONFIG_HOME", runtime.join("config"))
        .env("CODEX_HOME", &alias)
        .env("SHELL", "/bin/sh")
        .env_remove("UNITERM_SOCKET")
        .env_remove("UNITERM_PANE_ID")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    // SAFETY: the child-only pre-exec hook calls only getrlimit/setrlimit.
    // The harness, other Workspaces, and machine-wide limits are unchanged.
    unsafe {
        command.pre_exec(|| {
            let mut limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            limit.rlim_cur = limit.rlim_max.min(256);
            if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let server = Server {
        child: command.spawn().unwrap(),
        socket: socket.clone(),
    };
    common::wait_for_socket(&socket.with_extension("control.sock"));
    let mut client = connect(&socket);
    let context = request(
        &mut client,
        &name,
        ControlCommand::AgentHookContext { pane: PaneId(1) },
    );
    let pid = context["foreground_pid"].as_i64().unwrap() as i32;
    request(
        &mut client,
        &name,
        ControlCommand::AgentHookReport {
            pane: PaneId(1),
            source_pid: pid,
            foreground_pid: pid,
            invocation: context["invocation"].as_u64(),
            json: json!({"agent":"codex","event":"session_start","session_id":"root",
            "transcript_path":alias.join("sessions/2026/10/06/rollout-root.jsonl")})
            .to_string(),
        },
    );
    await_session(&mut client, &name, "child");

    // Every retained socket has received an answer, so this measures admitted
    // connections rather than the kernel's pending-connect backlog.
    let mut held = Vec::new();
    for _ in 0..140 {
        let mut connection = connect(&socket);
        request(&mut connection, &name, ControlCommand::Capabilities);
        held.push(connection);
    }
    let fresh = home.join("sessions/2026/10/07");
    std::fs::create_dir_all(&fresh).unwrap();
    std::fs::write(
        fresh.join("rollout-grandchild.jsonl"),
        header("grandchild", Some("child")),
    )
    .unwrap();
    await_session(&mut client, &name, "grandchild");
    let delayed = fresh.join("rollout-delayed.jsonl");
    std::fs::write(&delayed, "").unwrap();
    std::thread::sleep(Duration::from_millis(150));
    std::fs::write(&delayed, header("delayed", Some("grandchild"))).unwrap();
    await_session(&mut client, &name, "delayed");
    let split = request(
        &mut client,
        &name,
        ControlCommand::PaneSplit {
            pane: PaneId(1),
            axis: uniterm_proto::SplitAxis::LeftRight,
            background: true,
        },
    );
    assert_eq!(split["accepted"], true);
    let (_, panes) = uniterm_client::pane_list(&socket).unwrap();
    assert_eq!(panes.len(), 2, "one split must create exactly one Pane");
    assert!(
        panes
            .iter()
            .find(|pane| pane.id == PaneId(1))
            .unwrap()
            .active
    );
    let pane = panes.iter().find(|pane| pane.id != PaneId(1)).unwrap().id;
    let receipt = runtime.join("submission");
    request(
        &mut client,
        &name,
        ControlCommand::PanePaste {
            pane,
            text: format!("printf ACK >> '{}'", receipt.display()),
            submit: true,
        },
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !std::fs::read(&receipt).is_ok_and(|contents| !contents.is_empty()) {
        assert!(Instant::now() < deadline, "submission never executed");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(std::fs::read_to_string(&receipt).unwrap(), "ACK");
    let missing = exchange(
        &mut client,
        &name,
        ControlCommand::PanePaste {
            pane: PaneId(u64::MAX),
            text: "missing".into(),
            submit: true,
        },
    );
    assert!(
        missing.error.is_some()
            || matches!(
                missing.result,
                Some(uniterm_proto::ControlResult::PaneSent {
                    found: false,
                    accepted: false,
                    ..
                })
            )
    );
    request(&mut connect(&socket), &name, ControlCommand::AgentList);
    // Exhaust the child's remaining socket budget deliberately. Once held
    // clients close, the acceptor must recover without a Workspace restart.
    let probe = serde_json::to_string(&ControlRequest {
        version: uniterm_proto::CONTROL_API_VERSION,
        id: 2,
        workspace: name.clone(),
        command: ControlCommand::Capabilities,
    })
    .unwrap()
        + "\n";
    let mut exhausted = false;
    for _ in 0..256 {
        // An exhausted acceptor may reject a connection before timeout setup
        // completes (macOS can report EINVAL for that closed socket). Only the
        // deliberate exhaustion phase accepts this; recovery stays strict.
        let mut connection = match connect_with_timeout(&socket, Duration::from_millis(200)) {
            Ok(connection) => connection,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionRefused
                        | std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::NotConnected
                        | std::io::ErrorKind::BrokenPipe
                ) || (cfg!(target_os = "macos")
                    && error.raw_os_error() == Some(libc::EINVAL)) =>
            {
                exhausted = true;
                break;
            }
            Err(error) => panic!("unexpected exhaustion probe setup failure: {error}"),
        };
        let mut line = String::new();
        if connection.get_mut().write_all(probe.as_bytes()).is_err()
            || !matches!(connection.read_line(&mut line), Ok(count) if count > 0)
        {
            exhausted = true;
            break;
        }
        held.push(connection);
    }
    assert!(
        exhausted,
        "fixture did not reach its child-only descriptor budget"
    );
    drop(held);
    // Closing hundreds of clients also queues their disconnect notifications.
    // A newly accepted read-only probe can be explicitly rejected while that
    // bounded intake drains. This is recovery, not an immediate-read guarantee;
    // never retry a mutation or an ambiguous transport failure here.
    let mut recovered = connect(&socket);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let response = exchange(&mut recovered, &name, ControlCommand::Capabilities);
        match response.error {
            None => {
                assert!(response.result.is_some());
                break;
            }
            Some(error) => {
                assert_eq!(error.code, "server_busy", "{error:?}");
                assert!(
                    Instant::now() < deadline,
                    "intake never recovered: {error:?}"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
    drop(recovered);
    drop(client);
    drop(server);
    std::fs::remove_dir_all(runtime).unwrap();
}
