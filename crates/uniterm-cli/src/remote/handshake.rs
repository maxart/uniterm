//! During an in-app handoff OpenSSH still owns authentication and host trust.
//! Give it a private controlling tty for its prompts, separate from the
//! protocol pipes, remote stderr and the user's Workspace terminal.
use std::fs::File;
use std::io::{self, BufRead, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::time::{Duration, Instant};

use super::{MAX_REMOTE_PREAMBLE, REMOTE_READY};

pub(super) struct PromptTty {
    pub(super) master: File,
    slave: File,
}

impl PromptTty {
    pub(super) fn new() -> io::Result<Self> {
        let (mut master, mut slave) = (-1, -1);
        // SAFETY: both output pointers are valid, other arguments request defaults.
        if unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: openpty transferred two independent owned descriptors.
        let tty = unsafe {
            Self {
                master: File::from_raw_fd(master),
                slave: File::from_raw_fd(slave),
            }
        };
        for fd in [master, slave] {
            // SAFETY: both file descriptors remain owned by tty.
            if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        // Never reflect a response into the prompt surface, including when a
        // custom OpenSSH-compatible client reads /dev/tty without readpassphrase.
        // SAFETY: termios is POD and the slave remains open.
        unsafe {
            let mut settings = std::mem::zeroed();
            if libc::tcgetattr(slave, &mut settings) < 0 {
                return Err(io::Error::last_os_error());
            }
            settings.c_lflag &= !(libc::ECHO | libc::ECHONL);
            if libc::tcsetattr(slave, libc::TCSANOW, &settings) < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(tty)
    }

    pub(super) fn configure(&self, command: &mut Command) {
        let slave = self.slave.as_raw_fd();
        // SAFETY: the slave lives through spawn. Only async-signal-safe system
        // calls run between fork and exec, and the fd closes on exec.
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() < 0 || libc::ioctl(slave, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
}

pub(super) struct Handoff<'a> {
    pub(super) tty: &'a mut File,
    pub(super) input: i32,
    pub(super) winch: i32,
    /// Longest one open OpenSSH prompt may wait without a local keystroke.
    pub(super) prompt_timeout: Duration,
    pub(super) paint: &'a mut dyn FnMut(&mut uniterm_client::ConnectionInput, &str),
}

// A peer cannot keep the terminal in authentication by prompting forever.
const MAX_PROMPTS: usize = 16;

// OpenSSH leaves the cursor on the line it wants answered. A finished line,
// such as the newline it writes after reading a password, asks for nothing.
fn open_prompt(prompt: &[u8]) -> bool {
    !matches!(prompt.last(), None | Some(b'\n' | b'\r'))
        && prompt.iter().any(|byte| !byte.is_ascii_whitespace())
}

/// A read-only request on the candidate connection proves the selected
/// Workspace is serving framed requests before its predecessor detaches.
pub(super) struct WorkspaceProbe<'a> {
    writer: &'a mut dyn Write,
    expected: Option<&'a str>,
    pub(super) identity: Option<String>,
    /// Bytes the Workspace sent after its probe reply. They belong to the
    /// attach stream and must reach the client ahead of later bridge output.
    pub(super) pending: Vec<u8>,
}

// Probe replies are tiny. Anything larger is not a reply to the probe.
const MAX_PROBE_FRAME: usize = 1024;

impl<'a> WorkspaceProbe<'a> {
    pub(super) fn new(writer: &'a mut dyn Write, expected: Option<&'a str>) -> Self {
        Self {
            writer,
            expected,
            identity: None,
            pending: Vec::new(),
        }
    }

    fn accept(&mut self, bytes: &[u8]) -> io::Result<bool> {
        use uniterm_proto::ServerMessage;
        let invalid =
            |text: &'static str| io::Error::new(io::ErrorKind::InvalidData, text.to_owned());
        self.pending.extend_from_slice(bytes);
        // Decode one exact frame at a time so the first byte after the reply
        // stays here, whatever its size or framing.
        while let Some(header) = self.pending.first_chunk::<4>() {
            let size = u32::from_be_bytes(*header) as usize;
            if size > MAX_PROBE_FRAME {
                return Err(invalid("invalid SSH Workspace reply"));
            }
            let Some(frame) = self.pending.get(..4 + size) else {
                break;
            };
            let mut decoder = uniterm_proto::FrameDecoder::with_max_frame(MAX_PROBE_FRAME as u32);
            decoder.push(frame);
            let message = decoder
                .decode::<ServerMessage>()
                .ok()
                .flatten()
                .ok_or_else(|| invalid("invalid SSH Workspace reply"))?;
            self.pending.drain(..4 + size);
            match message {
                ServerMessage::SessionRenamed { name } if self.identity.is_none() => {
                    if uniterm_proto::validate_workspace_name(&name).is_err()
                        || self.expected.is_some_and(|expected| expected != name)
                    {
                        return Err(invalid("SSH bridge identified a different Workspace"));
                    }
                    self.identity = Some(name);
                }
                ServerMessage::Info { .. } => return Ok(true),
                _ => return Err(invalid("unexpected SSH Workspace reply")),
            }
        }
        Ok(false)
    }
}

pub(super) fn ready<R: Read + AsRawFd>(
    reader: &mut io::BufReader<R>,
    mut attached: Option<Handoff<'_>>,
    timeout: Duration,
    mut probe: Option<&mut WorkspaceProbe<'_>>,
) -> io::Result<()> {
    let mut probing = false;
    let mut deadline = Instant::now() + timeout;
    let mut tail = Vec::new();
    let mut total = 0;
    let mut prompt = Vec::new();
    let mut awaiting_answer = false;
    // The handshake budget counts only time spent waiting for the peer. An
    // open OpenSSH prompt suspends it and runs its own bound instead, which
    // only a local keystroke renews. Remote output never renews either one.
    let mut prompt_deadline = deadline;
    let mut prompt_started = Instant::now();
    let mut prompts = 0;
    let mut keys = uniterm_client::ConnectionInput::default();
    if let Some(ui) = &mut attached {
        (ui.paint)(&mut keys, "Waiting for SSH");
    }
    loop {
        let limit = if awaiting_answer {
            prompt_deadline
        } else {
            deadline
        };
        let left = limit.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                if awaiting_answer {
                    "SSH prompt was not answered in time"
                } else {
                    "SSH timed out waiting for the Uniterm handshake"
                },
            ));
        }
        let mut fds = [
            pollfd(reader.get_ref().as_raw_fd()),
            pollfd(attached.as_ref().map_or(-1, |ui| ui.input)),
            pollfd(attached.as_ref().map_or(-1, |ui| ui.tty.as_raw_fd())),
            pollfd(attached.as_ref().map_or(-1, |ui| ui.winch)),
        ];
        // The login marker and first protocol frame may share a single read.
        let buffered = !reader.buffer().is_empty();
        let wait_ms = if buffered {
            0
        } else {
            left.as_millis().clamp(1, i32::MAX as u128) as _
        };
        // SAFETY: the pollfd array and its live descriptors outlive poll.
        if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, wait_ms) } < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if fds[3].revents != 0 {
            if let Some(ui) = attached.as_mut() {
                let mut signals = [0u8; 128];
                // SAFETY: winch is the source attachment's nonblocking self-pipe.
                unsafe {
                    libc::read(ui.winch, signals.as_mut_ptr().cast(), signals.len());
                }
                if prompt.is_empty() {
                    (ui.paint)(&mut keys, "Waiting for SSH");
                } else {
                    (ui.paint)(&mut keys, &String::from_utf8_lossy(&prompt));
                }
            }
        }
        // Discard typeahead before processing new prompts or declaring the
        // new destination ready. Never forward mouse reports to authentication.
        if fds[1].revents != 0 {
            let mut bytes = [0; 4096];
            // Use the same unbuffered fd as the attach loop so the standard
            // library never prefetches input across an ownership transition.
            // SAFETY: fd 0 is owned by the current attach and bytes is writable.
            let n = unsafe { libc::read(fds[1].fd, bytes.as_mut_ptr().cast(), bytes.len()) };
            let read = if n < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(n as usize)
            };
            let n = match read {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "terminal closed",
                    ))
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    continue
                }
                other => other?,
            };
            let bytes = keys.decode(&bytes[..n])?;
            if awaiting_answer {
                if let Some(ui) = attached.as_mut() {
                    if !bytes.is_empty() {
                        prompt_deadline = Instant::now() + ui.prompt_timeout;
                    }
                    let end = bytes.iter().position(|b| matches!(b, b'\n' | b'\r'));
                    ui.tty.write_all(&bytes[..end.unwrap_or(bytes.len())])?;
                    if end.is_some() {
                        ui.tty.write_all(b"\n")?;
                        awaiting_answer = false;
                        prompt.clear();
                        // Give back the time the human spent on this prompt.
                        deadline += prompt_started.elapsed();
                    }
                }
            }
        }
        if fds[2].revents != 0 {
            if let Some(ui) = attached.as_mut() {
                let mut buf = [0; 1024];
                let n = ui.tty.read(&mut buf)?;
                if n > 0 {
                    append_tail(&mut prompt, &buf[..n]);
                    if !prompt.iter().any(|byte| !byte.is_ascii_whitespace()) {
                        // The line ending after an answer is not a new prompt.
                        prompt.clear();
                    } else {
                        (ui.paint)(&mut keys, &String::from_utf8_lossy(&prompt));
                    }
                    if !awaiting_answer && open_prompt(&prompt) {
                        prompts += 1;
                        if prompts > MAX_PROMPTS {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "SSH asked too many questions before the Uniterm handshake",
                            ));
                        }
                        awaiting_answer = true;
                        prompt_started = Instant::now();
                        prompt_deadline = prompt_started + ui.prompt_timeout;
                    }
                }
            }
        }
        if buffered || fds[0].revents != 0 {
            let bytes = reader.fill_buf()?;
            if bytes.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "SSH bridge closed before its protocol handshake",
                ));
            }
            let n = bytes.len();
            if probing {
                if let Some(current) = probe.as_mut() {
                    let confirmed = current.accept(bytes)?;
                    reader.consume(n);
                    if confirmed {
                        return Ok(());
                    }
                    continue;
                }
            }
            tail.extend_from_slice(bytes);
            if let Some(start) = tail
                .windows(REMOTE_READY.len())
                .position(|s| s == REMOTE_READY)
            {
                let consumed = n - (tail.len() - start - REMOTE_READY.len());
                if total + consumed <= MAX_REMOTE_PREAMBLE {
                    reader.consume(consumed);
                    if let Some(current) = probe.as_mut() {
                        current.writer.write_all(&uniterm_proto::encode_frame(
                            &uniterm_proto::ClientMessage::ListInfo,
                        ))?;
                        probing = true;
                        continue;
                    }
                    return Ok(());
                }
            }
            total += n;
            if total >= MAX_REMOTE_PREAMBLE {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "SSH login emitted too much text before the Uniterm handshake",
                ));
            }
            tail.drain(..tail.len().saturating_sub(REMOTE_READY.len() - 1));
            reader.consume(n);
        }
    }
}

fn pollfd(fd: i32) -> libc::pollfd {
    libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    }
}

pub(super) fn append_tail(tail: &mut Vec<u8>, bytes: &[u8]) {
    const LIMIT: usize = 4096;
    tail.extend_from_slice(bytes);
    tail.drain(..tail.len().saturating_sub(LIMIT));
}

/// Drain diagnostics for the whole connection, retaining a bounded tail.
/// The local reader can be closed even when an SSH helper inherits stderr.
pub(super) struct Diagnostics {
    socket: std::os::unix::net::UnixStream,
    worker: Option<std::thread::JoinHandle<String>>,
    native: std::sync::Arc<std::sync::Mutex<bool>>,
}

impl Diagnostics {
    pub(super) fn new(native_prompts: bool) -> io::Result<(Self, std::process::Stdio)> {
        use std::os::fd::OwnedFd;
        let (socket, writer) = std::os::unix::net::UnixStream::pair()?;
        let mut reader = socket.try_clone()?;
        let native = std::sync::Arc::new(std::sync::Mutex::new(native_prompts));
        let show = native.clone();
        let worker = std::thread::spawn(move || {
            let mut tail = Vec::new();
            let mut buf = [0; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        append_tail(&mut tail, &buf[..n]);
                        let show = show.lock().unwrap_or_else(|lock| lock.into_inner());
                        if *show {
                            // Direct CLI entry owns a cooked terminal. OpenSSH can
                            // print keyboard-interactive instructions on stderr.
                            let clean: String = String::from_utf8_lossy(&buf[..n])
                                .chars()
                                .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
                                .collect();
                            let _ = io::stderr().lock().write_all(clean.as_bytes());
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            String::from_utf8_lossy(&tail)
                .lines()
                .rev()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .unwrap_or_default()
                .chars()
                .filter(|c| !c.is_control())
                .take(512)
                .collect()
        });
        Ok((
            Self {
                socket,
                worker: Some(worker),
                native,
            },
            OwnedFd::from(writer).into(),
        ))
    }

    pub(super) fn end_native_prompts(&self) {
        *self.native.lock().unwrap_or_else(|lock| lock.into_inner()) = false;
    }

    pub(super) fn finish(mut self) -> String {
        let _ = self.socket.shutdown(std::net::Shutdown::Read);
        self.worker
            .take()
            .and_then(|worker| worker.join().ok())
            .unwrap_or_default()
    }
}

impl Drop for Diagnostics {
    fn drop(&mut self) {
        let _ = self.socket.shutdown(std::net::Shutdown::Read);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;
    use std::process::Stdio;

    #[test]
    fn workspace_probe_accepts_legacy_replies_and_checks_the_reported_identity() {
        use uniterm_proto::{encode_frame, ClientMessage, ServerMessage};
        let info = encode_frame(&ServerMessage::Info {
            windows: 1,
            panes: 1,
        });
        for name in [None, Some("build"), Some("other")] {
            let (mut server, client) = UnixStream::pair().unwrap();
            let mut output = REMOTE_READY.to_vec();
            if let Some(name) = name {
                output.extend(encode_frame(&ServerMessage::SessionRenamed {
                    name: name.into(),
                }));
            }
            output.extend(&info);
            server.write_all(&output).unwrap();
            let mut sent = Vec::new();
            let mut probe = WorkspaceProbe::new(&mut sent, Some("build"));
            let result = ready(
                &mut io::BufReader::with_capacity(3, client),
                None,
                Duration::from_secs(1),
                Some(&mut probe),
            );
            assert_eq!(result.is_ok(), name != Some("other"), "{result:?}");
            assert_eq!(probe.identity.as_deref(), name.filter(|n| *n == "build"));
            assert_eq!(sent, encode_frame(&ClientMessage::ListInfo));
        }
    }

    #[test]
    fn only_the_private_ssh_tty_can_request_an_answer() {
        let mut prompt = PromptTty::new().unwrap();
        let mut cmd = Command::new("sh");
        let (diagnostics, stderr) = Diagnostics::new(false).unwrap();
        cmd.arg("-c")
            .arg(
                r#"
            printf 'stderr password prompt\n' >&2
            printf 'banner password prompt\n'
            printf 'Host fingerprint. Trust? ' >/dev/tty
            IFS= read -r reply </dev/tty && test "$reply" = yes || exit 23
            printf 'SSH password: ' >/dev/tty
            IFS= read -r reply </dev/tty && test "$reply" = fixture-secret || exit 24
            printf 'UNITERM-REMOTE/1\n'
        "#,
            )
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(stderr);
        prompt.configure(&mut cmd);
        let mut child = cmd.spawn().unwrap();
        drop(cmd);
        let mut stdout = io::BufReader::new(child.stdout.take().unwrap());
        let (mut input, keyboard) = UnixStream::pair().unwrap();
        let (mut resize, winch) = UnixStream::pair().unwrap();
        let mut resized = false;
        let mut prompts = Vec::new();
        let mut paint = |_: &mut uniterm_client::ConnectionInput, text: &str| {
            prompts.push(text.to_owned());
            match text {
                "Waiting for SSH" => input.write_all(b"discard me\n").unwrap(),
                "Host fingerprint. Trust? " => {
                    input.write_all(b"yes\rignored second line\r").unwrap()
                }
                "SSH password: " if !resized => {
                    resized = true;
                    resize.write_all(&[1]).unwrap();
                }
                "SSH password: " => input.write_all(b"fixture-secret\r").unwrap(),
                _ => {}
            }
        };
        let ui = Handoff {
            tty: &mut prompt.master,
            input: keyboard.as_raw_fd(),
            winch: winch.as_raw_fd(),
            prompt_timeout: Duration::from_secs(3),
            paint: &mut paint,
        };
        let result = ready(&mut stdout, Some(ui), Duration::from_secs(3), None);
        if result.is_err() {
            let _ = child.kill();
        }
        let status = child.wait().unwrap();
        assert!(result.is_ok(), "{result:?} {prompts:?}");
        assert!(status.success(), "{status}");
        assert_eq!(
            prompts,
            [
                "Waiting for SSH",
                "Host fingerprint. Trust? ",
                "SSH password: ",
                "SSH password: "
            ]
        );
        assert_eq!(diagnostics.finish(), "stderr password prompt");
    }

    // Run a scripted stand-in for OpenSSH on a private prompt terminal. The
    // responder sees every painted text and may type into the local keyboard.
    fn prompt_handshake(
        script: &str,
        timeout: Duration,
        prompt_timeout: Duration,
        mut respond: impl FnMut(&str, &mut UnixStream),
    ) -> (io::Result<()>, Vec<String>, Duration) {
        let mut prompt = PromptTty::new().unwrap();
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        prompt.configure(&mut cmd);
        let mut child = cmd.spawn().unwrap();
        drop(cmd);
        let mut stdout = io::BufReader::new(child.stdout.take().unwrap());
        let (mut input, keyboard) = UnixStream::pair().unwrap();
        let (_resize, winch) = UnixStream::pair().unwrap();
        let mut painted = Vec::new();
        let mut paint = |_: &mut uniterm_client::ConnectionInput, text: &str| {
            painted.push(text.to_owned());
            respond(text, &mut input);
        };
        let start = Instant::now();
        let result = ready(
            &mut stdout,
            Some(Handoff {
                tty: &mut prompt.master,
                input: keyboard.as_raw_fd(),
                winch: winch.as_raw_fd(),
                prompt_timeout,
                paint: &mut paint,
            }),
            timeout,
            None,
        );
        let elapsed = start.elapsed();
        // SAFETY: the scripted session leader is owned and not yet reaped.
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
        let _ = child.wait();
        (result, painted, elapsed)
    }

    #[test]
    fn an_open_prompt_suspends_the_handshake_budget_until_it_is_answered() {
        // The person takes longer over the code than the whole handshake
        // budget. That time is theirs, and the budget resumes afterwards.
        let mut answering = None;
        let (result, painted, _) = prompt_handshake(
            "printf 'Verification code: ' >/dev/tty; IFS= read -r code </dev/tty; \
             test \"$code\" = 123456 && printf 'UNITERM-REMOTE/1\\n'",
            Duration::from_secs(1),
            Duration::from_secs(20),
            |text, input| {
                if text == "Verification code: " && answering.is_none() {
                    let mut keyboard = input.try_clone().unwrap();
                    answering = Some(std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_millis(2500));
                        keyboard.write_all(b"123456\r").unwrap();
                    }));
                }
            },
        );
        answering.unwrap().join().unwrap();
        assert!(result.is_ok(), "{result:?} {painted:?}");

        // After the answer the peer is on the clock again, and the line
        // ending OpenSSH writes behind a password is not another prompt.
        let (result, painted, elapsed) = prompt_handshake(
            "printf 'Password: ' >/dev/tty; IFS= read -r secret </dev/tty; \
             printf '\\n' >/dev/tty; sleep 30",
            Duration::from_millis(600),
            Duration::from_secs(20),
            |text, input| {
                if text == "Password: " {
                    input.write_all(b"secret\r").unwrap();
                }
            },
        );
        let error = result.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("handshake"), "{error}");
        assert!(elapsed < Duration::from_secs(10), "{elapsed:?}");
        assert_eq!(painted, ["Waiting for SSH", "Password: "]);
    }

    #[test]
    fn remote_output_and_pointer_reports_cannot_extend_an_unanswered_prompt() {
        // Banner output, further terminal text and mouse motion all arrive
        // while the prompt is open. None of them is a person answering it.
        let (result, painted, elapsed) = prompt_handshake(
            "printf 'Password: ' >/dev/tty; \
             while :; do printf 'banner\\n'; printf 'still there? ' >/dev/tty; sleep 0.05; done",
            Duration::from_secs(60),
            Duration::from_millis(500),
            |_, input| input.write_all(b"\x1b[<35;4;4M").unwrap(),
        );
        let error = result.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("not answered"), "{error}");
        assert!(elapsed < Duration::from_secs(10), "{elapsed:?}");
        assert!(painted.iter().any(|text| text.starts_with("Password: ")));

        // A typed character does renew it, so a slow typist is not cut off,
        // but a peer cannot hold the terminal by asking without end.
        let (result, _, _) = prompt_handshake(
            "i=0; while [ $i -lt 40 ]; do printf 'Again %s? ' $i >/dev/tty; \
             IFS= read -r reply </dev/tty; i=$((i+1)); done; printf 'UNITERM-REMOTE/1\\n'",
            Duration::from_secs(20),
            Duration::from_secs(20),
            |text, input| {
                if text.ends_with("? ") {
                    input.write_all(b"y\r").unwrap();
                }
            },
        );
        let error = result.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("too many"), "{error}");
    }

    #[test]
    fn prompt_lines_are_open_only_while_the_cursor_waits_on_visible_text() {
        for (text, open) in [
            (&b""[..], false),
            (b"\n", false),
            (b" \r\n", false),
            (b"   ", false),
            (b"Password: ", true),
            (b"Enter the code from your device.\nCode: ", true),
            (b"Enter the code from your device.\n", false),
            (b"Trust (yes/no)?", true),
        ] {
            assert_eq!(open_prompt(text), open, "{text:?}");
        }
    }

    #[test]
    fn probe_keeps_every_byte_after_its_reply_and_rejects_oversized_replies() {
        use uniterm_proto::{encode_frame, ServerMessage};
        let info = encode_frame(&ServerMessage::Info {
            windows: 2,
            panes: 3,
        });
        let after: Vec<u8> = (0..5000u32).map(|n| n as u8).collect();
        let stream = [info.as_slice(), &after].concat();
        // Whatever the read boundaries, the remainder is exact and complete.
        for chunk in [1, 3, info.len(), info.len() + 1, stream.len()] {
            let mut sent = Vec::new();
            let mut probe = WorkspaceProbe::new(&mut sent, None);
            let mut confirmed = false;
            for bytes in stream.chunks(chunk) {
                if confirmed {
                    probe.pending.extend_from_slice(bytes);
                } else {
                    confirmed = probe.accept(bytes).unwrap();
                }
            }
            assert!(confirmed);
            assert_eq!(probe.pending, after, "chunk {chunk}");
        }
        let mut sent = Vec::new();
        let mut probe = WorkspaceProbe::new(&mut sent, None);
        let oversized = (MAX_PROBE_FRAME as u32 + 1).to_be_bytes();
        assert_eq!(
            probe.accept(&oversized).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn diagnostic_tail_finishes_with_an_inherited_open_writer() {
        let (diagnostics, stderr) = Diagnostics::new(false).unwrap();
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "printf 'junk\\nDenied by peer\\033\\007\\n' >&2"])
            .stderr(stderr);
        assert!(cmd.status().unwrap().success());
        // cmd still owns its copy of the writer. Reaping the SSH process must
        // suffice to return a bounded diagnostic even with that descriptor open.
        assert_eq!(diagnostics.finish(), "Denied by peer");
    }
}
