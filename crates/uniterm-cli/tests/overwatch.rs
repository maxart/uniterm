//! Isolated lifecycle, recovery, scope and idempotency coverage for Overwatch.
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use uniterm_core::overwatch::{Operation, Phase, State};
use uniterm_proto::{
    encode_frame, ClientMessage, ControlCommand, ControlResult, FrameDecoder, ServerMessage,
};
mod common;

struct ServerProcess(std::process::Child);
impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn request_models(socket: &std::path::Path) -> uniterm_core::agent_models::ModelCatalog {
    let mut stream = UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::Attach {
            term: "xterm".into(),
            cols: 100,
            rows: 30,
        }))
        .unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::Overwatch {
            operation: Operation::Models { request: 73 },
        }))
        .unwrap();
    let mut decoder = FrameDecoder::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut bytes = [0; 32768];
    loop {
        while let Some(message) = decoder.decode::<ServerMessage>().unwrap() {
            if let ServerMessage::OverwatchModels {
                request: 73,
                catalog,
            } = message
            {
                return catalog;
            }
        }
        assert!(Instant::now() < deadline, "model catalog did not arrive");
        match stream.read(&mut bytes) {
            Ok(0) => panic!("server closed model catalog connection"),
            Ok(count) => decoder.push(&bytes[..count]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) => {}
            Err(error) => panic!("model catalog read: {error}"),
        }
    }
}

#[test]
fn overwatch_is_opt_in_scoped_idempotent_and_recovers_without_stealing_focus() {
    let state_home = common::isolate_state();
    let runtime = common::socket_root().join(format!("ow-{}", std::process::id()));
    let bin = runtime.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let provider_home = runtime.join("codex-home");
    std::fs::create_dir_all(provider_home.join("sessions")).unwrap();
    std::fs::write(
        provider_home.join("models_cache.json"),
        serde_json::json!({
            "models":[{"slug":"gpt-6-luna","display_name":"GPT-6-Luna","visibility":"list",
                "default_reasoning_level":"medium","supported_reasoning_levels":[
                    {"effort":"medium","description":"Standard reasoning"},
                    {"effort":"high","description":"More reasoning"}
                ]}]
        })
        .to_string(),
    )
    .unwrap();
    let fake = bin.join("codex");
    std::fs::write(&fake, r#"#!/bin/sh
printf '%s\n' "$@" > "$CODEX_HOME/last-argv"
printf '%s' "$UNITERM_SOCKET" > "$CODEX_HOME/last-socket"
printf '\033]777;notify;uniterm://cli-agent;{"agent":"codex","event":"prompt_submit","session_id":"overwatch-test","resume_command":["codex","resume","overwatch-test"]}\007'
exec /bin/cat
"#).unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
    let alternate = bin.join("claude");
    std::fs::write(&alternate, r#"#!/bin/sh
printf '%s\n' "$@" > "$CODEX_HOME/last-argv"
printf '%s' "$UNITERM_SOCKET" > "$CODEX_HOME/last-socket"
printf '\033]777;notify;uniterm://cli-agent;{"agent":"claude","event":"prompt_submit","session_id":"alternate-overwatch"}\007'
exec /bin/cat
"#).unwrap();
    std::fs::set_permissions(&alternate, std::fs::Permissions::from_mode(0o700)).unwrap();
    let workspace = common::unique_workspace_name();
    let socket = runtime.join("uniterm").join(format!("{workspace}.sock"));
    let command = || {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ut"));
        cmd.env("XDG_STATE_HOME", &state_home)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("XDG_CONFIG_HOME", &runtime)
            .env("CODEX_HOME", &provider_home)
            .env("SHELL", "/bin/sh")
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env_remove("UNITERM_SOCKET")
            .env_remove("UNITERM_PANE_ID")
            .env_remove("UNITERM_STATE_KEY_FILE");
        cmd
    };
    let start = || {
        ServerProcess(
            command()
                .args(["serve", &workspace])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        )
    };
    let wait_server = || {
        let deadline = Instant::now() + Duration::from_secs(10);
        while uniterm_client::pane_list(&socket).is_err() {
            assert!(Instant::now() < deadline, "server failed to start");
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    let call = |operation| {
        let response =
            uniterm_client::control_request(&socket, ControlCommand::Overwatch { operation })
                .unwrap();
        assert!(response.error.is_none(), "{:?}", response.error);
        let Some(ControlResult::Overwatch { state }) = response.result else {
            panic!("wrong response")
        };
        state
    };
    let wait_state = |predicate: &dyn Fn(&State) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let state = call(Operation::Status);
            if predicate(&state) {
                break state;
            }
            assert!(Instant::now() < deadline, "state: {state:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    let mut server = start();
    wait_server();
    assert_eq!(call(Operation::Status), State::default());
    let project_output = command()
        .args([
            "project",
            "add",
            "not-worked-today",
            runtime.to_str().unwrap(),
            "--background",
            "-w",
            &workspace,
        ])
        .output()
        .unwrap();
    assert!(
        project_output.status.success(),
        "{}",
        String::from_utf8_lossy(&project_output.stderr)
    );
    let preview = uniterm_client::control_request(
        &socket,
        ControlCommand::OverwatchPrune {
            expected_sequence: None,
            expected_date: None,
            projects: vec![],
            confirmed: false,
        },
    )
    .unwrap();
    assert!(preview.error.is_none(), "{:?}", preview.error);
    let Some(ControlResult::OverwatchPrune { report }) = preview.result else {
        panic!("missing prune preview")
    };
    assert_eq!(report.candidates.len(), 1);
    assert_eq!(report.candidates[0].name, "not-worked-today");
    let candidate_ids: Vec<_> = report.candidates.iter().map(|p| p.id).collect();
    let denied = uniterm_client::control_request(
        &socket,
        ControlCommand::OverwatchPrune {
            expected_sequence: Some(report.sequence),
            expected_date: Some(report.date.clone()),
            projects: candidate_ids.clone(),
            confirmed: false,
        },
    )
    .unwrap();
    assert_eq!(denied.error.unwrap().code, "confirmation_required");
    assert_eq!(uniterm_client::pane_list(&socket).unwrap().1.len(), 2);
    let applied = uniterm_client::control_request(
        &socket,
        ControlCommand::OverwatchPrune {
            expected_sequence: Some(report.sequence),
            expected_date: Some(report.date.clone()),
            projects: candidate_ids,
            confirmed: true,
        },
    )
    .unwrap();
    assert!(applied.error.is_none(), "{:?}", applied.error);
    assert!(
        matches!(applied.result, Some(ControlResult::OverwatchPrune { report }) if report.applied)
    );
    assert_eq!(uniterm_client::pane_list(&socket).unwrap().1.len(), 1);

    let original = uniterm_client::pane_list(&socket)
        .unwrap()
        .1
        .into_iter()
        .find(|p| p.active)
        .unwrap()
        .id;
    let configured = command()
        .args([
            "overwatch",
            "configure",
            "codex",
            "1",
            "--model",
            "fixture-model",
            "--json",
            "-w",
            &workspace,
        ])
        .output()
        .unwrap();
    assert!(
        configured.status.success(),
        "{}",
        String::from_utf8_lossy(&configured.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<State>(&configured.stdout)
            .unwrap()
            .model
            .as_deref(),
        Some("fixture-model")
    );
    call(Operation::Enable);
    let mut running = wait_state(&|s| s.phase == Phase::Running);
    let assert_model = |model: &str, effort: Option<&str>| {
        let args = std::fs::read_to_string(provider_home.join("last-argv")).unwrap();
        assert_eq!(
            args.lines()
                .filter(|arg| *arg == format!("--model={model}"))
                .count(),
            1,
            "{args}"
        );
        match effort {
            Some(effort) => assert_eq!(
                args.lines()
                    .filter(|arg| *arg == format!("model_reasoning_effort=\"{effort}\""))
                    .count(),
                1,
                "{args}"
            ),
            None => assert!(!args.contains("model_reasoning_effort="), "{args}"),
        }
    };
    assert_model("fixture-model", None);
    // Live settings replace only the assistant. Same selections remain no-ops.
    assert_eq!(
        call(Operation::Model {
            model: Some("fixture-model".into())
        }),
        running
    );
    for (provider, model) in [
        ("codex", Some("other-model")),
        ("codex", None),
        ("claude", Some("alternate-model")),
        ("codex", Some("fixture-model")),
    ] {
        let previous = running.pane;
        let operation = if provider == running.provider {
            Operation::Model {
                model: model.map(str::to_owned),
            }
        } else {
            Operation::Configure {
                provider: provider.into(),
                model: model.map(str::to_owned),
                project: None,
                manage: running.manage,
            }
        };
        let changed = call(operation);
        assert!(changed.enabled);
        running = wait_state(&|s| s.phase == Phase::Running && s.pane != previous);
        assert_eq!(running.provider, provider);
        assert_eq!(running.model.as_deref(), model);
        assert_eq!(running.recoveries, 0);
        // Both native providers inherit the grant on a fresh invocation, and
        // the restricted endpoint cannot change its own grant or navigation.
        let manager = socket
            .parent()
            .unwrap()
            .join("manager")
            .join(socket.file_name().unwrap());
        assert_eq!(
            std::fs::read_to_string(provider_home.join("last-socket")).unwrap(),
            manager.to_str().unwrap()
        );
        for denied_command in [
            ControlCommand::Overwatch {
                operation: Operation::Management { enabled: true },
            },
            ControlCommand::HierarchyFocus {
                project: uniterm_core::ProjectId(1),
                tab: 1,
                pane: None,
            },
            ControlCommand::PaneMove {
                pane: original,
                project: uniterm_core::ProjectId(1),
                tab: None,
            },
        ] {
            let response = uniterm_client::control_request(&manager, denied_command).unwrap();
            assert_eq!(response.error.unwrap().code, "read_only");
        }
        let old_pane = running.pane;
        call(Operation::Management { enabled: true });
        running = wait_state(&|s| s.phase == Phase::Running && s.pane != old_pane);
        assert!(running.manage);
        let inherited = std::fs::read_to_string(provider_home.join("last-socket")).unwrap();
        assert_eq!(inherited, socket.to_str().unwrap());
        let run = |args: &[&str]| {
            let output = command()
                .env("UNITERM_SOCKET", &inherited)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_string()
        };
        let project = run(&[
            "project",
            "add",
            "ui-control",
            runtime.to_str().unwrap(),
            "--background",
        ]);
        run(&["project", "switch", "ui-control"]);
        let panes = uniterm_client::pane_list(&socket).unwrap().1;
        assert_eq!(
            panes
                .iter()
                .find(|pane| pane.active)
                .unwrap()
                .project
                .0
                .to_string(),
            project
        );
        let anchor = panes.iter().find(|pane| pane.active).unwrap().id;
        run(&["project", "move", &project, "top"]);
        let split = run(&["pane", "split", &anchor.0.to_string(), "--background"]);
        run(&["pane", "move", &split, "--new-tab"]);
        assert_eq!(
            uniterm_client::pane_list(&socket)
                .unwrap()
                .1
                .iter()
                .find(|p| p.id.0.to_string() == split)
                .unwrap()
                .tab,
            2
        );
        run(&["tab", "move", &project, "2", "left", "--background"]);
        run(&["tab", "focus", &project, "2"]);
        run(&["pane", "move", &split, &project, "2"]);
        let panes = uniterm_client::pane_list(&socket).unwrap().1;
        assert_eq!(
            panes
                .iter()
                .filter(|p| p.project.0.to_string() == project)
                .map(|p| p.tab)
                .collect::<Vec<_>>(),
            vec![1, 1]
        );
        let noop = command()
            .env("UNITERM_SOCKET", &inherited)
            .args(["pane", "move", &split, &project, "1"])
            .output()
            .unwrap();
        assert!(
            !noop.status.success(),
            "same-Tab move must not claim a mutation"
        );
        let cross_project = command()
            .env("UNITERM_SOCKET", &inherited)
            .args(["pane", "move", &split, "1", "1"])
            .output()
            .unwrap();
        assert!(!cross_project.status.success());
        let unconfirmed = uniterm_client::control_request(
            &socket,
            ControlCommand::ProjectRemove {
                project: uniterm_core::ProjectId(project.parse().unwrap()),
                confirmed: false,
            },
        )
        .unwrap();
        assert_eq!(unconfirmed.error.unwrap().code, "confirmation_required");
        run(&["pane", "focus", &original.0.to_string()]);
        run(&["project", "remove", &project]);
        assert_eq!(uniterm_client::pane_list(&socket).unwrap().1.len(), 1);
        let old_pane = running.pane;
        call(Operation::Management { enabled: false });
        running = wait_state(&|s| s.phase == Phase::Running && s.pane != old_pane);
        assert!(!running.manage);
        assert_eq!(
            std::fs::read_to_string(provider_home.join("last-socket")).unwrap(),
            manager.to_str().unwrap()
        );
        let args = std::fs::read_to_string(provider_home.join("last-argv")).unwrap();
        match model {
            Some(model) => assert!(
                args.lines()
                    .any(|arg| arg == model || arg == format!("--model={model}")),
                "{args}"
            ),
            None => assert!(!args.contains("--model"), "{args}"),
        }
        assert!(
            uniterm_client::pane_read(&socket, previous.unwrap(), 1)
                .unwrap()
                .is_none(),
            "old assistant closed"
        );
        assert_eq!(
            uniterm_client::pane_list(&socket)
                .unwrap()
                .1
                .iter()
                .find(|p| p.active)
                .unwrap()
                .id,
            original
        );
    }
    assert_model("fixture-model", None);
    let catalog = request_models(&socket);
    assert_eq!(catalog.provider, "codex");
    assert_eq!(catalog.find("GPT-6-Luna").unwrap().id, "gpt-6-luna");
    assert_eq!(catalog.find("GPT-6-Luna").unwrap().efforts[0].id, "medium");
    let select = |provider: &str, model: &str, effort: &str| {
        command()
            .args([
                "overwatch",
                "select",
                provider,
                model,
                "--effort",
                effort,
                "--json",
                "-w",
                &workspace,
            ])
            .output()
            .unwrap()
    };
    let previous = running.pane;
    let selected = select("codex", "GPT-6-Luna", "medium");
    assert!(
        selected.status.success(),
        "{}",
        String::from_utf8_lossy(&selected.stderr)
    );
    let selected: State = serde_json::from_slice(&selected.stdout).unwrap();
    assert_eq!(selected.model.as_deref(), Some("gpt-6-luna"));
    assert_eq!(selected.effort.as_deref(), Some("medium"));
    running = wait_state(&|s| s.phase == Phase::Running && s.pane != previous);
    assert_model("gpt-6-luna", Some("medium"));
    assert!(!std::fs::read_to_string(provider_home.join("last-argv"))
        .unwrap()
        .lines()
        .any(|arg| arg == "resume"));
    let repeated = select("codex", "gpt-6-luna", "medium");
    assert!(repeated.status.success());
    assert_eq!(
        serde_json::from_slice::<State>(&repeated.stdout).unwrap(),
        running
    );
    let stale = select("claude", "gpt-6-luna", "medium");
    assert!(!stale.status.success());
    assert_eq!(call(Operation::Status), running);
    let unsupported = select("codex", "gpt-6-luna", "unsupported");
    assert!(!unsupported.status.success());
    assert_eq!(call(Operation::Status), running);
    for effort in ["high", "medium"] {
        let previous = running.pane;
        let changed = select("codex", "gpt-6-luna", effort);
        assert!(
            changed.status.success(),
            "{}",
            String::from_utf8_lossy(&changed.stderr)
        );
        running = wait_state(&|s| s.phase == Phase::Running && s.pane != previous);
        assert_model("gpt-6-luna", Some(effort));
        assert!(uniterm_client::pane_read(&socket, previous.unwrap(), 1)
            .unwrap()
            .is_none());
    }
    let human_status = command()
        .args(["overwatch", "status", "-w", &workspace])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&human_status.stdout).contains("effort: medium"));
    let first = running.pane.unwrap();
    assert_eq!(
        uniterm_client::pane_list(&socket).unwrap().1.len(),
        1,
        "Overwatch is not a Project Pane or Tab"
    );
    assert_eq!(call(Operation::Enable).pane, Some(first));
    assert_eq!(
        uniterm_client::pane_list(&socket)
            .unwrap()
            .1
            .iter()
            .find(|p| p.active)
            .unwrap()
            .id,
        original
    );
    // Stable ids from Overwatch status work without any Project/Tab membership.
    let pane_id = first.0.to_string();
    for args in [
        vec!["pane", "focus", &pane_id],
        vec!["pane", "prompt", &pane_id, "detached-numeric-prompt"],
    ] {
        let result = command()
            .args(args)
            .args(["-w", &workspace])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    let observed = command()
        .args([
            "pane",
            "wait-output",
            &pane_id,
            "detached-numeric-prompt",
            "--timeout",
            "5",
            "-w",
            &workspace,
        ])
        .output()
        .unwrap();
    assert!(
        observed.status.success(),
        "{}",
        String::from_utf8_lossy(&observed.stderr)
    );
    let read = command()
        .args(["pane", "read", &pane_id, "--json", "-w", &workspace])
        .output()
        .unwrap();
    assert!(read.status.success());
    let read: serde_json::Value = serde_json::from_slice(&read.stdout).unwrap();
    assert!(read["text"]
        .as_str()
        .unwrap()
        .contains("detached-numeric-prompt"));
    assert_eq!(
        uniterm_client::pane_list(&socket)
            .unwrap()
            .1
            .iter()
            .find(|pane| pane.active)
            .unwrap()
            .id,
        original
    );
    let queued = command()
        .args([
            "instruction",
            "add",
            &pane_id,
            "detached-queued-direction",
            "--json",
            "-w",
            &workspace,
        ])
        .output()
        .unwrap();
    assert!(
        queued.status.success(),
        "{}",
        String::from_utf8_lossy(&queued.stderr)
    );
    let queued: serde_json::Value = serde_json::from_slice(&queued.stdout).unwrap();
    let instruction_id = queued["id"].as_u64().unwrap().to_string();
    assert!(queued["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["pane"] == first.0 && item["project"].is_null()));
    let listing = command()
        .args(["instruction", "list", "-w", &workspace])
        .output()
        .unwrap();
    assert!(listing.status.success());
    let listing = String::from_utf8_lossy(&listing.stdout);
    assert!(
        listing.contains(&format!("Overwatch / Pane {pane_id}")),
        "{listing}"
    );
    assert!(!listing.contains("Tab 0"));
    let sent = command()
        .args(["instruction", "send-now", &instruction_id, "-w", &workspace])
        .output()
        .unwrap();
    assert!(
        sent.status.success(),
        "{}",
        String::from_utf8_lossy(&sent.stderr)
    );
    let observed = command()
        .args([
            "pane",
            "wait-output",
            &pane_id,
            "detached-queued-direction",
            "--timeout",
            "5",
            "-w",
            &workspace,
        ])
        .output()
        .unwrap();
    assert!(
        observed.status.success(),
        "{}",
        String::from_utf8_lossy(&observed.stderr)
    );
    assert!(uniterm_client::instruction_list(&socket)
        .unwrap()
        .is_empty());
    let manager = socket
        .parent()
        .unwrap()
        .join("manager")
        .join(socket.file_name().unwrap());
    assert!(matches!(
        uniterm_client::control_request(&manager, ControlCommand::OverwatchContext)
            .unwrap()
            .result,
        Some(ControlResult::OverwatchContext { .. })
    ));
    let denied = uniterm_client::control_request(
        &manager,
        ControlCommand::Overwatch {
            operation: Operation::Disable,
        },
    )
    .unwrap();
    assert_eq!(denied.error.unwrap().code, "read_only");
    // Closing the owned Pane is an observed failure, not a reason to lose ownership of the Workspace.
    let result = command()
        .args(["pane", "close", &first.0.to_string(), "-w", &workspace])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let recovered = wait_state(&|s| s.phase == Phase::Running && s.pane != Some(first));
    assert_eq!(recovered.recoveries, 1);
    assert_model("gpt-6-luna", Some("medium"));
    assert_eq!(
        uniterm_client::pane_list(&socket)
            .unwrap()
            .1
            .iter()
            .find(|p| p.active)
            .unwrap()
            .id,
        original
    );
    // Crash recovery reuses a validated provider session and preserves the budget.
    server.0.kill().unwrap();
    server.0.wait().unwrap();
    server = start();
    wait_server();
    let resumed = wait_state(&|s| s.phase == Phase::Running);
    assert_eq!(resumed.recoveries, 1);
    assert_eq!(resumed.pane, recovered.pane);
    assert_model("gpt-6-luna", Some("medium"));
    assert_eq!(resumed.effort.as_deref(), Some("medium"));
    assert!(std::fs::read_to_string(provider_home.join("last-argv"))
        .unwrap()
        .lines()
        .any(|arg| arg == "resume"));
    let added = command()
        .args([
            "project",
            "add",
            "survivor",
            runtime.to_str().unwrap(),
            "--background",
            "-w",
            &workspace,
        ])
        .output()
        .unwrap();
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );
    let removed = uniterm_client::control_request(
        &socket,
        ControlCommand::ProjectRemove {
            project: uniterm_core::ProjectId(1),
            confirmed: true,
        },
    )
    .unwrap();
    assert!(removed.error.is_none(), "{:?}", removed.error);
    assert_eq!(
        call(Operation::Status).pane,
        resumed.pane,
        "launch-context Project removal must preserve assistant"
    );
    let result = command()
        .args([
            "pane",
            "close",
            &resumed.pane.unwrap().0.to_string(),
            "-w",
            &workspace,
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let after_removal = wait_state(&|s| s.phase == Phase::Running && s.pane != resumed.pane);
    assert_eq!(after_removal.recoveries, 2);
    assert_eq!(uniterm_client::pane_list(&socket).unwrap().1.len(), 1);
    call(Operation::Disable);
    assert_eq!(call(Operation::Disable).phase, Phase::Off);
    assert_eq!(uniterm_client::pane_list(&socket).unwrap().1.len(), 1);
    let result = command()
        .args(["overwatch", "status", "--json", "-w", &workspace])
        .output()
        .unwrap();
    assert!(result.status.success());
    assert_eq!(
        serde_json::from_slice::<State>(&result.stdout)
            .unwrap()
            .phase,
        Phase::Off
    );
    // OFF survives both compaction and an actual server restart.
    uniterm_client::kill_server(&socket).unwrap();
    server.0.wait().unwrap();
    uniterm_server::history::compact(&workspace).unwrap();
    server = start();
    wait_server();
    let restored = call(Operation::Status);
    assert!(!restored.enabled);
    assert_eq!(restored.provider, "codex");
    assert_eq!(restored.model.as_deref(), Some("gpt-6-luna"));
    assert_eq!(restored.effort.as_deref(), Some("medium"));
    let cleared = command()
        .args([
            "overwatch",
            "select",
            "codex",
            "",
            "--effort",
            "",
            "--json",
            "-w",
            &workspace,
        ])
        .output()
        .unwrap();
    assert!(cleared.status.success());
    let cleared: State = serde_json::from_slice(&cleared.stdout).unwrap();
    assert_eq!(cleared.model, None);
    assert_eq!(cleared.effort, None);
    assert_eq!(uniterm_client::pane_list(&socket).unwrap().1.len(), 1);
    uniterm_client::kill_server(&socket).unwrap();
    server.0.wait().unwrap();
}
