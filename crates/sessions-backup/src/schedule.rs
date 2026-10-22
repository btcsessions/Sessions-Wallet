//! When to upload the state backup. Pure logic over a millisecond clock so it
//! can be tested with a fake one.
//!
//! * A change marks the backup dirty; it goes out [`Policy::debounce_ms`]
//!   after the last change (a burst of edits is one upload), but no later
//!   than [`Policy::max_delay_ms`] after the first.
//! * At most one upload per [`Policy::min_interval_ms`].
//! * A failed upload retries with exponential backoff, from
//!   [`Policy::backoff_base_ms`] up to [`Policy::backoff_max_ms`].
//! * `force` (Back up now) skips the waiting, but never runs two at once.
//! * Changes made while an upload runs keep the backup dirty afterwards.

#[derive(Debug, Clone, Copy)]
pub struct Policy {
    pub debounce_ms: u64,
    pub max_delay_ms: u64,
    pub min_interval_ms: u64,
    pub backoff_base_ms: u64,
    pub backoff_max_ms: u64,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            debounce_ms: 5_000,
            max_delay_ms: 30_000,
            min_interval_ms: 60_000,
            backoff_base_ms: 15_000,
            backoff_max_ms: 30 * 60_000,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct Scheduler {
    policy: Policy,
    /// Bumped on every change.
    generation: u64,
    /// The generation the last finished upload covered.
    uploaded_generation: u64,
    first_change: Option<u64>,
    last_change: Option<u64>,
    last_attempt: Option<u64>,
    failures: u32,
    forced: bool,
    /// The generation the running upload covers.
    running: Option<u64>,
}

impl Scheduler {
    pub fn new(policy: Policy) -> Self {
        Scheduler {
            policy,
            ..Default::default()
        }
    }

    /// Something in the backed-up state changed.
    pub fn touch(&mut self, now: u64) {
        self.generation += 1;
        self.first_change.get_or_insert(now);
        self.last_change = Some(now);
    }

    /// Upload as soon as possible (Back up now, or a stale backup at unlock).
    pub fn force(&mut self, now: u64) {
        self.touch(now);
        self.forced = true;
    }

    /// Changes not uploaded yet.
    pub fn pending(&self) -> bool {
        self.generation != self.uploaded_generation
    }

    pub fn running(&self) -> bool {
        self.running.is_some()
    }

    /// When the next upload should start; `None` when there's nothing to do
    /// (or one is running).
    pub fn next_due(&self) -> Option<u64> {
        if !self.pending() || self.running.is_some() {
            return None;
        }
        let p = &self.policy;
        let (first, last) = (
            self.first_change.unwrap_or(0),
            self.last_change.unwrap_or(0),
        );
        let mut due = if self.forced {
            0
        } else {
            (last + p.debounce_ms).min(first + p.max_delay_ms)
        };
        if let Some(at) = self.last_attempt {
            if self.failures > 0 {
                let backoff = p
                    .backoff_base_ms
                    .saturating_mul(1 << (self.failures - 1).min(20))
                    .min(p.backoff_max_ms);
                due = due.max(at + backoff);
            } else if !self.forced {
                due = due.max(at + p.min_interval_ms);
            }
        }
        Some(due)
    }

    /// Start an upload now if one is due. Returns whether to go.
    pub fn start(&mut self, now: u64) -> bool {
        match self.next_due() {
            Some(due) if due <= now => {
                self.running = Some(self.generation);
                self.forced = false;
                true
            }
            _ => false,
        }
    }

    /// Start an upload now, whatever the timers say (the user pressed Back
    /// up now), unless one is already running.
    pub fn start_now(&mut self, now: u64) -> bool {
        if self.running.is_some() {
            return false;
        }
        self.touch(now);
        self.running = Some(self.generation);
        self.forced = false;
        true
    }

    /// The upload couldn't even start (e.g. the wallet locked): nothing
    /// changes but it's no longer running.
    pub fn cancel(&mut self) {
        self.running = None;
    }

    /// The upload went out (or there was nothing new to upload: `uploaded`
    /// false, which doesn't count against the rate limit).
    pub fn succeeded(&mut self, now: u64, uploaded: bool) {
        if let Some(g) = self.running.take() {
            self.uploaded_generation = g;
        }
        if uploaded {
            self.last_attempt = Some(now);
        }
        self.failures = 0;
        if self.pending() {
            // Changed during the upload: debounce from now.
            self.first_change = Some(now);
        } else {
            self.first_change = None;
            self.last_change = None;
        }
    }

    pub fn failed(&mut self, now: u64) {
        self.running = None;
        self.last_attempt = Some(now);
        self.failures = self.failures.saturating_add(1);
    }

    pub fn failures(&self) -> u32 {
        self.failures
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1000;

    fn sched() -> Scheduler {
        Scheduler::new(Policy::default())
    }

    #[test]
    fn idle_until_touched() {
        let s = sched();
        assert!(!s.pending());
        assert_eq!(s.next_due(), None);
    }

    #[test]
    fn debounces_bursts() {
        let mut s = sched();
        s.touch(0);
        assert_eq!(s.next_due(), Some(5 * S));
        s.touch(3 * S);
        assert_eq!(s.next_due(), Some(8 * S));
        assert!(!s.start(7 * S));
        assert!(s.start(8 * S));
        assert!(s.running());
        assert_eq!(s.next_due(), None);
        s.succeeded(9 * S, true);
        assert!(!s.pending());
        assert_eq!(s.next_due(), None);
    }

    #[test]
    fn steady_edits_still_upload_by_max_delay() {
        let mut s = sched();
        for t in 0..40 {
            s.touch(t * S);
        }
        // Last change at 39 s would debounce to 44 s, but the first was at 0.
        assert_eq!(s.next_due(), Some(30 * S));
    }

    #[test]
    fn at_most_one_upload_per_minute() {
        let mut s = sched();
        s.touch(0);
        assert!(s.start(5 * S));
        s.succeeded(6 * S, true);
        s.touch(10 * S);
        // Debounce says 15 s, the rate limit says 66 s.
        assert_eq!(s.next_due(), Some(66 * S));
        assert!(!s.start(15 * S));
        assert!(s.start(66 * S));
    }

    #[test]
    fn unchanged_content_doesnt_use_up_the_rate_limit() {
        let mut s = sched();
        s.touch(0);
        assert!(s.start(5 * S));
        s.succeeded(5 * S, false);
        s.touch(6 * S);
        assert_eq!(s.next_due(), Some(11 * S));
    }

    #[test]
    fn backs_off_after_failures_then_recovers() {
        let mut s = sched();
        s.touch(0);
        assert!(s.start(5 * S));
        s.failed(6 * S);
        assert!(s.pending());
        assert_eq!(s.next_due(), Some(21 * S)); // 15 s
        assert!(s.start(21 * S));
        s.failed(21 * S);
        assert_eq!(s.next_due(), Some(51 * S)); // 30 s
        assert!(s.start(51 * S));
        s.failed(51 * S);
        assert_eq!(s.next_due(), Some(111 * S)); // 60 s
        for _ in 0..20 {
            s.failed(1000 * S);
        }
        assert_eq!(s.next_due(), Some(1000 * S + 30 * 60 * S)); // capped
        assert!(s.start(1000 * S + 30 * 60 * S));
        s.succeeded(3000 * S, true);
        assert_eq!(s.failures(), 0);
        assert!(!s.pending());
    }

    #[test]
    fn force_skips_debounce_and_rate_limit_not_backoff() {
        let mut s = sched();
        s.touch(0);
        assert!(s.start(5 * S));
        s.succeeded(5 * S, true);
        s.force(6 * S);
        assert_eq!(s.next_due(), Some(0));
        assert!(s.start(6 * S));
        s.failed(7 * S);
        s.force(8 * S);
        // Still waits out the backoff after a failure.
        assert_eq!(s.next_due(), Some(22 * S));
    }

    #[test]
    fn start_now_ignores_backoff_but_not_a_running_upload() {
        let mut s = sched();
        s.touch(0);
        assert!(s.start(5 * S));
        assert!(!s.start_now(6 * S));
        s.failed(6 * S);
        assert!(s.start_now(7 * S));
        s.succeeded(8 * S, true);
        assert!(!s.pending());
        s.touch(9 * S);
        assert!(s.start(70 * S));
        s.cancel();
        assert!(s.pending());
        assert!(!s.running());
    }

    #[test]
    fn changes_during_an_upload_stay_pending() {
        let mut s = sched();
        s.touch(0);
        assert!(s.start(5 * S));
        s.touch(6 * S);
        // Never two at once.
        assert_eq!(s.next_due(), None);
        s.succeeded(7 * S, true);
        assert!(s.pending());
        assert_eq!(s.next_due(), Some(67 * S));
    }
}
