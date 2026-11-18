//! Restart policy bookkeeping.
//!
//! Everything in this module is a pure function of (policy, exit status,
//! restart budget) plus the current time, so the decision the reaper makes
//! can be reasoned about and unit-tested without an executor. Randomness
//! (backoff jitter) is confined to [`jittered_backoff`].

use rand::Rng;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// What the reaper does with an exited unit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RestartLimits {
    /// Base of the exponential backoff between restarts (1s by default,
    /// doubled per restart up to [`RestartLimits::cap`]).
    pub base: Duration,
    /// Upper bound of the backoff between restarts.
    pub cap: Duration,
    /// Maximum number of restart attempts inside [`RestartLimits::window`].
    /// Past the limit the unit becomes terminally failed instead of restarting.
    pub max_attempts: u32,
    /// Sliding window over which restart attempts are counted.
    pub window: Duration,
    /// Symmetric jitter applied to the backoff, as a fraction of the
    /// computed delay (0.25 = ±25%). Prevents restart storms when many
    /// units fail against the same dependency at once.
    pub jitter: f64,
}

impl Default for RestartLimits {
    fn default() -> Self {
        Self {
            base: Duration::from_secs(1),
            cap: Duration::from_secs(30),
            max_attempts: 5,
            window: Duration::from_secs(600),
            jitter: 0.25,
        }
    }
}

/// Per-child restart policy, configured by the spawning unit at spawn time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RestartPolicy {
    /// Never restart, no matter how the unit exited. A failure still lands
    /// in the ring and stays visible until reaped.
    Never,
    /// Restart only on failure-class exits (`Failure`, `Panicked`,
    /// `TimedOut`). `Success` and `Cancelled` are not failures.
    OnFailure,
    /// Restart no matter how the unit exited. A *requested* stop
    /// (see [`crate::Init::cancel`]) still suppresses the restart.
    Always,
}

/// The decision made by the reaper for a zombie.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PolicyDecision {
    /// Do not restart. The zombie is removed from the registry at reap time.
    Remove,
    /// Do not restart: the start limit was breached. The unit is moved to
    /// `FailedTerminal` and stays visible in the registry.
    FailedTerminal,
    /// Restart once the backoff has elapsed. The unit moves to `Restarting`;
    /// the actual re-creation happens when the backoff expires.
    ScheduleRestart {
        /// How long to wait before re-creating the unit's body.
        after: Duration,
    },
}

impl RestartLimits {
    /// The exponential backoff for the `restart_count`-th restart, capped.
    /// Jitter is applied by the caller via [`jittered_backoff`].
    fn backoff(self, restart_count: u32) -> Duration {
        // 1u32 << 31 still fits a u32, and saturating_mul handles the
        // Duration range itself, so no input can overflow this.
        let exponent = restart_count.min(31);
        self.base.saturating_mul(1u32 << exponent).min(self.cap)
    }
}

/// The backoff for the next restart, jittered symmetrically by
/// [`RestartLimits::jitter`].
pub(crate) fn jittered_backoff(restart_count: u32, limits: &RestartLimits) -> Duration {
    let base = limits.backoff(restart_count);
    let jitter = limits.jitter.clamp(0.0, 0.99);
    if jitter == 0.0 {
        return base;
    }
    let factor = 1.0 - jitter + rand::rng().random_range(0.0..(2.0 * jitter));
    Duration::from_secs_f64(base.as_secs_f64() * factor)
}

/// Decide what happens to a zombie, without touching any state.
///
/// `recent_attempts` is the number of restart attempts already recorded
/// inside the current window (after eviction). The attempt is *not*
/// consumed here: it is consumed when the backoff expires and the body is
/// actually re-created, so a stop requested during the backoff does not
/// burn the unit's budget.
pub(crate) fn evaluate(
    policy: RestartPolicy,
    status: &crate::exit::ExitStatus,
    stop_requested: bool,
    restart_count: u32,
    recent_attempts: usize,
    limits: &RestartLimits,
) -> PolicyDecision {
    if stop_requested {
        // a requested stop is never second-guessed by a restart policy
        return PolicyDecision::Remove;
    }
    let wants_restart = match policy {
        RestartPolicy::Never => false,
        RestartPolicy::OnFailure => status.is_failure(),
        RestartPolicy::Always => true,
    };
    if !wants_restart {
        return PolicyDecision::Remove;
    }
    if recent_attempts >= limits.max_attempts as usize {
        return PolicyDecision::FailedTerminal;
    }
    PolicyDecision::ScheduleRestart {
        after: jittered_backoff(restart_count, limits),
    }
}

/// Sliding window of restart attempts, used to enforce the start limit.
///
/// `VecDeque` bounded by [`RestartLimits::max_attempts`]: once the limit is
/// reached no more attempts are recorded, so the deque cannot grow past it.
pub(crate) struct AttemptWindow {
    attempts: VecDeque<Instant>,
}

impl AttemptWindow {
    pub(crate) fn new() -> Self {
        Self {
            attempts: VecDeque::new(),
        }
    }

    fn evict(&mut self, now: Instant, window: Duration) {
        while let Some(front) = self.attempts.front() {
            if now.duration_since(*front) > window {
                self.attempts.pop_front();
            } else {
                break;
            }
        }
    }

    /// Attempts recorded inside the window at `now`, evicting stale ones.
    pub(crate) fn len_at(&mut self, now: Instant, window: Duration) -> usize {
        self.evict(now, window);
        self.attempts.len()
    }

    /// Record a restart attempt at `now`, evicting stale ones.
    pub(crate) fn record(&mut self, now: Instant, window: Duration) {
        self.evict(now, window);
        self.attempts.push_back(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exit::ExitStatus;

    fn limits() -> RestartLimits {
        RestartLimits::default()
    }

    #[test]
    fn never_never_restarts() {
        for status in [
            ExitStatus::Success,
            ExitStatus::failure("x"),
            ExitStatus::Panicked,
            ExitStatus::TimedOut,
        ] {
            let decision = evaluate(RestartPolicy::Never, &status, false, 0, 0, &limits());
            assert_eq!(decision, PolicyDecision::Remove);
        }
    }

    #[test]
    fn on_failure_only_on_failures() {
        let limits = limits();
        assert_eq!(
            evaluate(
                RestartPolicy::OnFailure,
                &ExitStatus::Success,
                false,
                0,
                0,
                &limits
            ),
            PolicyDecision::Remove
        );
        assert_eq!(
            evaluate(
                RestartPolicy::OnFailure,
                &ExitStatus::Cancelled,
                false,
                0,
                0,
                &limits
            ),
            PolicyDecision::Remove
        );
        for status in [
            ExitStatus::failure("boom"),
            ExitStatus::Panicked,
            ExitStatus::TimedOut,
        ] {
            assert!(matches!(
                evaluate(RestartPolicy::OnFailure, &status, false, 0, 0, &limits),
                PolicyDecision::ScheduleRestart { .. }
            ));
        }
    }

    #[test]
    fn always_restarts_on_success() {
        let decision = evaluate(
            RestartPolicy::Always,
            &ExitStatus::Success,
            false,
            0,
            0,
            &limits(),
        );
        assert!(matches!(decision, PolicyDecision::ScheduleRestart { .. }));
    }

    #[test]
    fn stop_requested_suppresses_restart() {
        let limits = limits();
        for policy in [
            RestartPolicy::Always,
            RestartPolicy::OnFailure,
            RestartPolicy::Never,
        ] {
            for status in [ExitStatus::Panicked, ExitStatus::Success] {
                assert_eq!(
                    evaluate(policy, &status, true, 0, 0, &limits),
                    PolicyDecision::Remove
                );
            }
        }
    }

    #[test]
    fn start_limit_breach_yields_failed_terminal() {
        let limits = limits();
        let decision = evaluate(
            RestartPolicy::OnFailure,
            &ExitStatus::Panicked,
            false,
            4,
            5, // five attempts already in the window
            &limits,
        );
        assert_eq!(decision, PolicyDecision::FailedTerminal);
    }

    #[test]
    fn backoff_grows_and_caps() {
        let limits = limits();
        assert_eq!(limits.backoff(0), Duration::from_secs(1));
        assert_eq!(limits.backoff(1), Duration::from_secs(2));
        assert_eq!(limits.backoff(4), Duration::from_secs(16));
        assert_eq!(limits.backoff(5), Duration::from_secs(30)); // 32 capped
        assert_eq!(limits.backoff(31), Duration::from_secs(30));
    }

    #[test]
    fn jitter_stays_within_bounds() {
        let limits = limits();
        for restart_count in 0..8 {
            let base = limits.backoff(restart_count).as_secs_f64();
            for _ in 0..64 {
                let jittered = jittered_backoff(restart_count, &limits).as_secs_f64();
                assert!(
                    jittered >= base * 0.75 && jittered <= base * 1.25,
                    "jitter {jittered} out of bounds for base {base}"
                );
            }
        }
    }

    #[test]
    fn attempt_window_evicts_stale_entries() {
        let base = Instant::now();
        let limits = limits();
        let mut window = AttemptWindow::new();
        for _ in 0..5 {
            window.record(base, limits.window);
        }
        assert_eq!(window.len_at(base, limits.window), 5);
        // everything aged past the window is evicted
        let later = base + limits.window + Duration::from_secs(1);
        assert_eq!(window.len_at(later, limits.window), 0);
        // a partial slide evicts only what expired
        let mut window = AttemptWindow::new();
        window.record(base, limits.window);
        window.record(base + Duration::from_secs(500), limits.window);
        assert_eq!(
            window.len_at(base + Duration::from_secs(700), limits.window),
            1,
            "only the first attempt expired"
        );
    }
}
