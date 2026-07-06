//! Headless idle/hard-deadline watchdog. See T7 in the implementation plan.
//!
//! Tracks two independent timers:
//! - **Idle window** — sliding window reset by [`Watchdog::note_activity`].
//! - **Hard deadline** — absolute ceiling fixed at construction; cannot be reset.
//!
//! [`Watchdog::tick`] sleeps until whichever deadline arrives first, then reports
//! which one fired. Uses [`tokio::time::Instant`] / [`tokio::time::sleep_until`]
//! so `#[tokio::test(start_paused = true)]` paused-time mode advances the clock
//! deterministically.

use std::time::Duration;
use tokio::time::Instant;

pub struct Watchdog {
    idle_window: Duration,
    last_activity: Instant,
    hard_deadline: Option<Instant>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum WatchdogTick {
    Continue,
    IdleExpired,
    HardExpired,
}

impl Watchdog {
    pub fn new(idle: Duration, hard: Option<Duration>) -> Self {
        let now = Instant::now();
        Self { idle_window: idle, last_activity: now, hard_deadline: hard.map(|h| now + h) }
    }

    pub fn note_activity(&mut self) {
        self.last_activity = Instant::now();
    }

    /// Soonest deadline this watchdog will fire at — the earlier of `last_activity + idle_window`
    /// and `hard_deadline` (when set). Exposed for tests under paused tokio time, where blocking
    /// on `tick()` to assert the idle reset is impractical.
    pub fn next_deadline(&self) -> Instant {
        let idle_at = self.last_activity + self.idle_window;
        match self.hard_deadline {
            Some(h) if h < idle_at => h,
            _ => idle_at,
        }
    }

    pub async fn tick(&mut self) -> WatchdogTick {
        tokio::time::sleep_until(self.next_deadline()).await;
        let now = Instant::now();
        if self.hard_deadline.is_some_and(|h| now >= h) {
            return WatchdogTick::HardExpired;
        }
        if now >= self.last_activity + self.idle_window {
            return WatchdogTick::IdleExpired;
        }
        WatchdogTick::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn idle_expires_after_window() {
        let mut wd = Watchdog::new(Duration::from_secs(10), None);
        tokio::time::advance(Duration::from_secs(11)).await;
        assert_eq!(wd.tick().await, WatchdogTick::IdleExpired);
    }

    #[tokio::test(start_paused = true)]
    async fn activity_resets_idle() {
        // Under `start_paused`, calling `tick()` would block forever waiting for
        // a deadline that only moves when the test calls `tokio::time::advance`.
        // To assert the reset semantics without that dance, we observe the
        // `next_deadline()` accessor: after `note_activity()`, the deadline must
        // shift forward to `now + idle_window`.
        let mut wd = Watchdog::new(Duration::from_secs(10), None);
        tokio::time::advance(Duration::from_secs(8)).await;
        let before = wd.next_deadline();
        wd.note_activity();
        let after = wd.next_deadline();
        assert!(
            after > before,
            "expected deadline to advance after note_activity(); before={before:?} after={after:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn hard_deadline_expires_even_with_activity() {
        let mut wd = Watchdog::new(Duration::from_secs(10), Some(Duration::from_secs(15)));
        for _ in 0..20 {
            tokio::time::advance(Duration::from_secs(1)).await;
            wd.note_activity();
        }
        assert_eq!(wd.tick().await, WatchdogTick::HardExpired);
    }
}
