//! Coalescing of terminal resize reports.
//!
//! A window-manager drag delivers SIGWINCH many times per second, and every
//! distinct size the server hears about costs a relayout, a scrollback reflow
//! for each pane, and a whole-frame repaint for every attached client. Holding
//! the size briefly and reporting only the settled geometry turns a storm of
//! intermediate sizes into one or two reflows (see `docs/00-vision-and-scope.md`
//! on latency and idle budgets). The hold is armed only by a real signal and
//! disarms when it flushes, so an idle client still blocks in poll without a
//! timeout.

use std::time::{Duration, Instant};

/// Quiet time after the last size change before the size is reported. One
/// display frame: a lone resize still applies promptly.
pub(crate) const SETTLE: Duration = Duration::from_millis(16);

/// Longest a continuous stream of changes may be held, so a slow drag still
/// repaints several times per second instead of only when the hand stops.
pub(crate) const MAX_HOLD: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy)]
struct Pending {
    size: (u16, u16),
    first: Instant,
    last: Instant,
}

/// The latest unreported terminal size and when it must be sent.
#[derive(Debug, Default)]
pub(crate) struct ResizeCoalescer {
    pending: Option<Pending>,
}

impl ResizeCoalescer {
    /// Record a size observed at `now`. An unchanged size is still recorded:
    /// after SIGCONT the client cannot trust the physical terminal and relies
    /// on the server answering even a same-size report with a repaint.
    pub(crate) fn note(&mut self, size: (u16, u16), now: Instant) {
        match &mut self.pending {
            Some(pending) => {
                pending.size = size;
                pending.last = now;
            }
            None => {
                self.pending = Some(Pending {
                    size,
                    first: now,
                    last: now,
                });
            }
        }
    }

    /// When the pending size is due, or `None` while nothing is pending.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.pending
            .map(|pending| (pending.last + SETTLE).min(pending.first + MAX_HOLD))
    }

    /// Poll timeout that wakes the loop exactly when the pending size is due.
    pub(crate) fn timeout(&self, now: Instant) -> Option<Duration> {
        self.deadline()
            .map(|deadline| deadline.saturating_duration_since(now))
    }

    /// Take the pending size now, regardless of its deadline. Keyboard input
    /// typed after a resize must not overtake it: a program that queries its
    /// window size on the next keystroke has to see the new geometry.
    pub(crate) fn take_pending(&mut self) -> Option<(u16, u16)> {
        self.pending.take().map(|pending| pending.size)
    }

    /// Take the pending size if its deadline has passed.
    pub(crate) fn take_due(&mut self, now: Instant) -> Option<(u16, u16)> {
        if self.deadline().is_some_and(|deadline| deadline <= now) {
            self.pending.take().map(|pending| pending.size)
        } else {
            None
        }
    }
}

/// The earlier of two optional timeouts; `None` means wait indefinitely.
pub(crate) fn min_timeout(a: Option<Duration>, b: Option<Duration>) -> Option<Duration> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, None) => a,
        (None, b) => b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_pending_means_no_timeout_and_nothing_to_send() {
        let mut resizes = ResizeCoalescer::default();
        let now = Instant::now();
        assert_eq!(resizes.timeout(now), None);
        assert_eq!(resizes.take_due(now), None);
    }

    #[test]
    fn a_lone_resize_is_held_one_settle_period_then_sent_once() {
        let mut resizes = ResizeCoalescer::default();
        let start = Instant::now();
        resizes.note((100, 40), start);
        assert_eq!(resizes.timeout(start), Some(SETTLE));
        assert_eq!(resizes.take_due(start + SETTLE / 2), None);
        assert_eq!(resizes.take_due(start + SETTLE), Some((100, 40)));
        assert_eq!(resizes.take_due(start + SETTLE), None);
        assert_eq!(resizes.timeout(start + SETTLE), None);
    }

    #[test]
    fn a_storm_settles_to_its_final_size_and_never_loses_it() {
        let mut resizes = ResizeCoalescer::default();
        let start = Instant::now();
        let step = Duration::from_millis(10);
        let mut sent = Vec::new();
        for i in 0..20u16 {
            let now = start + step * u32::from(i);
            if let Some(size) = resizes.take_due(now) {
                sent.push(size);
            }
            resizes.note((80 + i, 24 + i), now);
        }
        // Ten millisecond steps never let the settle period lapse, so only the
        // hold cap forces a report mid-storm.
        assert_eq!(sent, vec![(89, 33)]);
        let after = start + step * 19 + SETTLE;
        assert_eq!(resizes.take_due(after), Some((99, 43)));
        assert_eq!(resizes.timeout(after), None);
    }

    #[test]
    fn the_hold_cap_bounds_latency_during_a_continuous_drag() {
        let mut resizes = ResizeCoalescer::default();
        let start = Instant::now();
        resizes.note((80, 24), start);
        let mut now = start;
        while now < start + MAX_HOLD {
            now += Duration::from_millis(5);
            resizes.note((80, 24), now);
            assert!(resizes.deadline().unwrap() <= start + MAX_HOLD);
        }
        assert_eq!(resizes.take_due(now), Some((80, 24)));
    }

    #[test]
    fn input_flushes_the_pending_size_ahead_of_its_deadline() {
        let mut resizes = ResizeCoalescer::default();
        let now = Instant::now();
        resizes.note((120, 40), now);
        assert_eq!(resizes.take_pending(), Some((120, 40)));
        assert_eq!(resizes.take_pending(), None);
        assert_eq!(resizes.timeout(now), None);
    }

    #[test]
    fn the_same_size_is_still_reported_after_a_resume() {
        let mut resizes = ResizeCoalescer::default();
        let now = Instant::now();
        resizes.note((80, 24), now);
        assert_eq!(resizes.take_due(now + SETTLE), Some((80, 24)));
    }

    #[test]
    fn the_earlier_timeout_wins_and_none_means_forever() {
        let a = Some(Duration::from_millis(5));
        let b = Some(Duration::from_millis(9));
        assert_eq!(min_timeout(a, b), a);
        assert_eq!(min_timeout(None, b), b);
        assert_eq!(min_timeout(a, None), a);
        assert_eq!(min_timeout(None, None), None);
    }
}
