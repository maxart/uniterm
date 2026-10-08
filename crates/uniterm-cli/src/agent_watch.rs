//! Event-driven agent monitoring over the durable control stream.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use uniterm_core::{AgentDetail, AgentDetails, AgentStatus, LoopState, PaneId};
use uniterm_proto::{
    ControlCommand, ControlFrame, ControlRequest, ControlResult, CONTROL_API_VERSION,
};

#[derive(Default)]
struct State {
    agent: Option<String>,
    status: Option<AgentStatus>,
    details: AgentDetails,
    waiting: Option<u64>,
}

impl State {
    fn apply(&mut self, pane: PaneId, event: &Value, at_ms: u64) -> Option<&'static str> {
        if let Some(resolved) = event.get("WaitingResolved") {
            if resolved["id"].as_u64() == self.waiting && self.waiting.is_some() {
                self.waiting = None;
                return Some("waiting_resolved");
            }
            return None;
        }
        let (kind, data) = event.as_object()?.iter().next()?;
        let event_pane = if kind == "WaitingCreated" {
            &data["item"]["pane"]
        } else {
            &data["pane"]
        };
        if event_pane.as_u64() != Some(pane.0) {
            return None;
        }
        match kind.as_str() {
            "AgentBound" => {
                *self = Self {
                    agent: data["agent"].as_str().map(str::to_owned),
                    details: AgentDetails::new(
                        data["invocation"].as_i64().and_then(|v| v.try_into().ok()),
                    ),
                    ..Self::default()
                };
                Some("bound")
            }
            "AgentStatus" => {
                let status: AgentStatus = serde_json::from_value(data["status"].clone()).ok()?;
                if self.status == Some(status) {
                    return None;
                }
                self.status = Some(status);
                self.details.observe_status(status);
                Some("status")
            }
            "AgentSessionObserved" => {
                if self.details.observe_session(
                    data["session_id"].as_str(),
                    data["transcript_path"].as_str(),
                ) {
                    self.waiting = None;
                }
                Some("session")
            }
            "AgentDetail" => {
                let invocation = data["invocation"].as_i64().and_then(|v| v.try_into().ok());
                if self.details.invocation.is_some() && invocation != self.details.invocation {
                    return None;
                }
                if self.details.invocation.is_none() {
                    self.details.invocation = invocation;
                }
                let detail: AgentDetail =
                    if let Ok(detail) = serde_json::from_value(data["detail"].clone()) {
                        detail
                    } else {
                        // The read-only manager stream preserves event kinds but
                        // deliberately omits provider-authored text and paths.
                        match data["kind"].as_str()? {
                            "notification" => AgentDetail::Notification {
                                kind: String::new(),
                                message: String::new(),
                            },
                            "permission_asked" => AgentDetail::PermissionAsked {
                                tool: String::new(),
                                preview: String::new(),
                            },
                            "loop" => AgentDetail::Loop {
                                state: serde_json::from_value(data["loop_state"].clone()).ok()?,
                                delay_seconds: None,
                            },
                            _ => return None,
                        }
                    };
                let kind = match &detail {
                    AgentDetail::Notification { .. } => "notification",
                    AgentDetail::PermissionAsked { .. } => "permission",
                    AgentDetail::Loop { .. } => "loop",
                };
                self.details.apply(&detail, at_ms);
                Some(kind)
            }
            "WaitingCreated" => {
                self.waiting = data["item"]["id"].as_u64();
                Some("waiting")
            }
            "AgentUnbound" => {
                *self = Self::default();
                Some("unbound")
            }
            "PaneClosed" => Some("closed"),
            _ => None,
        }
    }

    fn line(&self, pane: PaneId, event: &str, sequence: u64, timestamp_ms: u64) -> Value {
        json!({"sequence":sequence,"timestamp_ms":timestamp_ms,"pane":pane.0,"event":event,
            "agent":self.agent,"status":self.status.map(AgentStatus::label),
            "invocation":self.details.invocation,"session_id":self.details.session_id,
            "transcript_path":self.details.transcript_path,"notification":self.details.last_notification,
            "permission":self.details.pending_permission,"loop":self.details.loop_state,"waiting_id":self.waiting})
    }

    fn matches(&self, wanted: &str, event: &str) -> bool {
        match wanted {
            "notification" => event == "notification",
            "permission" => {
                matches!(event, "permission" | "waiting")
                    && self.status == Some(AgentStatus::Permission)
                    && self.waiting.is_some()
            }
            "loop-stopped" => {
                event == "loop"
                    && self
                        .details
                        .loop_state
                        .as_ref()
                        .is_some_and(|item| item.state == LoopState::Stopped)
            }
            _ => false,
        }
    }
}

fn read_frame(
    reader: &mut BufReader<UnixStream>,
    deadline: Option<Instant>,
) -> io::Result<ControlFrame> {
    if let Some(deadline) = deadline {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "agent event wait timed out"))?;
        reader.get_ref().set_read_timeout(Some(remaining))?;
    }
    let mut line = Vec::new();
    reader
        .by_ref()
        .take(u64::from(uniterm_proto::CONTROL_MAX_FRAME_BYTES) + 1)
        .read_until(b'\n', &mut line)?;
    if line.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "agent stream closed; reconnect after the last sequence",
        ));
    }
    if line.len() > uniterm_proto::CONTROL_MAX_FRAME_BYTES as usize || !line.ends_with(b"\n") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid control frame",
        ));
    }
    let frame: ControlFrame = serde_json::from_slice(&line)?;
    match &frame {
        ControlFrame::Response(response) if response.version != CONTROL_API_VERSION => {
            return Err(io::Error::other("unsupported control version"))
        }
        ControlFrame::Response(response) if response.error.is_some() => {
            return Err(io::Error::other(
                response.error.as_ref().unwrap().message.clone(),
            ))
        }
        ControlFrame::StreamError(error) => {
            return Err(io::Error::other(format!(
                "{}: {}",
                error.code, error.message
            )))
        }
        _ => {}
    }
    Ok(frame)
}

fn send(
    reader: &mut BufReader<UnixStream>,
    workspace: &str,
    id: u64,
    command: ControlCommand,
) -> io::Result<()> {
    writeln!(
        reader.get_mut(),
        "{}",
        serde_json::to_string(&ControlRequest {
            version: CONTROL_API_VERSION,
            id,
            workspace: workspace.into(),
            command,
        })?
    )
}

fn print_line(line: &Value) -> io::Result<()> {
    let mut out = io::stdout().lock();
    writeln!(out, "{line}")?;
    out.flush()
}

fn watch(
    socket: &Path,
    pane: PaneId,
    after: Option<u64>,
    wanted: Option<&str>,
    timeout: Duration,
) -> io::Result<()> {
    let workspace = socket
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| io::Error::other("invalid Workspace socket"))?;
    let stream = UnixStream::connect(socket.with_extension("control.sock"))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(stream);
    let deadline = wanted
        .map(|_| {
            Instant::now()
                .checked_add(timeout)
                .ok_or_else(|| io::Error::other("timeout too large"))
        })
        .transpose()?;
    let mut state = State::default();
    send(&mut reader, workspace, 1, ControlCommand::AgentList)?;
    let ControlFrame::Response(response) = read_frame(&mut reader, deadline)? else {
        return Err(io::Error::other("expected agent snapshot"));
    };
    let Some(ControlResult::Fleet { entries, sequence }) = response.result else {
        return Err(io::Error::other(
            "server does not support agent monitoring; upgrade the Workspace server",
        ));
    };
    if after.is_some_and(|cursor| cursor > sequence) {
        return Err(io::Error::other(
            "--after exceeds the Workspace event cursor",
        ));
    }
    {
        if let Some(entry) = entries.into_iter().find(|entry| entry.pane_id == pane) {
            if after.is_none() {
                state = State {
                    agent: Some(entry.agent),
                    status: Some(entry.status),
                    details: entry.details,
                    waiting: entry.waiting,
                };
            }
        } else {
            send(&mut reader, workspace, 2, ControlCommand::PaneList)?;
            let ControlFrame::Response(response) = read_frame(&mut reader, deadline)? else {
                return Err(io::Error::other("expected Pane snapshot"));
            };
            if !matches!(response.result, Some(ControlResult::Panes { panes, .. }) if panes.iter().any(|entry| entry.id == pane))
            {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no Pane {}", pane.0),
                ));
            }
        }
        if after.is_none() && wanted.is_none() {
            print_line(&state.line(pane, "snapshot", sequence, 0))?;
        }
    }
    let cutoff = after.unwrap_or(sequence);
    // Reconnect rebuilds identity from the retained log, never by mixing a
    // current snapshot into older events. Memory remains bounded to one Pane.
    send(
        &mut reader,
        workspace,
        3,
        ControlCommand::Subscribe {
            after_sequence: if after.is_some() { 0 } else { sequence },
        },
    )?;
    reader.get_ref().set_read_timeout(None)?;
    loop {
        match read_frame(&mut reader, deadline)? {
            ControlFrame::Event(event) => {
                if event.version != CONTROL_API_VERSION || event.workspace != workspace {
                    return Err(io::Error::other("unexpected agent stream scope"));
                }
                if let Some(kind) = state.apply(pane, &event.event, event.timestamp_ms) {
                    if event.sequence <= cutoff {
                        continue;
                    }
                    if wanted.is_none() || wanted.is_some_and(|wanted| state.matches(wanted, kind))
                    {
                        print_line(&state.line(pane, kind, event.sequence, event.timestamp_ms))?;
                        if wanted.is_some() {
                            return Ok(());
                        }
                    }
                    if kind == "closed" {
                        return if wanted.is_some() {
                            Err(io::Error::other("Pane closed before the requested event"))
                        } else {
                            Ok(())
                        };
                    }
                }
            }
            ControlFrame::Response(_) => {}
            ControlFrame::StreamError(_) => unreachable!(),
        }
    }
}

pub(super) fn command(socket: &Path, args: &[String], wait: bool) -> i32 {
    let usage = if wait {
        "ut agent wait PANE --event notification|permission|loop-stopped [--after SEQUENCE] [--timeout SECONDS]"
    } else {
        "ut agent watch PANE [--after SEQUENCE]"
    };
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        println!("Usage: {usage}\nStreams durable events as NDJSON; idle includes loop pauses and does not imply completion.");
        return 0;
    }
    let parse = || -> Result<_, String> {
        let pane = super::resolve_pane_target(args.first().map(String::as_str))?;
        let mut after = None;
        let mut wanted = None;
        let mut timeout = Duration::from_secs(30);
        let mut rest = args.get(1..).unwrap_or_default().iter();
        while let Some(option) = rest.next() {
            let value = rest
                .next()
                .ok_or_else(|| format!("missing value for {option}"))?;
            match option.as_str() {
                "--after" if after.is_none() => {
                    after = Some(value.parse::<u64>().map_err(|_| "invalid sequence")?)
                }
                "--timeout" if wait => {
                    timeout =
                        Duration::from_secs(value.parse::<u64>().map_err(|_| "invalid timeout")?)
                }
                "--event"
                    if wait
                        && wanted.is_none()
                        && matches!(
                            value.as_str(),
                            "notification" | "permission" | "loop-stopped"
                        ) =>
                {
                    wanted = Some(value.as_str())
                }
                _ => return Err(format!("invalid option or value: {option} {value}")),
            }
        }
        if wait && wanted.is_none() {
            return Err("--event is required".into());
        }
        Ok((pane, after, wanted, timeout))
    };
    let (pane, after, wanted, timeout) = match parse() {
        Ok(options) => options,
        Err(error) => {
            eprintln!("uniterm agent: {error}\nusage: {usage}");
            return 2;
        }
    };
    match watch(socket, pane, after, wanted, timeout) {
        Ok(()) => 0,
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => 0,
        Err(error) => {
            eprintln!(
                "uniterm agent: {}",
                super::terminal_safe(&error.to_string())
            );
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unrelated_and_duplicate_status_events_are_no_ops() {
        let mut state = State::default();
        assert_eq!(
            state.apply(
                PaneId(1),
                &json!({"AgentStatus":{"pane":2,"status":"Working"}}),
                1
            ),
            None
        );
        assert_eq!(state.status, None);
        assert_eq!(
            state.apply(
                PaneId(1),
                &json!({"AgentStatus":{"pane":1,"status":"Idle"}}),
                2
            ),
            Some("status")
        );
        assert_eq!(
            state.apply(
                PaneId(1),
                &json!({"AgentStatus":{"pane":1,"status":"Idle"}}),
                3
            ),
            None
        );
    }
    #[test]
    fn redacted_events_keep_their_kind_without_synthesizing_private_fields() {
        let mut state = State::default();
        assert_eq!(state.apply(PaneId(1),&json!({"AgentDetail":{"pane":1,"invocation":null,"kind":"notification","loop_state":null}}),1),Some("notification"));
        assert_eq!(
            state.details.last_notification.as_ref().unwrap().message,
            ""
        );
        assert_eq!(state.apply(PaneId(1),&json!({"AgentDetail":{"pane":1,"invocation":null,"kind":"loop","loop_state":"stopped"}}),2),Some("loop"));
        assert!(state.matches("loop-stopped", "loop"));
        assert_eq!(state.details.transcript_path, None);
    }
    #[test]
    fn first_process_identification_preserves_session_and_accepts_details() {
        let mut state = State::default();
        state.apply(
            PaneId(1),
            &json!({"AgentBound":{"pane":1,"agent":"demo","invocation":null}}),
            1,
        );
        state.apply(
            PaneId(1),
            &json!({"AgentSessionObserved":{"pane":1,"session_id":"demo-a"}}),
            2,
        );
        assert_eq!(state.apply(PaneId(1), &json!({"AgentDetail":{"pane":1,"invocation":5,"detail":{"Notification":{"kind":"push","message":"ready"}}}}), 3), Some("notification"));
        assert_eq!(state.details.invocation, Some(5));
        assert_eq!(state.details.session_id.as_deref(), Some("demo-a"));
        assert_eq!(state.details.last_notification.unwrap().message, "ready");
    }
    #[test]
    fn a_foreign_invocation_cannot_replace_current_details() {
        let mut state = State {
            details: AgentDetails::new(Some(4)),
            ..State::default()
        };
        assert_eq!(state.apply(PaneId(1),&json!({"AgentDetail":{"pane":1,"invocation":5,"detail":{"Notification":{"kind":"push","message":"stale"}}}}),1),None);
        assert!(state.details.last_notification.is_none());
    }
}
