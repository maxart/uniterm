//! Structured collaboration without scraping or injecting terminal conversations.
use std::io::{BufRead as _, Read as _, Write as _};
use std::os::unix::net::UnixStream;
use uniterm_core::inbox::{
    Author, Kind, Operation, PostDraft, PAGE_LIMIT, PROJECT_LIMIT, THREAD_LIMIT,
};
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
fn message_id(value: &str) -> Result<u64, String> {
    let id = number(value.strip_prefix("thread:").unwrap_or(value))?;
    if id == 0 {
        return Err("message IDs start at 1".into());
    }
    Ok(id)
}
fn number(value: &str) -> Result<u64, String> {
    value
        .parse()
        .map_err(|_| format!("expected non-negative integer: {value}"))
}

// Old servers filter only by root ID. When handed a comment, resolve its
// root from the Project's bounded change feed using this same authorization.
fn legacy_thread(
    id: u64,
    request: &mut impl FnMut(Operation) -> Result<ControlResult, String>,
) -> Result<(u64, ControlResult), String> {
    let mut list = |after, thread| {
        request(Operation::List {
            after,
            thread,
            limit: PAGE_LIMIT,
        })
    };
    let first = list(0, Some(id))?;
    if matches!(&first, ControlResult::Inbox {page, ..} if !page.entries.is_empty()) {
        return Ok((id, first));
    }
    let mut after = 0;
    for _ in 0..PROJECT_LIMIT.div_ceil(PAGE_LIMIT) {
        let ControlResult::Inbox { page, .. } = list(after, None)? else {
            return Err("unexpected Inbox page".into());
        };
        if let Some(message) = page.entries.iter().map(|e| &e.message).find(|m| m.id == id) {
            let root = message.thread;
            return Ok((root, list(0, Some(root))?));
        }
        if !page.more || page.cursor <= after {
            break;
        }
        after = page.cursor;
    }
    Err("thread not found in this Project".into())
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
    let options_end = args
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(args.len());
    let (socket, mut options) = super::project_socket(&args[..options_end])?;
    options.extend_from_slice(&args[options_end..]);
    let args = options;
    let verb = args.first().map_or("discover", String::as_str);
    if matches!(verb, "help" | "--help" | "-h")
        || args
            .iter()
            .take_while(|arg| arg.as_str() != "--")
            .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        println!("ut inbox discover|credential|list|read ID|post|reply ID|comment ID|watch|ack ID|claim ID|release ID|pause|resume|clear|revoke PANE INVOCATION\nOptions: --project ID --json --after CURSOR --epoch EPOCH --thread ID --timeout SECONDS\nRead: read MESSAGE (or thread:ID) --epoch EPOCH returns the whole conversation in posting order.\nPost or reply: --type plan|proposal|critique|feedback|question|answer|finding|request|handoff|decision|blocked --key KEY [--reply-to ID] [--ref THREAD]... [--to PANE:INVOCATION] [--task ID] [--ttl SECONDS] TEXT | --stdin (use -- before option-like body text)\nPosts default to finding, and replies and comments default to feedback. Any Project agent can comment on a root or reply. Replies expire with their parent. On thread_budget (100 posts), start a new post and cite the old thread. Inline thread:ID is a display link when available. Use --ref MESSAGE with --epoch for a validated citation, including from a reply to its root.\nClaim/release: --revision N [--lease SECONDS]. Mutations referring to old IDs require --epoch.\nInside a Pane, identity and Project are automatic. Human administration: --human. Clear: --confirm.\nDetached backend setup: capture `ut inbox credential` inside the live invocation into UNITERM_INBOX_IDENTITY; never print or post this secret.
Read ut --skill monitoring for collaboration guidance. Posts follow the Workspace's Inbox retention setting (24h, 7d, 30d or forever; 24h by default); --ttl only shortens one post. A restart keeps unexpired posts under a new epoch but drops claims, agent acknowledgements and credentials.");
        return Ok(());
    }
    let mut project = None;
    let mut epoch = None;
    let mut after = 0;
    let mut thread = None;
    let replying = matches!(verb, "reply" | "comment");
    let mut kind = if replying {
        Kind::Feedback
    } else {
        Kind::Finding
    };
    let mut references = Vec::new();
    let mut key = None;
    let mut reply_to = None;
    let mut to = None;
    let mut task = None;
    let mut ttl_ms = None;
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
            "--" => {
                positional.extend_from_slice(&args[i + 1..]);
                break;
            }
            "--project" => project = Some(ProjectId(number(&value(&args, &mut i)?)?)),
            "--epoch" => epoch = Some(number(&value(&args, &mut i)?)?),
            "--after" => after = number(&value(&args, &mut i)?)?,
            "--thread" => thread = Some(message_id(&value(&args, &mut i)?)?),
            "--ref" => references.push(message_id(&value(&args, &mut i)?)?),
            "--reply-to" => reply_to = Some(message_id(&value(&args, &mut i)?)?),
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
                ttl_ms = Some(
                    number(&value(&args, &mut i)?)?
                        .checked_mul(1000)
                        .ok_or("TTL overflow")?,
                )
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
    if !references.is_empty() && verb != "post" && !replying {
        return Err("--ref belongs on a post or reply".into());
    }
    if replying {
        if reply_to.is_some() || positional.is_empty() {
            return Err("reply requires one parent message ID before the body".into());
        }
        reply_to = Some(message_id(&positional.remove(0))?);
    }
    if verb == "read" {
        if positional.len() != 1 || thread.is_some() || after != 0 {
            return Err("use read MESSAGE --epoch EPOCH for the complete conversation".into());
        }
        thread = Some(message_id(&positional[0])?);
    }
    let id = || {
        positional
            .first()
            .ok_or_else(|| "message ID required".into())
            .and_then(|v| message_id(v))
    };
    if matches!(verb, "ack" | "claim" | "release") && positional.len() != 1 {
        return Err(format!("{verb} requires exactly one message ID"));
    }
    let operation = match verb {
        "discover" => Operation::Discover,
        "credential" => Operation::Credential,
        "read" => Operation::Read {
            id: thread.ok_or("read requires a message ID")?,
            after_id: 0,
            through: None,
            seen: 0,
        },
        "list" => Operation::List {
            after,
            thread,
            limit: PAGE_LIMIT,
        },
        "watch" => Operation::Watch { after, timeout_ms },
        "post" | "reply" | "comment" => {
            let body = if stdin {
                if !positional.is_empty() {
                    return Err("choose text arguments or --stdin".into());
                }
                let mut body = String::new();
                std::io::stdin()
                    .take(uniterm_core::inbox::BODY_LIMIT as u64 + 3)
                    .read_to_string(&mut body)
                    .map_err(|e| e.to_string())?;
                // A shell pipeline usually supplies one line ending. Keep user
                // blank lines, and leave exactly one paragraph break before citations.
                if !references.is_empty() && body.ends_with('\n') {
                    body.pop();
                    if body.ends_with('\r') {
                        body.pop();
                    }
                }
                body
            } else {
                positional.join(" ")
            };
            PostDraft {
                references,
                kind,
                body,
                reply_to,
                to,
                task,
                key: key.ok_or("post requires a stable --key for safe retries")?,
                ttl_ms,
            }
            .into_operation()
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
        Operation::Cite(_)
            | Operation::Ack { .. }
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
    let mut request = |operation| {
        exchange(
            &mut reader,
            workspace,
            ControlCommand::Inbox {
                project,
                identity: identity.clone(),
                epoch,
                operation,
            },
        )
    };
    let mut legacy_read = false;
    let mut result = match request(operation) {
        // Servers predating the thread-read operation still speak the
        // change-ordered list protocol; the response is an explicit refusal.
        Err(error)
            if verb == "read" && error.starts_with("invalid_request: unknown variant `read`") =>
        {
            legacy_read = true;
            let id = thread.ok_or("read requires a message ID")?;
            let (root, page) = legacy_thread(id, &mut request)?;
            thread = Some(root);
            page
        }
        Err(error) if error.starts_with("invalid_request: unknown variant `cite`") => {
            return Err(
                "Thread references require an updated Workspace server. The post was refused."
                    .into(),
            )
        }
        result => result?,
    };
    if let Some(id) = thread.filter(|_| verb == "read") {
        let ControlResult::Inbox { page, .. } = &mut result else {
            return Err("unexpected thread response".into());
        };
        if page.entries.is_empty() {
            return Err("thread not found in this Project".into());
        }
        if page.entries.len() > PAGE_LIMIT {
            return Err("oversized thread page".into());
        }
        if legacy_read {
            // Legacy pages are change ordered. A concurrent revision may
            // repeat a message, so deduplicate by immutable ID and bound retries.
            let mut messages = std::collections::BTreeMap::new();
            let max_pages = THREAD_LIMIT.div_ceil(PAGE_LIMIT) * 2;
            for fetched in 1..=max_pages {
                messages.extend(page.entries.drain(..).map(|e| (e.message.id, e)));
                if messages.len() > THREAD_LIMIT {
                    return Err("thread exceeds the Inbox retention limit".into());
                }
                if !page.more {
                    page.entries = messages.into_values().collect();
                    break;
                }
                if fetched == max_pages {
                    return Err("thread changed during read; retry".into());
                }
                let ControlResult::Inbox { page: next, .. } = request(Operation::List {
                    after: page.cursor,
                    thread,
                    limit: PAGE_LIMIT,
                })?
                else {
                    return Err("unexpected thread page".into());
                };
                if next.cursor <= page.cursor || next.entries.len() > PAGE_LIMIT {
                    return Err("invalid thread page cursor".into());
                }
                *page = next;
            }
        } else {
            // Current servers return messages once, in ID order. Keep the
            // first Vec for a single-page read and append only further pages.
            let cutoff = page.cursor;
            while page.more {
                let last_id = page.entries.last().ok_or("empty thread page")?.message.id;
                if page.entries.len() >= THREAD_LIMIT {
                    return Err("thread exceeds the Inbox retention limit".into());
                }
                let ControlResult::Inbox { page: next, .. } = request(Operation::Read {
                    id,
                    after_id: last_id,
                    through: Some(cutoff),
                    seen: page.entries.len(),
                })?
                else {
                    return Err("unexpected thread page".into());
                };
                if next.cursor != cutoff
                    || next.entries.len() > PAGE_LIMIT
                    || (next.entries.is_empty() && next.more)
                    || next
                        .entries
                        .first()
                        .is_some_and(|e| e.message.id <= last_id)
                    || page.entries.len() + next.entries.len() > THREAD_LIMIT
                {
                    return Err("invalid thread page cursor".into());
                }
                page.entries.extend(next.entries);
                page.more = next.more;
                page.floor = next.floor;
                page.paused = next.paused;
            }
        }
    }
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
                "#{} thread:{}{} {:?} {:?} revision={} claim={:?}\n{}",
                m.id,
                m.thread,
                m.reply_to
                    .map_or_else(String::new, |id| format!(" replying to #{id}")),
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
