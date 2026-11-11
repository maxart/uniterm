//! Structured collaboration without scraping or injecting terminal conversations.
use std::io::{BufRead as _, Read as _, Write as _};
use std::os::unix::net::UnixStream;
use uniterm_core::inbox::{Author, Kind, Operation, TTL_MS};
use uniterm_proto::{
    ControlCommand, ControlFrame, ControlRequest, ControlResult, InboxIdentity, PaneId, ProjectId,
};

fn exchange(
    reader: &mut std::io::BufReader<UnixStream>,
    workspace: &str,
    command: ControlCommand,
) -> Result<ControlResult, String> {
    let request = ControlRequest {
        version: uniterm_proto::CONTROL_API_VERSION,
        id: 1,
        workspace: workspace.into(),
        command,
    };
    writeln!(
        reader.get_mut(),
        "{}",
        serde_json::to_string(&request).map_err(|e| e.to_string())?
    )
    .map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    reader
        .by_ref()
        .take(u64::from(uniterm_proto::CONTROL_MAX_FRAME_BYTES) + 1)
        .read_until(b'\n', &mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > uniterm_proto::CONTROL_MAX_FRAME_BYTES as usize || !bytes.ends_with(b"\n") {
        return Err(
            "invalid or closed inbox response; outcome unknown, retry posts with the same key"
                .into(),
        );
    }
    match serde_json::from_slice::<ControlFrame>(&bytes).map_err(|e| e.to_string())? {
        ControlFrame::Response(response) => response.result.ok_or_else(|| {
            response.error.map_or("missing response".into(), |e| {
                format!("{}: {}", e.code, e.message)
            })
        }),
        _ => Err("unexpected inbox frame".into()),
    }
}
fn value(args: &[String], index: &mut usize) -> Result<String, String> {
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or("missing option value".into())
}
fn number(value: &str) -> Result<u64, String> {
    value
        .parse()
        .map_err(|_| format!("expected non-negative integer: {value}"))
}

pub(crate) fn command(args: &[String]) -> i32 {
    match run(args) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("uniterm inbox: {error}");
            1
        }
    }
}
fn run(args: &[String]) -> Result<(), String> {
    let (socket, args) = super::project_socket(args)?;
    let verb = args.first().map_or("discover", String::as_str);
    if matches!(verb, "help" | "--help" | "-h")
        || args
            .iter()
            .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        println!("ut inbox discover|list|watch|post|ack ID|claim ID|release ID|pause|resume|clear|revoke PANE INVOCATION\nOptions: --project ID --json --after CURSOR --epoch EPOCH --thread ID --timeout SECONDS\nPost: --type plan|proposal|critique|feedback|question|answer|finding|request|handoff|decision|blocked --key KEY [--reply-to ID] [--to PANE:INVOCATION] [--task ID] [--ttl SECONDS] --stdin\nClaim/release: --revision N [--lease SECONDS]. Mutations referring to old IDs require --epoch.\nInside a Pane, identity and Project are automatic. Human administration: --human. Clear: --confirm.\nDetached backend setup: capture `ut inbox credential` inside the live invocation into UNITERM_INBOX_IDENTITY; never print or post this secret.
Read ut --skill monitoring for collaboration guidance. Bodies expire and are lost on restart.");
        return Ok(());
    }
    let mut project = None;
    let mut epoch = None;
    let mut after = 0;
    let mut thread = None;
    let mut kind = Kind::Finding;
    let mut key = None;
    let mut reply_to = None;
    let mut to = None;
    let mut task = None;
    let mut ttl_ms = TTL_MS;
    let mut timeout_ms = 300_000;
    let mut revision = None;
    let mut lease_ms = 900_000;
    let mut json = false;
    let mut stdin = false;
    let mut human = false;
    let mut confirmed = false;
    let mut positional = Vec::new();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--project" => project = Some(ProjectId(number(&value(&args, &mut i)?)?)),
            "--epoch" => epoch = Some(number(&value(&args, &mut i)?)?),
            "--after" => after = number(&value(&args, &mut i)?)?,
            "--thread" => thread = Some(number(&value(&args, &mut i)?)?),
            "--reply-to" => reply_to = Some(number(&value(&args, &mut i)?)?),
            "--task" => task = Some(number(&value(&args, &mut i)?)?),
            "--revision" => revision = Some(number(&value(&args, &mut i)?)?),
            "--key" => key = Some(value(&args, &mut i)?),
            "--type" => {
                kind = serde_json::from_value(serde_json::Value::String(value(&args, &mut i)?))
                    .map_err(|_| "unknown message type")?
            }
            "--to" => {
                let v = value(&args, &mut i)?;
                let (pane, inv) = v.split_once(':').ok_or("--to requires PANE:INVOCATION")?;
                to = Some(Author::Agent {
                    pane: PaneId(number(pane)?),
                    invocation: number(inv)?,
                });
            }
            "--ttl" => {
                ttl_ms = number(&value(&args, &mut i)?)?
                    .checked_mul(1000)
                    .ok_or("TTL overflow")?
            }
            "--timeout" => {
                timeout_ms = number(&value(&args, &mut i)?)?
                    .checked_mul(1000)
                    .ok_or("timeout overflow")?
            }
            "--lease" => {
                lease_ms = number(&value(&args, &mut i)?)?
                    .checked_mul(1000)
                    .ok_or("lease overflow")?
            }
            "--json" => json = true,
            "--stdin" => stdin = true,
            "--human" => human = true,
            "--confirm" => confirmed = true,
            flag if flag.starts_with('-') => return Err(format!("unknown option {flag}")),
            arg => positional.push(arg.to_owned()),
        }
        i += 1;
    }
    let id = || {
        positional
            .first()
            .ok_or_else(|| "message ID required".into())
            .and_then(|v| number(v))
    };
    let operation = match verb {
        "discover" => Operation::Discover,
        "credential" => Operation::Credential,
        "list" => Operation::List {
            after,
            thread,
            limit: 50,
        },
        "watch" => Operation::Watch { after, timeout_ms },
        "post" => {
            let body = if stdin {
                if !positional.is_empty() {
                    return Err("choose text arguments or --stdin".into());
                }
                let mut body = String::new();
                std::io::stdin()
                    .take(uniterm_core::inbox::BODY_LIMIT as u64 + 1)
                    .read_to_string(&mut body)
                    .map_err(|e| e.to_string())?;
                body
            } else {
                positional.join(" ")
            };
            Operation::Post {
                kind,
                body,
                reply_to,
                to,
                task,
                key: key.ok_or("post requires a stable --key for safe retries")?,
                ttl_ms,
            }
        }
        "ack" => Operation::Ack { id: id()? },
        "claim" => Operation::Claim {
            id: id()?,
            revision: revision.ok_or("claim requires --revision from list")?,
            lease_ms,
        },
        "release" => Operation::Release {
            id: id()?,
            revision: revision.ok_or("release requires --revision from list")?,
        },
        "pause" => Operation::Pause { paused: true },
        "resume" => Operation::Pause { paused: false },
        "clear" if confirmed => Operation::Clear { confirmed: true },
        "revoke" if positional.len() == 2 => Operation::Revoke {
            author: Author::Agent {
                pane: PaneId(number(&positional[0])?),
                invocation: number(&positional[1])?,
            },
        },
        _ => return Err("run ut inbox help; clear requires --confirm".into()),
    };
    let inherited = if verb != "credential" && !human {
        std::env::var("UNITERM_INBOX_IDENTITY")
            .ok()
            .filter(|s| !s.is_empty())
            .map(|s| {
                serde_json::from_str::<InboxIdentity>(&s)
                    .map_err(|_| "invalid inherited inbox identity".to_string())
            })
            .transpose()?
    } else {
        None
    };
    let pane = if human {
        None
    } else {
        std::env::var("UNITERM_PANE_ID")
            .ok()
            .map(|s| number(&s).map(PaneId))
            .transpose()?
            .or_else(|| inherited.as_ref().map(|identity| identity.pane))
    };
    if inherited
        .as_ref()
        .is_some_and(|identity| Some(identity.pane) != pane)
    {
        return Err("inherited inbox identity belongs to a different Pane".into());
    }
    let references_previous = matches!(
        operation,
        Operation::Ack { .. }
            | Operation::Claim { .. }
            | Operation::Release { .. }
            | Operation::Clear { .. }
            | Operation::Revoke { .. }
    ) || reply_to.is_some()
        || after != 0
        || thread.is_some();
    if references_previous && epoch.is_none() {
        return Err(
            "--epoch from discover/list is required when referencing prior messages or cursors"
                .into(),
        );
    }
    let control = socket.with_extension("control.sock");
    let control = if pane.is_some() {
        control
            .parent()
            .ok_or("invalid socket")?
            .join("inbox")
            .join(control.file_name().ok_or("invalid socket")?)
    } else {
        control
    };
    let stream = UnixStream::connect(control)
        .map_err(|e| format!("{e}; inbox requires an updated running server"))?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(std::time::Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    let mut reader = std::io::BufReader::new(stream);
    let workspace = socket
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or("invalid Workspace socket")?;
    let identity = if let Some(identity) = inherited {
        Some(identity)
    } else if let Some(pane) = pane {
        match exchange(
            &mut reader,
            workspace,
            ControlCommand::AgentHookContext { pane },
        )? {
            ControlResult::AgentHookContext {
                pane,
                foreground_pid: Some(foreground_pid),
                invocation: Some(invocation),
            } => Some(InboxIdentity {
                capability: None,
                capability_project: None,
                pane,
                foreground_pid,
                invocation,
            }),
            _ => {
                return Err(
                    "this Pane has no live recognized agent; humans may explicitly use --human"
                        .into(),
                )
            }
        }
    } else {
        None
    };
    reader
        .get_mut()
        .set_read_timeout(Some(std::time::Duration::from_millis(
            timeout_ms.min(300_000) + 5000,
        )))
        .map_err(|e| e.to_string())?;
    let result = exchange(
        &mut reader,
        workspace,
        ControlCommand::Inbox {
            project,
            identity: identity.clone(),
            epoch,
            operation,
        },
    )?;
    if let ControlResult::InboxCredential { project, token } = result {
        if verb != "credential" {
            return Err("unexpected credential response".into());
        }
        let mut identity = identity.ok_or("credentials require an agent Pane")?;
        identity.capability = Some(token);
        identity.capability_project = Some(project);
        println!(
            "{}",
            serde_json::to_string(&identity).map_err(|e| e.to_string())?
        );
        return Ok(());
    }
    if json {
        println!(
            "{}",
            serde_json::to_string(&result).map_err(|e| e.to_string())?
        );
    } else if let ControlResult::Inbox {
        epoch,
        project,
        author,
        page,
        id,
        guidance,
    } = result
    {
        println!(
            "Project {} inbox | epoch {} | cursor {} | paused {} | author {:?}",
            project.0, epoch, page.cursor, page.paused, author
        );
        if let Some(id) = id {
            println!("message {id}");
        }
        for entry in page.entries {
            let m = entry.message;
            println!(
                "#{} thread={} {:?} {:?} revision={} claim={:?}\n{}",
                m.id,
                m.thread,
                m.author,
                m.kind,
                entry.revision,
                entry.claim,
                m.body
                    .chars()
                    .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
                    .collect::<String>()
            );
        }
        if page.more {
            println!(
                "More results: use --after {} --epoch {}",
                page.cursor, epoch
            );
        }
        if verb == "discover" {
            println!("{guidance}");
        }
    } else {
        return Err("unexpected server response".into());
    }
    Ok(())
}
