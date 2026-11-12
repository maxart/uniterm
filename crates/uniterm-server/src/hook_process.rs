//! Detached hook ownership follows kernel parent links, not a reusable Pane.
//! Called only for a received hook report on the agent runtime's worker pool.

/// A hook must descend from the Pane's current foreground process group.
/// Walking at most 64 exact parents does no process-table scan or idle work.
pub(crate) fn belongs_to_invocation(mut source: i32, foreground: i32) -> bool {
    if source <= 0 || foreground <= 0 {
        return false;
    }
    for _ in 0..64 {
        // SAFETY: getpgid accepts a PID and returns an error for an exited
        // process; it neither signals nor changes the target process.
        let group = unsafe { libc::getpgid(source) };
        if group < 0 {
            return false;
        }
        if source == foreground || group == foreground {
            return true;
        }
        let Some(parent) = parent_pid(source) else {
            return false;
        };
        if parent <= 1 || parent == source {
            return false;
        }
        source = parent;
    }
    false
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn parent_pid(pid: i32) -> Option<i32> {
    use std::io::Read as _;
    let mut stat = String::new();
    std::fs::File::open(format!("/proc/{pid}/stat"))
        .ok()?
        .take(4096)
        .read_to_string(&mut stat)
        .ok()?;
    // comm may contain spaces and parentheses. Fields after its final ')'
    // start with state, then PPID.
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

#[cfg(target_os = "macos")]
fn parent_pid(pid: i32) -> Option<i32> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let size = std::mem::size_of::<libc::proc_bsdinfo>();
    // SAFETY: the buffer has exactly the size required by PROC_PIDTBSDINFO.
    // It is read only after the kernel reports a complete initialized value.
    let received = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size as i32,
        )
    };
    if received != size as i32 {
        return None;
    }
    // SAFETY: proc_pidinfo initialized the complete structure above.
    i32::try_from(unsafe { info.assume_init() }.pbi_ppid).ok()
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
fn parent_pid(_pid: i32) -> Option<i32> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead as _, Write as _};
    use std::os::unix::process::CommandExt as _;
    use std::process::{Command, Stdio};

    #[test]
    fn detached_hook_retains_exact_parent_ownership() {
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "printf 'ready\\n'; read line"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        // SAFETY: setsid is async-signal-safe; the child does no allocation
        // or locking between fork and exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        let mut ready = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(ready, "ready\n");
        let pid = child.id() as i32;
        assert!(belongs_to_invocation(pid, std::process::id() as i32));
        assert!(!belongs_to_invocation(pid, i32::MAX));
        child.stdin.take().unwrap().write_all(b"done\n").unwrap();
        assert!(child.wait().unwrap().success());
        assert!(!belongs_to_invocation(pid, std::process::id() as i32));
        assert!(!belongs_to_invocation(0, std::process::id() as i32));
    }
}
