//! Resolve discovery and attachment in the same remote login environment.

use std::io;

/// A discovery result may follow noise from the host's outer SSH login shell.
pub(super) const CATALOG_READY: &[u8] = b"UNITERM-CATALOG/1\n";

fn command(arguments: &str, preamble: &str) -> String {
    // Real interactive shells can close descriptors above 2. The probe's fd 0
    // is the write end of a private pipe, while rc output goes to /dev/null.
    // The real protocol streams remain with the outer POSIX shell. Only its
    // own sh's replayable `export -p` output is evaluated, never rc stdout.
    const PROBE: &str = r#"vars=$(export -p); LC_ALL=C; printf "%s\n%s" "${#vars}" "$vars" >&0"#;
    const RUN: &str = r#"exec "$uniterm_ssh_sh" -c "$uniterm_ssh_probe""#;
    const CSH_RUN: &str =
        r#"if ( -r ~/.login ) source ~/.login; exec "$uniterm_ssh_sh" -c "$uniterm_ssh_probe""#;
    // Stop reading once the declared block is complete, even if a background
    // process started by the rc retained fd 0. Keep the original environment
    // as a fallback if the login shell exits before producing a complete one.
    let launch = format!(
        "TERM=${{TERM:-xterm-256color}}; uniterm_ssh_path=$PATH; \
         uniterm_ssh_sh=$(command -v sh); uniterm_ssh_probe=$1; \
         export TERM uniterm_ssh_probe uniterm_ssh_sh; \
         case ${{SHELL##*/}} in csh|tcsh) flags=-ic; run=$3;; *) flags=-lic; run=$2;; esac; \
         vars=$(\"${{SHELL:-$uniterm_ssh_sh}}\" \"$flags\" \"$run\" 0>&1 >/dev/null 2>&1 | \
            (IFS= read -r n && [ \"$n\" -ge 0 ] && [ \"$n\" -le 8388608 ] || exit 1; \
             LC_ALL=C; block=$(head -c \"$n\"); [ \"${{#block}}\" -eq \"$n\" ] && printf %s \"$block\")); \
         if [ -n \"$vars\" ]; then eval \"$vars\"; fi; \
         PATH=${{PATH:+$PATH:}}$uniterm_ssh_path; export PATH; \
         unset uniterm_ssh_path uniterm_ssh_sh uniterm_ssh_probe; \
         for binary in uniterm ut; do \
             if command -v \"$binary\" >/dev/null 2>&1; then {preamble}exec \"$binary\" {arguments}; fi; \
         done; \
         printf \"%s\\n\" \"Uniterm is not installed on the remote host (expected uniterm or ut on PATH)\" >&2; exit 127"
    );
    // Separate single-line arguments avoid nested quoting in the SSH account's
    // shell. All four scripts contain no single quotes, including the validated
    // Workspace name and fixed catalog marker.
    format!("exec sh -c '{launch}' uniterm-ssh '{PROBE}' '{RUN}' '{CSH_RUN}'")
}

/// Build the fixed remote bridge invocation and reject invalid Workspace names
/// before they become shell arguments.
pub fn bridge_command(workspace: Option<&str>) -> io::Result<String> {
    let mut args = format!(
        "remote-bridge --protocol {}",
        uniterm_proto::WIRE_PROTOCOL_VERSION
    );
    if let Some(name) = workspace {
        uniterm_proto::validate_workspace_name(name)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        args.push(' ');
        args.push_str(name);
    }
    Ok(command(&args, ""))
}

pub(super) fn catalog_command() -> String {
    command("machine-catalog", "printf \"UNITERM-CATALOG/1\\n\"; ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Stdio};

    #[test]
    fn ssh_bridge_and_catalog_keep_provider_path_and_isolate_login_output() {
        let dir = std::env::temp_dir().join(format!("ut-ssh-path-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let initial = dir.join("ssh");
        let profile = dir.join("profile");
        let rc = dir.join("rc");
        for bin in [&initial, &profile, &rc] {
            fs::create_dir_all(bin).unwrap();
        }
        let write_script = |path: &std::path::Path, script: &str| {
            fs::write(path, script).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        };
        let login = dir.join("login-shell");
        write_script(
            &login,
            r#"#!/bin/sh
test "$1" = -lic || exit 21
printf 'rc stdout\n'
printf 'rc job control diagnostic\n' >&2
IFS= read -r unexpected 2>/dev/null && exit 22
PATH=$TEST_PROFILE:$TEST_RC:/usr/bin:/bin
export PATH
export TEST_PROVIDER_AUTH="rc value with a quote: '"
exec /bin/sh -c "$2"
"#,
        );
        write_script(&rc.join("agent-cli"), "#!/bin/sh\nexit 0\n");
        // Cover each resolver independently, including an SSH-only binary
        // hidden by a login rc that replaces PATH and a profile-only alias.
        for (location, name) in [(&initial, "uniterm"), (&profile, "ut"), (&rc, "uniterm")] {
            let binary = location.join(name);
            write_script(
                &binary,
                r#"#!/bin/sh
command -v agent-cli >/dev/null || exit 23
test "$TEST_PROVIDER_AUTH" = "rc value with a quote: '" || exit 25
printf 'helper diagnostic\n' >&2
case $1 in
  machine-catalog) printf '{"protocol":%s,"workspaces":[]}\n' "$TEST_VERSION" ;;
  remote-bridge) IFS= read -r line && printf '%s\n%s\n' "$*" "$line" ;;
  *) exit 24 ;;
esac
"#,
            );
            for (remote, catalog) in [
                (bridge_command(Some("test.work")).unwrap(), false),
                (catalog_command(), true),
            ] {
                let mut child = Command::new("/bin/sh")
                    .args(["-c", &remote])
                    .env("SHELL", &login)
                    .env("PATH", format!("{}:/usr/bin:/bin", initial.display()))
                    .env("TEST_PROFILE", &profile)
                    .env("TEST_RC", &rc)
                    .env(
                        "TEST_VERSION",
                        uniterm_proto::WIRE_PROTOCOL_VERSION.to_string(),
                    )
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap();
                child
                    .stdin
                    .take()
                    .unwrap()
                    .write_all(b"wire input\n")
                    .unwrap();
                let output = child.wait_with_output().unwrap();
                assert!(output.status.success(), "{output:?}");
                assert_eq!(output.stderr, b"helper diagnostic\n");
                if catalog {
                    assert!(output.stdout.starts_with(CATALOG_READY));
                    let mut outer_banner = b"outer SSH banner\n".to_vec();
                    outer_banner.extend(output.stdout);
                    assert!(super::super::parse_reply(&outer_banner)
                        .unwrap()
                        .workspaces
                        .is_empty());
                } else {
                    let expected = format!(
                        "remote-bridge --protocol {} test.work\nwire input\n",
                        uniterm_proto::WIRE_PROTOCOL_VERSION
                    );
                    assert_eq!(output.stdout, expected.as_bytes());
                }
            }
            fs::remove_file(binary).unwrap();
        }
        assert!(bridge_command(Some("bad name")).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
