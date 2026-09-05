//! End-to-end contract for event-driven provider manifest reload and
//! structured detection provenance.

use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use uniterm_core::{AgentStatus, PaneId};
use uniterm_proto::{encode_frame, ClientMessage, DetectionSource, FrameDecoder, ServerMessage};
use uniterm_server::Server;

mod common;

use common::{isolate_state, unique_workspace_name};

fn temp_root() -> PathBuf {
    let nonce = common::unique_nonce();
    common::socket_root().join(format!(
        "uniterm-provider-reload-{}-{nonce}",
        std::process::id()
    ))
}

fn wait_for(path: &Path) {
    for _ in 0..300 {
        if path.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("socket never appeared at {}", path.display());
}

fn manifest(version: &str, status: &str) -> String {
    format!(
        r#"{{
  "schema_version": 1,
  "manifest_version": "{version}",
  "providers": [{{
    "id": "reload-test",
    "executable_aliases": ["reload-agent"],
    "capabilities": ["process", "screen"],
    "rules": [{{
      "id": "screen.custom-waiting",
      "evidence": "screen",
      "status": "{status}",
      "pattern": "custom waiting",
      "confidence": 97,
      "dwell_ms": 0
    }}]
  }}]
}}"#
    )
}

fn replace_manifest(path: &Path, text: &str) {
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, text).unwrap();
    std::fs::rename(temporary, path).unwrap();
}

struct Wire {
    stream: UnixStream,
    decoder: FrameDecoder,
}

impl Wire {
    fn connect(path: &Path) -> Self {
        let stream = UnixStream::connect(path).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        Self {
            stream,
            decoder: FrameDecoder::new(),
        }
    }

    fn explain_until(
        &mut self,
        status: AgentStatus,
        version: &str,
    ) -> uniterm_proto::AgentDetectionInfo {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut buffer = [0u8; 16 * 1024];
        let mut last = None;
        loop {
            self.stream
                .write_all(&encode_frame(&ClientMessage::AgentExplain {
                    pane: Some(PaneId(1)),
                }))
                .unwrap();
            loop {
                while let Some(message) = self.decoder.decode::<ServerMessage>().unwrap() {
                    if let ServerMessage::AgentExplanation { entries } = message {
                        if let Some(entry) = entries.into_iter().next() {
                            if entry.status == status
                                && entry.provenance.manifest_version.as_deref() == Some(version)
                            {
                                return entry;
                            }
                            last = Some(entry);
                        }
                        break;
                    }
                }
                match self.stream.read(&mut buffer) {
                    Ok(0) => panic!("server closed the provider test connection"),
                    Ok(read) => self.decoder.push(&buffer[..read]),
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) =>
                    {
                        break;
                    }
                    Err(error) => panic!("provider explain read failed: {error}"),
                }
            }
            assert!(
                Instant::now() < deadline,
                "manifest reload did not become visible; last explanation: {last:?}"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
}

#[test]
fn local_manifest_reload_reclassifies_current_grid_without_pty_activity() {
    isolate_state();
    let root = temp_root();
    let config_home = root.join("config");
    let cache_home = root.join("cache");
    let state_home = root.join("state");
    std::fs::create_dir_all(&root).unwrap();
    let previous_config = std::env::var_os("XDG_CONFIG_HOME");
    let previous_cache = std::env::var_os("XDG_CACHE_HOME");
    let previous_state = std::env::var_os("XDG_STATE_HOME");
    std::env::set_var("XDG_CONFIG_HOME", &config_home);
    std::env::set_var("XDG_CACHE_HOME", &cache_home);
    std::env::set_var("XDG_STATE_HOME", &state_home);

    let socket = root.join(format!("{}.sock", unique_workspace_name()));
    let executable = root.join("reload-agent");
    std::fs::write(
        &executable,
        "#!/bin/sh\ni=0; while [ $i -lt 15 ]; do printf '\\r\\n'; i=$((i + 1)); done; printf 'custom waiting'; while IFS= read -r line; do :; done\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    let server_socket = socket.clone();
    let server_executable = executable.clone();
    let server = thread::spawn(move || {
        let (mut server, mut poll) = Server::bind(
            &server_socket,
            server_executable.to_str().unwrap(),
            &[],
            80,
            24,
        )
        .unwrap();
        let _ = server.run(&mut poll);
    });
    wait_for(&socket);
    let manifest_path = config_home.join("uniterm/providers.json");
    wait_for(manifest_path.parent().unwrap());
    let mut wire = Wire::connect(&socket);

    replace_manifest(&manifest_path, &manifest("local-v1", "permission"));
    let first = wire.explain_until(AgentStatus::Permission, "local-v1");
    assert_eq!(first.agent.as_deref(), Some("reload-test"));
    assert_eq!(first.provenance.source, DetectionSource::LocalOverride);
    assert_eq!(
        first.provenance.matched_rule.as_deref(),
        Some("screen.custom-waiting")
    );
    assert_eq!(first.provenance.confidence, Some(97));
    assert_eq!(first.provenance.dwell_ms, Some(0));
    assert_eq!(first.provenance.invocation_pid, first.foreground_pid);

    // The pane emits no more output. Reclassification therefore proves the
    // notify edge caused a validated reload and a core-owned grid resnapshot.
    replace_manifest(&manifest_path, &manifest("local-v2", "error"));
    let second = wire.explain_until(AgentStatus::Error, "local-v2");
    assert!(second.provenance.evidence_timestamp_ms >= first.provenance.evidence_timestamp_ms);

    wire.stream
        .write_all(&encode_frame(&ClientMessage::KillServer))
        .unwrap();
    server.join().unwrap();

    match previous_config {
        Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
        None => std::env::remove_var("XDG_CONFIG_HOME"),
    }
    match previous_cache {
        Some(value) => std::env::set_var("XDG_CACHE_HOME", value),
        None => std::env::remove_var("XDG_CACHE_HOME"),
    }
    match previous_state {
        Some(value) => std::env::set_var("XDG_STATE_HOME", value),
        None => std::env::remove_var("XDG_STATE_HOME"),
    }
    let _ = std::fs::remove_dir_all(root);
}
