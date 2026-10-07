//! On-demand listener termination scoped to the source PTY session.
//! Process and socket inspection happens only for an explicit stop action.

use std::time::{Duration, Instant};

use crate::process_watch::ProcessWatch;

/// Stop the selected port's owners, preserving other jobs and the Pane shell.
pub(crate) fn stop_listener(session: u32, port: u16) -> Result<(), String> {
    stop(session, port).map_err(|error| error.to_string())
}

fn in_session(pid: i32, session: u32) -> bool {
    // SAFETY: getsid only queries the supplied positive process id.
    pid > 1 && session > 1 && unsafe { libc::getsid(pid) } == session as i32
}

fn stop(session: u32, port: u16) -> std::io::Result<()> {
    let session_pid = i32::try_from(session).map_err(std::io::Error::other)?;
    let anchor = ProcessWatch::new(session_pid)?;
    let owners = listener_owners(session, port)?;
    let mut watches = Vec::new();
    for pid in owners {
        let watch = ProcessWatch::new(pid)?;
        if !in_session(pid, session) || anchor.wait_exit(Duration::ZERO)? {
            return Err(std::io::Error::other(
                "The listener no longer belongs to the source pane",
            ));
        }
        watches.push(watch);
    }
    if watches.is_empty() {
        return Err(std::io::Error::other(
            "No listener owned by the source pane was found",
        ));
    }
    // Recheck socket ownership after acquiring native process identities.
    let current = listener_owners(session, port)?;
    if watches.iter().any(|watch| !current.contains(&watch.pid)) {
        return Err(std::io::Error::other(
            "The listener changed; refresh the Servers list and retry",
        ));
    }
    for watch in &watches {
        watch.signal(libc::SIGTERM)?;
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    for watch in &watches {
        if !watch.wait_exit(deadline.saturating_duration_since(Instant::now()))? {
            watch.signal(libc::SIGKILL)?;
        }
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    for watch in &watches {
        if !watch.wait_exit(deadline.saturating_duration_since(Instant::now()))? {
            return Err(std::io::Error::other(
                "The server did not exit after being killed",
            ));
        }
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn listener_owners(session: u32, port: u16) -> std::io::Result<Vec<i32>> {
    use std::collections::HashSet;
    let mut sockets = HashSet::new();
    for table in ["tcp", "tcp6"] {
        match std::fs::read_to_string(format!("/proc/{session}/net/{table}")) {
            Ok(text) => {
                for line in text.lines().skip(1) {
                    let fields: Vec<_> = line.split_whitespace().collect();
                    if fields.len() > 9
                        && fields[3] == "0A"
                        && fields[1]
                            .rsplit_once(':')
                            .and_then(|(_, value)| u16::from_str_radix(value, 16).ok())
                            == Some(port)
                    {
                        sockets.insert(format!("socket:[{}]", fields[9]));
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && table == "tcp6" => {}
            Err(error) => return Err(error),
        }
    }
    if sockets.is_empty() {
        return Ok(Vec::new());
    }
    let mut pending = vec![session as i32];
    let mut seen = HashSet::new();
    let mut owners = Vec::new();
    while let Some(pid) = pending.pop() {
        if !seen.insert(pid) {
            continue;
        }
        if seen.len() > 4096 {
            return Err(std::io::Error::other(
                "The source pane has too many processes to inspect safely",
            ));
        }
        if !in_session(pid, session) {
            continue;
        }
        // Follow just this Pane's descendants, including children spawned by
        // worker threads. Never scan the machine's process table per Pane.
        if let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) {
            for task in tasks.flatten() {
                if let Ok(children) = std::fs::read_to_string(task.path().join("children")) {
                    pending.extend(
                        children
                            .split_whitespace()
                            .filter_map(|pid| pid.parse::<i32>().ok()),
                    );
                }
            }
        }
        if let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) {
            if fds.flatten().any(|fd| {
                std::fs::read_link(fd.path())
                    .ok()
                    .is_some_and(|path| sockets.contains(path.to_string_lossy().as_ref()))
            }) {
                owners.push(pid);
            }
        }
    }
    Ok(owners)
}

#[cfg(target_os = "macos")]
fn listener_owners(session: u32, port: u16) -> std::io::Result<Vec<i32>> {
    // macOS ships lsof. This explicit, port-scoped lookup is never a watcher
    // or a periodic process scan; kernel session ownership is checked below.
    let output = std::process::Command::new("/usr/sbin/lsof")
        .args(["-nP", "-a", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-Fp"])
        .output()?;
    if !output.status.success() && output.status.code() != Some(1) {
        return Err(std::io::Error::other(
            "Could not inspect the listening process",
        ));
    }
    let mut owners: Vec<_> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix('p')?.parse::<i32>().ok())
        .filter(|pid| in_session(*pid, session))
        .collect();
    owners.sort_unstable();
    owners.dedup();
    Ok(owners)
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
fn listener_owners(_session: u32, _port: u16) -> std::io::Result<Vec<i32>> {
    Err(std::io::Error::other(
        "Stopping listeners is not supported on this platform",
    ))
}
