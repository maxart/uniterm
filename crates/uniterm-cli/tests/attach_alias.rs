//! Attach aliases must dispatch identically, never as bare Workspace names.

use std::path::Path;
use std::process::{Command, Output, Stdio};

mod common;

fn invoke(binary: &str, args: &[&str], state: &Path, root: &Path) -> Output {
    Command::new(binary)
        .args(args)
        .env("XDG_STATE_HOME", state)
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .stdin(Stdio::null())
        .output()
        .expect("run CLI with isolated state")
}

#[test]
fn attach_aliases_share_target_defaults_and_validation() {
    let state = common::isolate_state();
    let root = common::temp_dir("attach-alias");
    let missing = common::unique_workspace_name();
    for binary in [env!("CARGO_BIN_EXE_ut"), env!("CARGO_BIN_EXE_uniterm")] {
        for (target, code) in [
            (None, 1),
            (Some(missing.as_str()), 1),
            (Some("../invalid"), 2),
        ] {
            let mut args = vec!["attach"];
            args.extend(target);
            let expected = invoke(binary, &args, &state, &root);
            assert_eq!(expected.status.code(), Some(code));
            assert!(String::from_utf8_lossy(&expected.stderr).starts_with("uniterm attach:"));
            for alias in ["att", "a"] {
                args[0] = alias;
                let actual = invoke(binary, &args, &state, &root);
                assert_eq!(actual.status.code(), expected.status.code(), "{args:?}");
                assert_eq!(actual.stdout, expected.stdout, "{args:?}");
                assert_eq!(actual.stderr, expected.stderr, "{args:?}");
            }
        }
        for args in [&["--help"][..], &["help", "att"][..]] {
            let help = invoke(binary, args, &state, &root);
            assert!(help.status.success());
            assert!(String::from_utf8_lossy(&help.stdout).contains("aliases: att, a"));
        }
    }
    assert!(
        std::fs::read_dir(&state).unwrap().next().is_none(),
        "attach must not create Workspace state"
    );
    std::fs::remove_dir_all(root).unwrap();
}
