//! A one-shot readiness pipe lets recovery finish regardless of log size.
//! Only the launcher waits here; the running server gains no timer or polling.

use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::process::Child;
use std::time::{Duration, Instant};

const READY: u8 = b'R';
const DIAGNOSTIC_LIMIT: usize = 16 * 1024;

pub(super) fn notify_ready() -> io::Result<()> {
    // Stop writing diagnostics to the launcher's pipe before it goes away.
    let null = std::fs::OpenOptions::new().write(true).open("/dev/null")?;
    // SAFETY: dup2 replaces only this detached server's standard streams.
    if unsafe { libc::dup2(null.as_raw_fd(), libc::STDERR_FILENO) } == -1 {
        return Err(io::Error::last_os_error());
    }
    // Failure to notify (e.g. Ctrl-C in the launcher) must not stop recovery.
    let _ = io::stdout().lock().write_all(&[READY]);
    let _ = io::stdout().lock().flush();
    // SAFETY: as above; no later output belongs to the readiness pipe.
    if unsafe { libc::dup2(null.as_raw_fd(), libc::STDOUT_FILENO) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) fn wait_for_server(child: &mut Child, name: &str) -> io::Result<()> {
    let mut ready = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("missing readiness pipe"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("missing diagnostic pipe"))?;
    let mut fds = [
        libc::pollfd {
            fd: ready.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: stderr.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    let notice_at = Instant::now() + Duration::from_secs(1);
    let mut notified = false;
    let mut diagnostics = Vec::new();
    loop {
        let timeout = if notified {
            -1
        } else {
            notice_at
                .saturating_duration_since(Instant::now())
                .as_millis()
                .min(i32::MAX as u128) as i32
        };
        // SAFETY: both pollfd entries remain valid for this blocking call.
        let result = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if !notified && Instant::now() >= notice_at {
            eprintln!("uniterm: restoring Workspace '{name}'; waiting for startup to finish...");
            notified = true;
        }
        // Drain diagnostics during startup so a full pipe cannot deadlock
        // recovery. Retain only a bounded tail for a useful failure message.
        if fds[1].revents != 0 {
            let mut buffer = [0; 4096];
            match stderr.read(&mut buffer) {
                Ok(0) => fds[1].fd = -1,
                Ok(n) => {
                    diagnostics.extend_from_slice(&buffer[..n]);
                    if diagnostics.len() > DIAGNOSTIC_LIMIT {
                        diagnostics.drain(..diagnostics.len() - DIAGNOSTIC_LIMIT);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        if fds[0].revents != 0 {
            let mut byte = [0];
            match ready.read(&mut byte) {
                Ok(1) if byte[0] == READY => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
                _ => fds[0].fd = -1,
            }
        }
        if fds.iter().all(|fd| fd.fd == -1) {
            let status = child.wait()?;
            let details = String::from_utf8_lossy(&diagnostics);
            return Err(io::Error::other(format!(
                "Workspace '{name}' server exited during startup ({status}): {}",
                details.trim()
            )));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    fn fixture(script: &str) -> Child {
        Command::new("/bin/sh")
            .args(["-c", script])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    #[test]
    fn startup_waits_past_the_old_deadline() {
        let start = Instant::now();
        let mut child = fixture("sleep 6; printf R; exec sleep 30");
        let result = wait_for_server(&mut child, "slow");
        let alive = child.try_wait().unwrap().is_none();
        let _ = child.kill();
        child.wait().unwrap();
        result.unwrap();
        assert!(start.elapsed() >= Duration::from_secs(6));
        assert!(alive, "readiness must not terminate the server");
    }

    #[test]
    fn startup_reports_failure_and_drains_more_than_a_pipe_of_diagnostics() {
        let mut child = fixture("i=0; while [ $i -lt 10000 ]; do echo diagnostic >&2; i=$((i+1)); done; echo startup-boom >&2; exit 7");
        let error = wait_for_server(&mut child, "broken").unwrap_err();
        let message = error.to_string();
        assert!(message.contains("exited during startup"));
        assert!(message.contains('7'));
        assert!(message.contains("startup-boom"));
        assert!(message.len() < DIAGNOSTIC_LIMIT + 200);
    }

    #[test]
    fn startup_accepts_immediate_readiness_without_a_socket_probe() {
        let mut child = fixture("printf R");
        wait_for_server(&mut child, "ready").unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn startup_rejects_exit_without_readiness_even_with_zero_status() {
        let mut child = fixture("exit 0");
        assert!(wait_for_server(&mut child, "missing").is_err());
    }
}
