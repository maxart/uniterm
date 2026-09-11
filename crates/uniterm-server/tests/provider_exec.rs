//! Process identity misses must not hide a later exec in the same invocation.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::Duration;
use uniterm_core::PaneId;
use uniterm_proto::{AgentToCore, CoreToAgent};
use uniterm_server::runtime::CoreLoop;

mod common;

#[test]
fn detects_agent_after_same_pid_exec_without_idle_work() {
    common::isolate_state();
    let root = common::temp_dir("provider-exec");
    std::env::set_var("XDG_CONFIG_HOME", root.join("config"));
    std::env::set_var("XDG_CACHE_HOME", root.join("cache"));
    let executable = root.join("codex");
    std::os::unix::fs::symlink("/bin/cat", &executable).unwrap();
    // Pass the target over stdin so it cannot match process evidence before exec.
    let mut child = Command::new("/bin/sh")
        .args([
            "-c",
            "printf 'LAUNCHER_READY\\n'; read target; exec \"$target\"",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id() as i32;
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    output.read_line(&mut line).unwrap();
    assert_eq!(line, "LAUNCHER_READY\n");
    let mut core = CoreLoop::new().unwrap();
    let evidence = |changed| CoreToAgent::PaneEvidence {
        pane: PaneId(1),
        foreground_pid: Some(pid),
        process_changed: changed,
        tail: "ready".into(),
        title: String::new(),
        bound_agent: None,
    };
    core.send_to_agent(evidence(true));
    // An ordered validation reply proves the initial miss was processed.
    core.send_to_agent(CoreToAgent::EditorSettingsValidate {
        client: 42,
        editor: "/bin/sh".into(),
        editor_rules: Vec::new(),
    });
    let replies = core.tick(Some(Duration::from_secs(5))).unwrap();
    assert!(matches!(
        replies.as_slice(),
        [AgentToCore::EditorSettingsValidated { .. }]
    ));
    writeln!(input, "{}", executable.display()).unwrap();
    writeln!(input, "AGENT_READY").unwrap();
    line.clear();
    output.read_line(&mut line).unwrap();
    assert_eq!(line, "AGENT_READY\n");
    core.send_to_agent(evidence(false));
    let replies = core.tick(Some(Duration::from_secs(5))).unwrap();
    assert!(
        matches!(replies.as_slice(), [AgentToCore::AgentDetected {
        foreground_pid: Some(found), agent: Some(agent), ..
    }] if *found == pid && agent == "codex"),
        "{replies:?}"
    );
    assert!(core
        .tick(Some(Duration::from_millis(100)))
        .unwrap()
        .is_empty());
    drop(input);
    assert!(child.wait().unwrap().success());
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}
