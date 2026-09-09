//! Event-driven ownership of closed PTYs. A removed Pane leaves no grid here:
//! only its process and descriptors remain until TERM, KILL, and kernel exit.

use super::*;
use std::time::{Duration, Instant};

const TERM_GRACE: Duration = Duration::from_millis(15);
const EXIT_FALLBACK_BOUND: Duration = Duration::from_secs(2);
const DRAIN_BYTES: usize = 64 * 1024;

pub(super) struct RetiringPty {
    pty: PtyProcess,
    watch: Option<crate::process_watch::ProcessWatch>,
    exit_token: Token,
    master_registered: bool,
    read_pending: bool,
    child_exited: bool,
    kill_at: Option<Instant>,
    fallback_at: Option<Instant>,
}

impl Server {
    /// Remove UI ownership immediately while preserving the process and its
    /// master until the kernel reports exit. No sleep or blocking wait occurs.
    pub(super) fn retire_pty(&mut self, reg: &Registry, mut pty: PtyProcess) {
        let master_token = Token(self.next_token);
        let exit_token = Token(self.next_token + 1);
        self.next_token += 2;
        if let Err(error) = pty.begin_terminate() {
            eprintln!("uniterm: could not terminate closed Pane: {error}");
        }
        let watch = pty
            .child_pid()
            .and_then(|pid| crate::process_watch::ProcessWatch::new(pid as i32).ok())
            .and_then(|mut watch| watch.register(reg, exit_token).ok().map(|()| watch));
        let master_registered = reg
            .register(
                &mut SourceFd(&pty.raw_fd()),
                master_token,
                mio::Interest::READABLE,
            )
            .is_ok();
        if watch.is_some() {
            self.retiring_exits.insert(exit_token, master_token);
        }
        self.retiring_ptys.insert(
            master_token,
            RetiringPty {
                pty,
                watch,
                exit_token,
                master_registered,
                read_pending: true,
                child_exited: false,
                kill_at: Some(Instant::now() + TERM_GRACE),
                fallback_at: None,
            },
        );
    }

    pub(super) fn next_reap_deadline(&self) -> Option<Instant> {
        self.retiring_ptys
            .values()
            .flat_map(|pty| {
                pty.kill_at
                    .into_iter()
                    .chain(pty.fallback_at)
                    .chain(pty.read_pending.then(Instant::now))
            })
            .min()
    }

    pub(super) fn on_retiring_pty(&mut self, reg: &Registry, token: Token) -> bool {
        if let Some(master) = self.retiring_exits.remove(&token) {
            if let Some(retiring) = self.retiring_ptys.get_mut(&master) {
                if let Some(mut watch) = retiring.watch.take() {
                    watch.deregister(reg);
                }
                // Keep the zombie leader until KILL has been sent to its
                // group, so its pid cannot be reused during TERM grace.
                if retiring.kill_at.is_none() {
                    retiring.child_exited = retiring.pty.try_wait_exited();
                }
                retiring.read_pending = true;
            }
            true
        } else if let Some(retiring) = self.retiring_ptys.get_mut(&token) {
            retiring.read_pending = true;
            true
        } else {
            false
        }
    }

    pub(super) fn flush_retiring_ptys(&mut self, reg: &Registry) {
        let now = Instant::now();
        self.retiring_ptys.retain(|_, retiring| {
            if retiring.kill_at.is_some_and(|deadline| now >= deadline) {
                if let Err(error) = retiring.pty.force_terminate() {
                    eprintln!("uniterm: could not kill closed Pane: {error}");
                }
                retiring.kill_at = None;
                retiring.child_exited |= retiring.pty.try_wait_exited();
                retiring.read_pending = true;
                if !retiring.child_exited && retiring.watch.is_none() {
                    // A failed pidfd/kqueue registration cannot introduce an
                    // idle poll. Make one bounded final reap attempt instead.
                    retiring.fallback_at = Some(now + EXIT_FALLBACK_BOUND);
                }
            }
            if retiring.read_pending {
                retiring.drain(reg);
            }
            let fallback_expired = retiring.fallback_at.is_some_and(|deadline| now >= deadline);
            if fallback_expired {
                retiring.child_exited |= retiring.pty.try_wait_exited();
                if !retiring.child_exited {
                    eprintln!(
                        "uniterm: closed Pane could not be reaped after native exit-watch failure"
                    );
                }
            }
            let complete =
                retiring.kill_at.is_none() && (retiring.child_exited || fallback_expired);
            if complete {
                if retiring.master_registered {
                    let _ = reg.deregister(&mut SourceFd(&retiring.pty.raw_fd()));
                }
                if let Some(mut watch) = retiring.watch.take() {
                    watch.deregister(reg);
                }
                self.retiring_exits.remove(&retiring.exit_token);
            }
            !complete
        });
    }

    /// The live loop has ended, so finish pending teardown against the same
    /// descriptors before releasing Workspace ownership. The bound covers a
    /// child stuck in the kernel; no per-Pane sleeps accumulate at shutdown.
    pub(super) fn drain_retiring_ptys(&mut self, poll: &mut Poll) {
        let deadline = Instant::now() + EXIT_FALLBACK_BOUND + TERM_GRACE;
        let mut events = Events::with_capacity(64);
        while !self.retiring_ptys.is_empty() && Instant::now() < deadline {
            self.flush_retiring_ptys(poll.registry());
            if self.retiring_ptys.is_empty() {
                break;
            }
            let next = self.next_reap_deadline().unwrap_or(deadline).min(deadline);
            if poll
                .poll(
                    &mut events,
                    Some(next.saturating_duration_since(Instant::now())),
                )
                .is_err()
            {
                continue;
            }
            for event in events.iter() {
                self.on_retiring_pty(poll.registry(), event.token());
            }
        }
    }
}

impl RetiringPty {
    fn drain(&mut self, reg: &Registry) {
        let mut buffer = [0u8; 4096];
        let mut total = 0;
        self.read_pending = false;
        while total < DRAIN_BYTES {
            match self.pty.read(&mut buffer) {
                Ok(0) => {}
                Ok(read) => {
                    total += read;
                    continue;
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return,
                Err(_) => {}
            }
            if self.master_registered {
                let _ = reg.deregister(&mut SourceFd(&self.pty.raw_fd()));
                self.master_registered = false;
            }
            return;
        }
        self.read_pending = total >= DRAIN_BYTES;
    }
}

#[cfg(test)]
#[path = "../../tests/common/mod.rs"]
mod common;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closing_a_pane_returns_before_grace_and_reaps_from_kernel_events() {
        common::isolate_state();
        let dir = common::socket_root().join(format!("ut-reap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join(format!("{}.sock", common::unique_workspace_name()));
        let (mut server, mut poll) = Server::bind(
            &socket,
            "/bin/sh",
            &[
                "-c",
                "trap '' TERM; printf ready; while :; do read -r line; done",
            ],
            80,
            24,
        )
        .unwrap();
        let id = server.windows[0].active;
        let pid = server.panes[&id].pty.child_pid().unwrap();
        let other = server
            .spawn_pane(
                poll.registry(),
                &[
                    "-c",
                    "while read -r line; do printf 'other:%s\\n' \"$line\"; done",
                ],
            )
            .unwrap();
        server.push_window(other);
        server.relayout();
        let mut buffer = [0; 32];
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if server
                .panes
                .get_mut(&id)
                .unwrap()
                .pty
                .read(&mut buffer)
                .is_ok_and(|n| n != 0)
            {
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        let started = Instant::now();
        server.close_pane(poll.registry(), id);
        let elapsed = started.elapsed();
        assert!(!server.panes.contains_key(&id));
        assert_eq!(server.retiring_ptys.len(), 1);
        let retiring = server.retiring_ptys.values_mut().next().unwrap();
        assert!(retiring.kill_at.is_some());
        assert!(
            !retiring.pty.try_wait_exited(),
            "close synchronously killed the TERM-resistant child"
        );
        // Stretch only this test's grace period to prove that live Pane I/O
        // proceeds independently of unfinished process teardown.
        retiring.kill_at = Some(Instant::now() + Duration::from_secs(10));
        assert!(server.running);
        let other_pane = server.panes.get_mut(&other).unwrap();
        let token = other_pane.token;
        assert!(Server::queue_pane_input(
            poll.registry(),
            other_pane,
            b"responsive\n"
        ));
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            server.on_pty(poll.registry(), token, true, false);
            if server.panes[&other]
                .term
                .dump_text()
                .contains("other:responsive")
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "live Pane stalled behind teardown"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        let retiring = server.retiring_ptys.values_mut().next().unwrap();
        assert!(!retiring.pty.try_wait_exited());
        retiring.kill_at = Some(Instant::now());
        eprintln!(
            "Pane close returned in {} us; teardown remains pending",
            elapsed.as_micros()
        );
        server.drain_retiring_ptys(&mut poll);
        server.close_pane(poll.registry(), other);
        server.drain_retiring_ptys(&mut poll);
        assert!(server.retiring_ptys.is_empty());
        assert!(server.retiring_exits.is_empty());
        assert!(server.next_reap_deadline().is_none());
        // SAFETY: waitpid targets only this test's known child, non-blocking.
        assert_eq!(
            unsafe { libc::waitpid(pid as i32, std::ptr::null_mut(), libc::WNOHANG) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
        // No teardown work may keep the idle poller awake after it drains.
        let mut events = Events::with_capacity(8);
        poll.poll(&mut events, Some(Duration::ZERO)).unwrap();
        assert!(events
            .iter()
            .all(|event| !server.on_retiring_pty(poll.registry(), event.token())));
    }
}
