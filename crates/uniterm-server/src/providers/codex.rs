use super::{Manifest, Rule, RuleAnchor, RuleRegion};
use uniterm_core::AgentStatus;

// Priorities: a title marker beats any grid text (110 to 100), blocked
// prompts beat activity (90 to 85), activity is anchored to a spinner or a
// line start so typed prompt text cannot impersonate it (75 to 60), and idle
// hints are the weakest positive signal (20 to 10). No rule matching means
// idle: the matcher never treats output volume as evidence.
pub const MANIFEST: Manifest = Manifest {
    id: "codex",
    executables: &["codex"],
    rules: &[
        Rule {
            status: AgentStatus::Permission,
            patterns: &["action required"],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Title,
            priority: 110,
        },
        Rule {
            status: AgentStatus::Working,
            patterns: &[""],
            anchor: RuleAnchor::SpinnerLine,
            region: RuleRegion::Title,
            priority: 105,
        },
        Rule {
            status: AgentStatus::Permission,
            patterns: &[
                "would you like to run the following command?",
                "press enter to confirm",
                "allow codex to",
            ],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 90,
        },
        Rule {
            status: AgentStatus::Question,
            patterns: &["waiting for your response", "answer the question"],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 85,
        },
        Rule {
            status: AgentStatus::Error,
            patterns: &["stream disconnected", "usage limit", "failed to"],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 80,
        },
        Rule {
            status: AgentStatus::Working,
            patterns: &["working", "thinking"],
            anchor: RuleAnchor::SpinnerLine,
            region: RuleRegion::Bottom,
            priority: 75,
        },
        Rule {
            status: AgentStatus::Working,
            patterns: &["running command"],
            anchor: RuleAnchor::LineStart,
            region: RuleRegion::Bottom,
            priority: 60,
        },
        Rule {
            status: AgentStatus::Idle,
            patterns: &["codex>"],
            anchor: RuleAnchor::LineStart,
            region: RuleRegion::Bottom,
            priority: 20,
        },
        Rule {
            status: AgentStatus::Idle,
            patterns: &["ask codex"],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 10,
        },
    ],
};

/// Codex rollout headers declare identity and explicit spawned-thread ancestry.
pub(super) fn session_identity(line: &str) -> Option<super::sessions::Identity> {
    #[derive(serde::Deserialize)]
    struct Header {
        #[serde(rename = "type")]
        kind: String,
        payload: Metadata,
    }
    #[derive(serde::Deserialize)]
    struct Metadata {
        id: String,
        #[serde(default)]
        parent_thread_id: Option<String>,
        #[serde(default)]
        source: serde_json::Value,
    }
    // Unknown payload fields include long instructions. Deserialize only
    // identity fields, so prompts are neither retained nor projected.
    let record: Header = serde_json::from_str(line).ok()?;
    if record.kind != "session_meta" {
        return None;
    }
    let payload = record.payload;
    let id = payload.id;
    if id.is_empty() || id.len() > 512 || id.chars().any(char::is_control) {
        return None;
    }
    let parent = payload
        .parent_thread_id
        .as_deref()
        .or_else(|| {
            payload
                .source
                .pointer("/subagent/thread_spawn/parent_thread_id")
                .and_then(serde_json::Value::as_str)
        })
        .or_else(|| {
            payload
                .source
                .pointer("/subagent/thread_spawn/parent_session_id")
                .and_then(serde_json::Value::as_str)
        })
        .filter(|parent| {
            !parent.is_empty()
                && parent.len() <= 512
                && *parent != id
                && !parent.chars().any(char::is_control)
        })
        .map(str::to_owned);
    // Other subagent sources (review/compact) are not root TUI sessions.
    if payload.source.pointer("/subagent").is_some() && parent.is_none() {
        return None;
    }
    Some(super::sessions::Identity { id, parent })
}

pub(super) fn session_watch_root(path: &std::path::Path) -> Option<std::path::PathBuf> {
    path.ancestors()
        .find(|path| path.file_name().is_some_and(|name| name == "sessions"))
        .map(std::path::Path::to_owned)
}

/// Native storage root for an exact process, respecting per-invocation homes.
pub(super) fn session_root(pid: Option<i32>) -> Option<std::path::PathBuf> {
    #[cfg(target_os = "linux")]
    if let Some(pid) = pid {
        use std::io::Read;
        if let Ok(file) = std::fs::File::open(format!("/proc/{pid}/environ")) {
            let mut bytes = Vec::new();
            if file.take(128 * 1024).read_to_end(&mut bytes).is_ok() {
                if let Some(home) = bytes
                    .split(|byte| *byte == 0)
                    .find_map(|entry| entry.strip_prefix(b"CODEX_HOME="))
                {
                    use std::os::unix::ffi::OsStrExt;
                    return Some(
                        std::path::Path::new(std::ffi::OsStr::from_bytes(home)).join("sessions"),
                    );
                }
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = pid;
    std::env::var_os("CODEX_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".codex"))
        })
        .map(|home| home.join("sessions"))
}

/// Resolve only a known identity, never the newest file or shared cwd.
/// Header validation follows in the generic observer before publication.
pub(super) fn session_path(root: &std::path::Path, id: &str) -> Option<std::path::PathBuf> {
    if id.is_empty()
        || id.len() > 512
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return None;
    }
    let suffix = format!("-{id}.jsonl");
    let mut directories = vec![(root.to_owned(), 0)];
    while let Some((directory, depth)) = directories.pop() {
        let Ok(entries) = std::fs::read_dir(directory) else {
            continue;
        };
        let mut paths = Vec::new();
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().ends_with(&suffix) {
                return Some(entry.path());
            }
            if depth < 3 && entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                paths.push((entry.path(), depth + 1));
            }
        }
        paths.sort(); // Visit recent year/month/day directories first.
        directories.extend(paths);
    }
    None
}

#[cfg(test)]
mod session_tests {
    #[test]
    fn known_session_lookup_searches_past_large_history() {
        let root =
            std::env::temp_dir().join(format!("uniterm-session-lookup-{}", std::process::id()));
        let recent = root.join("2026/10/06");
        let older = root.join("2025/01/01");
        std::fs::create_dir_all(&recent).unwrap();
        std::fs::create_dir_all(&older).unwrap();
        for id in 0..4100 {
            std::fs::write(recent.join(format!("rollout-{id}.jsonl")), "").unwrap();
        }
        let expected = older.join("rollout-target.jsonl");
        std::fs::write(&expected, "").unwrap();
        assert_eq!(super::session_path(&root, "target"), Some(expected));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rollout_identity_preserves_explicit_parent_and_ignores_ephemeral_sessions() {
        let root = super::session_identity(
            r#"{"type":"session_meta","payload":{"id":"root","source":"cli"}}"#,
        )
        .unwrap();
        assert_eq!(root.id, "root");
        assert_eq!(root.parent, None);
        let child = super::session_identity(r#"{"type":"session_meta","payload":{"id":"child","source":{"subagent":{"thread_spawn":{"parent_thread_id":"root"}}}}}"#).unwrap();
        assert_eq!(child.parent.as_deref(), Some("root"));
        assert!(super::session_identity(
            r#"{"type":"session_meta","payload":{"id":"review","source":{"subagent":"review"}}}"#
        )
        .is_none());
        assert!(
            super::session_identity(r#"{"type":"response_item","payload":{"id":"guess"}}"#)
                .is_none()
        );
    }
}
