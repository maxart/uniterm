//! W2 integration: a workflow drives live panes end to end.
//!
//! Two fake providers (shell scripts taking the role prompt as $1) stand in for
//! real CLI agents. Launching a `pair` workflow must preserve the explicit
//! role-to-provider mapping in both Pane launches and the durable run graph;
//! submission tokens still drive handoff and completion.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant};

use uniterm_proto::{
    encode_frame, ClientMessage, ControlCommand, ControlFrame, ControlRequest, ControlResult,
    FrameDecoder, OrchestrationKind, OrchestrationLaunch, ServerMessage, CONTROL_API_VERSION,
};
use uniterm_server::Server;

mod common;

use common::{isolate_state, unique_workspace_name};

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = common::socket_root().join(format!("uniterm-it-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn wait_for(path: &std::path::Path) {
    for _ in 0..200 {
        if path.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("server socket never appeared at {}", path.display());
}

fn read_until(
    stream: &mut UnixStream,
    dec: &mut FrameDecoder,
    secs: u64,
    pred: impl Fn(&str) -> bool,
) -> String {
    let mut got = String::new();
    let mut buf = [0u8; 32768];
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline && !pred(&got) {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                dec.push(&buf[..n]);
                while let Ok(Some(msg)) = dec.decode::<ServerMessage>() {
                    if let ServerMessage::RenderOps(ops) = msg {
                        got.push_str(&String::from_utf8_lossy(&ops));
                    }
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => break,
        }
    }
    got
}

/// The first `submit <N>` token in `s` after `from` occurrences are skipped.
fn nth_token(s: &str, n: usize) -> Option<u64> {
    let mut found = Vec::new();
    let mut rest = s;
    while let Some(pos) = rest.find("TOKEN submit ") {
        let tail = &rest[pos + "TOKEN submit ".len()..];
        let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(v) = digits.parse::<u64>() {
            if found.last() != Some(&v) {
                found.push(v);
            }
        }
        rest = &rest[pos + 6..];
    }
    found.get(n).copied()
}

fn read_runs(stream: &mut UnixStream, dec: &mut FrameDecoder) -> Vec<uniterm_proto::RunEntry> {
    let mut buf = [0u8; 32768];
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                dec.push(&buf[..n]);
                while let Ok(Some(message)) = dec.decode::<ServerMessage>() {
                    if let ServerMessage::Runs { runs, .. } = message {
                        return runs;
                    }
                }
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => break,
        }
    }
    panic!("run graph response never arrived")
}

fn read_artifacts(
    stream: &mut UnixStream,
    dec: &mut FrameDecoder,
) -> Vec<uniterm_proto::ArtifactEntry> {
    let mut buf = [0u8; 32768];
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                dec.push(&buf[..n]);
                while let Ok(Some(message)) = dec.decode::<ServerMessage>() {
                    if let ServerMessage::Artifacts { artifacts, .. } = message {
                        return artifacts;
                    }
                }
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => break,
        }
    }
    panic!("artifact ledger response never arrived")
}

#[test]
fn pair_workflow_advances_on_submits_and_completes() {
    isolate_state();
    let dir = temp_dir("workflow");
    // Each fake provider prints its identity and a grep-safe token, then stays
    // alive so the handoff exercises two independent role Panes.
    for provider in ["builderagent", "reviewagent"] {
        let agent = dir.join(provider);
        std::fs::write(
            &agent,
            format!(
                "#!/bin/sh\nprintf 'PROVIDER {provider} TOKEN %s\\n' \"$(printf '%s' \"$1\" | grep -o 'submit [0-9]*' | head -n1)\"\ncat > /dev/null\n"
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    let old_path = std::env::var("PATH").unwrap_or_default();
    std::env::set_var("PATH", format!("{}:{}", dir.display(), old_path));

    let workspace = unique_workspace_name();
    let sock = dir.join(format!("{workspace}.sock"));
    let sock_srv = sock.clone();
    let server = thread::spawn(move || {
        let (mut s, mut poll) =
            Server::bind(&sock_srv, "/bin/sh", &["-c", "sleep 30"], 200, 30).unwrap();
        let _ = s.run(&mut poll);
    });

    wait_for(&sock);
    let mut stream = UnixStream::connect(&sock).unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::Attach {
            term: "xterm-256color".into(),
            cols: 200,
            rows: 30,
        }))
        .unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let mut dec = FrameDecoder::new();

    // Launch over the neutral automation socket. The global provider fills
    // builder; the explicit role choice overrides only verifier.
    let control = sock.with_extension("control.sock");
    wait_for(&control);
    let mut control_writer = UnixStream::connect(&control).unwrap();
    control_writer
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut control_reader = BufReader::new(control_writer.try_clone().unwrap());
    serde_json::to_writer(
        &mut control_writer,
        &ControlRequest {
            version: CONTROL_API_VERSION,
            id: 1,
            workspace: workspace.clone(),
            command: ControlCommand::OrchestrationStart {
                launch: OrchestrationLaunch {
                    kind: OrchestrationKind::Workflow,
                    template: Some("pair".into()),
                    goal: "make the tests pass".into(),
                    provider: Some("builderagent".into()),
                    role_providers: vec![uniterm_proto::RoleProviderSelection {
                        role: "verifier".into(),
                        provider: "reviewagent".into(),
                    }],
                    project: None,
                },
            },
        },
    )
    .unwrap();
    control_writer.write_all(b"\n").unwrap();
    control_writer.flush().unwrap();
    let mut response = String::new();
    control_reader.read_line(&mut response).unwrap();
    assert!(matches!(
        serde_json::from_str::<ControlFrame>(&response).unwrap(),
        ControlFrame::Response(response)
            if matches!(response.result, Some(ControlResult::OrchestrationStarted { .. }))
    ));
    let mut all = read_until(&mut stream, &mut dec, 5, |s| {
        s.contains("wf:pair") && nth_token(s, 0).is_some()
    });
    assert!(all.contains("wf:pair"), "workflow window missing");
    assert!(
        all.contains("PROVIDER builderagent"),
        "builder did not use its selected provider: {all}"
    );
    let t1 = nth_token(&all, 0).expect("builder token never appeared");
    let artifact_dir = std::env::current_dir()
        .unwrap()
        .join("target")
        .join(format!("uniterm-artifact-it-{}", std::process::id()));
    std::fs::create_dir_all(&artifact_dir).unwrap();
    let artifact_file = artifact_dir.join("report.txt");
    std::fs::write(&artifact_file, b"first report\n").unwrap();
    let artifact_path = artifact_file
        .strip_prefix(std::env::current_dir().unwrap())
        .unwrap()
        .to_string_lossy()
        .into_owned();

    // A forged token does nothing; the real one opens the verifier's turn.
    stream
        .write_all(&encode_frame(&ClientMessage::WorkflowSubmit {
            token: t1 + 999,
            failed: false,
            verdict: None,
            summary: String::new(),
        }))
        .unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::OrchestrationSubmit {
            kind: OrchestrationKind::Workflow,
            token: t1,
            status: uniterm_proto::SubmissionStatus::Done,
            verdict: None,
            summary: String::new(),
            artifacts: vec![uniterm_proto::ArtifactClaim {
                kind: uniterm_proto::ArtifactKind::Report,
                path: artifact_path.clone(),
            }],
        }))
        .unwrap();
    all.push_str(&read_until(&mut stream, &mut dec, 5, |s| {
        nth_token(&(all.clone() + s), 1).is_some()
    }));
    let t2 = nth_token(&all, 1).expect("verifier token never appeared");
    assert_ne!(t1, t2, "verifier turn must mint a fresh token");
    assert!(
        all.contains("PROVIDER reviewagent"),
        "verifier inherited the builder provider: {all}"
    );

    stream
        .write_all(&encode_frame(&ClientMessage::RunList {
            project: None,
            active_only: true,
        }))
        .unwrap();
    let runs = read_runs(&mut stream, &mut dec);
    let run = runs
        .iter()
        .find(|run| run.title.contains("workflow pair"))
        .expect("workflow missing from run graph");
    assert_eq!(run.roles.len(), 2);
    assert_eq!(run.roles[0].provider, "builderagent");
    assert_eq!(run.roles[1].provider, "reviewagent");

    stream
        .write_all(&encode_frame(&ClientMessage::ArtifactList {
            project: Some(run.project),
            run: Some(run.id),
            include_superseded: false,
        }))
        .unwrap();
    let artifacts = read_artifacts(&mut stream, &mut dec);
    assert_eq!(artifacts.len(), 1);
    assert_eq!(artifacts[0].kind, uniterm_proto::ArtifactKind::Report);
    assert_eq!(artifacts[0].path, artifact_path);
    assert_eq!(artifacts[0].producer_run, run.id);
    assert_eq!(artifacts[0].producer_role, run.roles[0].id);
    assert_eq!(artifacts[0].status, uniterm_core::ArtifactStatus::Available);
    let first_digest = artifacts[0].digest.clone();

    // A watched file change is re-observed on the runtime, then appended as a
    // lifecycle event by the core. No timer or grid access is involved.
    std::fs::write(&artifact_file, b"second report\n").unwrap();
    let refreshed = (0..100).find_map(|_| {
        stream
            .write_all(&encode_frame(&ClientMessage::ArtifactList {
                project: None,
                run: Some(run.id),
                include_superseded: false,
            }))
            .unwrap();
        let artifact = read_artifacts(&mut stream, &mut dec).into_iter().next()?;
        if artifact.digest != first_digest {
            Some(artifact)
        } else {
            thread::sleep(Duration::from_millis(20));
            None
        }
    });
    let refreshed = refreshed.expect("watched Artifact digest never refreshed");
    assert_eq!(refreshed.size, 14);

    std::fs::remove_file(&artifact_file).unwrap();
    let missing = (0..100).find_map(|_| {
        stream
            .write_all(&encode_frame(&ClientMessage::ArtifactList {
                project: None,
                run: Some(run.id),
                include_superseded: false,
            }))
            .unwrap();
        let artifact = read_artifacts(&mut stream, &mut dec).into_iter().next()?;
        if artifact.status == uniterm_core::ArtifactStatus::Missing {
            Some(artifact)
        } else {
            thread::sleep(Duration::from_millis(20));
            None
        }
    });
    assert!(missing.is_some(), "watched Artifact never became missing");

    // The verifier approves: the run completes and the window title says so.
    stream
        .write_all(&encode_frame(&ClientMessage::WorkflowSubmit {
            token: t2,
            failed: false,
            verdict: Some("approved".into()),
            summary: "looks good".into(),
        }))
        .unwrap();
    let done = read_until(&mut stream, &mut dec, 5, |s| s.contains("wf:pair: done"));
    assert!(
        done.contains("wf:pair: done"),
        "approved verdict did not complete the workflow"
    );

    stream
        .write_all(&encode_frame(&ClientMessage::KillServer))
        .unwrap();
    let _ = server.join();
    let _ = std::fs::remove_dir_all(artifact_dir);
}
