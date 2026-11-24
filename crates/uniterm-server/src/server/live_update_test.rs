//! Exercise the kernel half of the proposed live update in a disposable
//! process. The destination is a test harness, not the production server.
use super::{Server, WorkspaceLock};
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

use crate::server::reaper::common;

const TEST: &str = "server::socket::live_update_test::live_pty_and_workspace_claims_survive_exec";

pub(super) fn helper(phase: &str, socket: &Path) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([TEST, "--exact", "--nocapture"])
        .env("UT_TEST_HANDOFF_PHASE", phase)
        .env("UT_TEST_HANDOFF_SOCKET", socket);
    command
}

fn line(reader: &mut impl BufRead) -> String {
    let mut line = String::new();
    assert!(reader.read_line(&mut line).unwrap() > 0);
    line.trim_end().to_owned()
}

fn source(socket: &Path) {
    common::isolate_state();
    let script = r#"stty -echo
shell_value=preserved
sh -c 'printf "worker %s\n" "$$"; read -r reply; printf "got %s %s\n" "$$" "$reply"'
printf 'shell %s %s\n' "$$" "$shell_value"
exit 17"#;
    let (server, _poll) = Server::bind(socket, "/bin/sh", &["-c", script], 80, 24).unwrap();
    let pty = &server.panes.values().next().unwrap().pty;
    let master = pty.raw_fd();
    // SAFETY: the server owns master for the lifetime of this function.
    let fd = unsafe { BorrowedFd::borrow_raw(master) };
    // Use blocking fixture I/O. No core loop is running in this child.
    let mut io = File::from(fd.try_clone_to_owned().unwrap());
    let flags = unsafe { libc::fcntl(master, libc::F_GETFL) };
    assert!(flags >= 0);
    assert_eq!(
        unsafe { libc::fcntl(master, libc::F_SETFL, flags & !libc::O_NONBLOCK) },
        0
    );
    let greeting = line(&mut BufReader::new(&io));
    let worker: i32 = greeting.strip_prefix("worker ").unwrap().parse().unwrap();
    io.write_all(b"from-old-").unwrap();
    let mut peer = UnixStream::connect(socket).unwrap();
    peer.write_all(b"queued").unwrap();

    let claim = server.workspace_lock.as_ref().unwrap();
    let listener = server.listener.as_raw_fd();
    let fds = [
        master,
        listener,
        peer.as_raw_fd(),
        claim._socket_claim.as_raw_fd(),
        claim._state_claim.as_raw_fd(),
    ];
    let mut destination = helper("destination", socket);
    destination
        .env(
            "UT_TEST_HANDOFF_LOCK",
            crate::persist::lock_path(socket.file_stem().unwrap().to_str().unwrap()),
        )
        .env(
            "UT_TEST_HANDOFF_IDS",
            format!(
                "{} {worker} {}",
                pty.child_pid().unwrap(),
                std::process::id()
            ),
        )
        .env(
            "UT_TEST_HANDOFF_FDS",
            fds.map(|fd| fd.to_string()).join(" "),
        );
    // SAFETY: only the isolated process running this test changes images.
    // pre_exec operates on its known-live descriptors with async-signal-safe
    // fcntl calls. The original durable lock descriptor must survive exec.
    unsafe {
        destination.pre_exec(move || {
            for fd in fds {
                if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    panic!("exec failed: {}", destination.exec());
}

fn destination(socket: &Path) {
    use std::io::Read as _;
    fn numbers<const N: usize>(key: &str) -> [i32; N] {
        let text = std::env::var(key).unwrap();
        let mut values = text.split_whitespace().map(|n| n.parse().unwrap());
        std::array::from_fn(|_| values.next().unwrap())
    }
    let [shell, worker, owner] = numbers("UT_TEST_HANDOFF_IDS");
    assert_eq!(std::process::id(), owner as u32);
    let fds: [RawFd; 5] = numbers("UT_TEST_HANDOFF_FDS");
    for fd in fds {
        // SAFETY: these are the descriptors retained by source. Resume normal
        // close-on-exec before launching the independent lock contender.
        assert_eq!(
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) },
            0
        );
    }
    let [master, listener, peer, _, _] = fds;
    // SAFETY: source transferred sole ownership of each original descriptor.
    let (mut master, listener, _peer) = unsafe {
        (
            File::from_raw_fd(master),
            UnixListener::from_raw_fd(listener),
            UnixStream::from_raw_fd(peer),
        )
    };
    let (mut stream, _) = listener.accept().unwrap();
    let mut queued = [0; 6];
    stream.read_exact(&mut queued).unwrap();
    assert_eq!(&queued, b"queued");
    assert!(helper("claim", socket).status().unwrap().success());
    let owned = master.as_fd().try_clone_to_owned().unwrap();
    assert!(crate::pty::PtyProcess::resume_after_exec(
        master.as_fd().try_clone_to_owned().unwrap(),
        worker
    )
    .is_err());
    assert!(crate::pty::PtyProcess::resume_after_exec(
        listener.as_fd().try_clone_to_owned().unwrap(),
        shell,
    )
    .is_err());
    let mut pane = crate::pty::PtyProcess::resume_after_exec(owned, shell).unwrap();
    assert_eq!(pane.child_pid(), Some(shell as u32));
    assert!(!pane.try_wait_exited());
    master.write_all(b"and-new\n").unwrap();
    let mut lines = BufReader::new(master);
    assert_eq!(line(&mut lines), format!("got {worker} from-old-and-new"));
    assert_eq!(line(&mut lines), format!("shell {shell} preserved"));
    // Only the resumed owner can collect the original child. A second wait
    // reads the cached status without touching a possibly reused numeric pid.
    assert_eq!(pane.wait_status().unwrap().unwrap().code(), Some(17));
    assert!(!pane.wait().unwrap());
    assert!(pane.try_wait_exited());
}

#[test]
fn live_pty_and_workspace_claims_survive_exec() {
    if let Ok(phase) = std::env::var("UT_TEST_HANDOFF_PHASE") {
        let path = std::env::var_os("UT_TEST_HANDOFF_SOCKET").unwrap();
        let socket = Path::new(&path);
        // SAFETY: this process runs only this one test. Bound a failed read.
        unsafe { libc::alarm(20) };
        match phase.as_str() {
            "source" => source(socket),
            "destination" => destination(socket),
            "claim" => {
                // Unit tests give every process a private durable root. Point
                // the contender at the exact inode claimed by the source.
                let state_lock =
                    crate::persist::lock_path(socket.file_stem().unwrap().to_str().unwrap());
                std::fs::create_dir_all(state_lock.parent().unwrap()).unwrap();
                std::fs::hard_link(
                    std::env::var_os("UT_TEST_HANDOFF_LOCK").unwrap(),
                    state_lock,
                )
                .unwrap();
                let other = socket
                    .parent()
                    .unwrap()
                    .join("other")
                    .join(socket.file_name().unwrap());
                for path in [socket, &other] {
                    assert_eq!(
                        WorkspaceLock::acquire(path).unwrap_err().kind(),
                        std::io::ErrorKind::AddrInUse
                    );
                }
            }
            _ => panic!("unexpected handoff phase"),
        }
        return;
    }
    let root = common::temp_dir("handoff");
    let socket = root.join(format!("{}.sock", common::unique_workspace_name()));
    let output = helper("source", &socket)
        .env("XDG_RUNTIME_DIR", &root)
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("TMPDIR", &root)
        .current_dir(&root)
        .output()
        .unwrap();
    std::fs::remove_dir_all(root).unwrap();
    assert!(output.status.success(), "{output:?}");
}
