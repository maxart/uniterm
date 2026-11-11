//! On-demand native model discovery. No prompts, background refresh, or credentials on the wire.
use std::{path::Path, process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use uniterm_core::agent_models::ModelCatalog;

mod claude;
mod codex;
mod grok_cursor;
mod opencode;
mod pi;

const LIMIT: u64 = 16 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(20);

/// Discover only on explicit user request, preserving adapter provenance.
pub(crate) async fn discover(
    provider: &str,
    command: &str,
    cwd: &Path,
    refresh: bool,
) -> ModelCatalog {
    let result = match provider {
        "codex" => codex::discover(command, cwd, refresh).await,
        "claude" => claude::discover(command, cwd).await,
        "pi" => pi::discover(command, cwd).await,
        "opencode" => opencode::discover(command, cwd).await,
        "grok" | "cursor" => grok_cursor::discover(provider, command, cwd).await,
        _ => Err("This harness does not expose a supported model catalog. Use its native settings or a custom model ID.".into()),
    };
    let mut catalog = result.unwrap_or_else(|notice| ModelCatalog {
        notice,
        ..Default::default()
    });
    catalog.provider = provider.into();
    catalog.models.truncate(256);
    catalog.notice = clean(&catalog.notice, 512);
    for model in &mut catalog.models {
        model.label = clean(&model.label, 128);
        model.description = clean(&model.description, 256);
        model.aliases.retain(|s| valid_id(s));
        model.aliases.truncate(8);
        model.efforts.retain(|e| valid_id(&e.id));
        model.efforts.truncate(32);
        for effort in &mut model.efforts {
            effort.label = clean(&effort.label, 80);
            effort.description = clean(&effort.description, 160);
        }
    }
    catalog.models.retain(|m| valid_id(&m.id));
    catalog
}

fn clean(value: &str, limit: usize) -> String {
    value
        .chars()
        .filter(|c| !c.is_control())
        .take(limit)
        .collect()
}
fn valid_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}

fn command(command: &str, args: &[&str], cwd: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(command);
    cmd.args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Never return diagnostics that might contain tokens or configured headers.
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .env("GROK_DISABLE_AUTOUPDATER", "1");
    // Every discovery process owns a group, including any helper descendants.
    unsafe {
        cmd.pre_exec(|| {
            // SAFETY: setsid is async-signal-safe and accesses no shared Rust state.
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd
}

struct Group(u32);
impl Drop for Group {
    fn drop(&mut self) {
        // SAFETY: the child created this private session; no caller-controlled pid is used.
        unsafe {
            libc::kill(-(self.0 as i32), libc::SIGKILL);
        }
    }
}

async fn stop(child: &mut tokio::process::Child, group: Group) {
    // SAFETY: this is the session created for this metadata subprocess only.
    unsafe {
        libc::kill(-(group.0 as i32), libc::SIGTERM);
    }
    let _ = tokio::time::timeout(Duration::from_millis(150), child.wait()).await;
    // Kill any descendants that ignored TERM, even if the group leader exited.
    drop(group);
    let _ = child.wait().await;
}

/// Bounded noninteractive metadata command; never exposes raw subprocess errors.
async fn run(
    command_name: &str,
    args: &[&str],
    cwd: &Path,
    input: Option<&str>,
) -> Result<String, String> {
    let mut child = command(command_name, args, cwd)
        .spawn()
        .map_err(|_| "Could not start native model discovery".to_string())?;
    let group = Group(
        child
            .id()
            .ok_or("Model discovery has no process identity")?,
    );
    let result = tokio::time::timeout(TIMEOUT, async {
        if let Some(mut stdin) = child.stdin.take() {
            if let Some(input) = input {
                stdin
                    .write_all(input.as_bytes())
                    .await
                    .map_err(|_| "Could not send model discovery request")?;
            }
        }
        let mut bytes = Vec::new();
        child
            .stdout
            .take()
            .ok_or("Missing discovery output")?
            .take(LIMIT + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| "Could not read model discovery")?;
        if bytes.len() as u64 > LIMIT {
            return Err("Model discovery response exceeded its limit");
        }
        let status = child
            .wait()
            .await
            .map_err(|_| "Could not reap model discovery")?;
        if !status.success() {
            return Err("Native model discovery failed; check harness sign-in and configuration");
        }
        String::from_utf8(bytes).map_err(|_| "Native model catalog was not UTF-8")
    })
    .await
    .map_err(|_| "Model discovery timed out; retry or use a custom model ID".to_string())
    .and_then(|r| r.map_err(str::to_string));
    stop(&mut child, group).await;
    result
}

/// Perform a bounded metadata-only JSON-lines exchange without closing stdin early.
async fn rpc(
    command_name: &str,
    args: &[&str],
    cwd: &Path,
    steps: &[(String, Option<(String, String)>)],
) -> Result<serde_json::Value, String> {
    let mut child = command(command_name, args, cwd)
        .spawn()
        .map_err(|_| "Could not start native model discovery".to_string())?;
    let group = Group(
        child
            .id()
            .ok_or("Model discovery has no process identity")?,
    );
    let result = tokio::time::timeout(TIMEOUT, async {
        let mut stdin = child.stdin.take().ok_or("Missing discovery input")?;
        let mut stdout = BufReader::new(
            child
                .stdout
                .take()
                .ok_or("Missing discovery output")?
                .take(LIMIT + 1),
        );
        let mut used = 0usize;
        let mut last = serde_json::Value::Null;
        for (request, expected) in steps {
            stdin
                .write_all(request.as_bytes())
                .await
                .map_err(|_| "Could not send model request")?;
            stdin
                .write_all(b"\n")
                .await
                .map_err(|_| "Could not send model request")?;
            let Some((key, id)) = expected else {
                continue;
            };
            loop {
                let mut line = String::new();
                let n = stdout
                    .read_line(&mut line)
                    .await
                    .map_err(|_| "Could not read model response")?;
                used += n;
                if n == 0 || used as u64 > LIMIT {
                    return Err("Native model discovery returned no valid bounded response");
                }
                let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                    continue;
                };
                let candidate = if key.starts_with('/') {
                    value.pointer(key)
                } else {
                    value.get(key)
                };
                if candidate.is_some_and(|v| {
                    v.as_str() == Some(id.as_str())
                        || v.as_u64()
                            .zip(id.parse::<u64>().ok())
                            .is_some_and(|(actual, expected)| actual == expected)
                }) {
                    last = value;
                    break;
                }
            }
        }
        Ok(last)
    })
    .await
    .map_err(|_| "Model discovery timed out; retry or use a custom model ID".to_string())
    .and_then(|r: Result<_, &str>| r.map_err(str::to_string));
    stop(&mut child, group).await;
    result
}

/// Validate an identifier before its runtime-discovered launch mode is available.
pub(crate) fn validate_model(provider: &str, model: &str) -> Result<(), String> {
    if !valid_id(model) {
        return Err("Invalid model identifier".into());
    }
    if provider == "opencode" {
        return Ok(());
    }
    super::resume_model_argv(provider, &[provider.into()], Some(model)).map(|_| ())
}

/// Apply model and effort through provider-owned syntax on launch and native resume.
pub(crate) fn selection_argv(
    provider: &str,
    argv: &[String],
    model: Option<&str>,
    effort: Option<&str>,
    mode: Option<&str>,
) -> Result<Vec<String>, String> {
    if model.is_some_and(|v| !valid_id(v)) || effort.is_some_and(|v| !valid_id(v)) {
        return Err("Invalid model or effort identifier".into());
    }
    if provider == "pi" {
        return pi::selection_argv(argv, model, effort);
    }
    if provider == "opencode" {
        return opencode::selection_argv_for_mode(argv, model, effort, mode.unwrap_or_default());
    }
    let mut result = super::resume_model_argv(provider, argv, model)?;
    let Some(effort) = effort else {
        return Ok(result);
    };
    let options = match provider {
        "codex" => codex::effort_args(effort)?,
        "claude" => claude::effort_args(effort)?,
        "grok" => vec![format!("--reasoning-effort={effort}")],
        "pi" => vec![format!("--thinking={effort}")],
        _ => return Err(
            "This harness has no separate effort override; choose a native model variant instead"
                .into(),
        ),
    };
    // Effort values are native identifiers, never TOML or shell fragments.
    if !effort
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err("Invalid effort identifier".into());
    }
    // Replace prior native effort arguments while preserving prompts after `--`.
    let mut cleaned = Vec::with_capacity(result.len() + options.len());
    let mut args = result.drain(..).peekable();
    while let Some(arg) = args.next() {
        if arg == "--" {
            cleaned.push(arg);
            cleaned.extend(args);
            break;
        }
        let flag = match provider {
            "claude" => "--effort",
            "grok" => "--reasoning-effort",
            "pi" => "--thinking",
            _ => "",
        };
        if !flag.is_empty() && (arg == flag || (provider == "grok" && arg == "--effort")) {
            args.next();
            continue;
        }
        if !flag.is_empty()
            && (arg.starts_with(&format!("{flag}="))
                || (provider == "grok" && arg.starts_with("--effort=")))
        {
            continue;
        }
        if provider == "codex"
            && (arg == "-c" || arg == "--config")
            && args
                .peek()
                .is_some_and(|s| s.starts_with("model_reasoning_effort="))
        {
            args.next();
            continue;
        }
        if provider == "codex"
            && (arg.starts_with("--config=model_reasoning_effort=")
                || arg.starts_with("-cmodel_reasoning_effort=")
                || arg.starts_with("-c=model_reasoning_effort="))
        {
            continue;
        }
        cleaned.push(arg);
    }
    if cleaned.is_empty() {
        return Err("Empty native launch command".into());
    }
    cleaned.splice(1..1, options);
    Ok(cleaned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_model_and_effort_replace_saved_native_arguments() {
        let codex = [
            "codex",
            "resume",
            "saved-session",
            "--model=old",
            "-m",
            "older",
            "-moldest",
            "--config=model_reasoning_effort=\"low\"",
            "-cmodel_reasoning_effort=\"high\"",
            "--",
            "literal --effort=low",
        ]
        .map(String::from);
        let selected =
            selection_argv("codex", &codex, Some("gpt-6-luna"), Some("medium"), None).unwrap();
        assert!(selected
            .windows(2)
            .any(|w| w == ["-c", "model_reasoning_effort=\"medium\""]));
        assert_eq!(
            selected
                .iter()
                .filter(|s| s.contains("model_reasoning_effort"))
                .count(),
            1
        );
        assert!(selected.iter().any(|s| s == "--model=gpt-6-luna"));
        assert!(selected.iter().any(|s| s == "saved-session"));
        assert_eq!(selected.last().unwrap(), "literal --effort=low");
        assert_eq!(
            selection_argv("codex", &selected, Some("gpt-6-luna"), Some("medium"), None).unwrap(),
            selected
        );
        for (provider, flag) in [
            ("claude", "--effort"),
            ("grok", "--reasoning-effort=medium"),
        ] {
            let result = selection_argv(
                provider,
                &[provider.into(), "--resume=session".into()],
                Some("model"),
                Some("medium"),
                None,
            )
            .unwrap();
            assert!(result.iter().any(|arg| arg == flag));
            assert!(result.iter().any(|arg| arg == "--resume=session"));
        }
        assert!(selection_argv(
            "cursor",
            &["agent".into()],
            Some("native-thinking-variant"),
            Some("high"),
            None
        )
        .is_err());
        assert!(selection_argv(
            "codex",
            &["codex".into()],
            Some("model"),
            Some("high\";bad"),
            None
        )
        .is_err());
        assert_eq!(
            selection_argv("codex", &codex, None, None, None).unwrap(),
            codex
        );
    }

    #[tokio::test]
    async fn metadata_exchange_keeps_stdin_open_until_response_and_reaps_child() {
        let script = "read first; printf '%s\\n' '{\"id\":1,\"result\":{}}'; read second; printf '%s\\n' '{\"id\":2,\"result\":{\"models\":[]}}'; read remain";
        let value = rpc(
            "/bin/sh",
            &["-c", script],
            Path::new("/tmp"),
            &[
                ("first".into(), Some(("id".into(), "1".into()))),
                ("second".into(), Some(("id".into(), "2".into()))),
            ],
        )
        .await
        .unwrap();
        assert_eq!(value["id"], 2);
        assert_eq!(value["result"]["models"], serde_json::json!([]));
        let output = run(
            "/bin/sh",
            &["-c", "cat"],
            Path::new("/tmp"),
            Some("metadata\n"),
        )
        .await
        .unwrap();
        assert_eq!(output, "metadata\n");
    }
}
