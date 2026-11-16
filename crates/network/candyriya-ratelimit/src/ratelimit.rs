//! Fixed-window rate limiting, one counter per key.
//!
//! Keys are typically IP addresses as strings (`"203.0.113.7"`,
//! `"2001:db8::1"`); the limiter itself is agnostic to key content.
//!
//! Window semantics: each key gets its own window anchored at its first
//! request, not aligned to wall-clock boundaries. Within a window at most
//! `limit` requests are allowed; once the window elapses, the next request
//! starts a fresh window. Denied requests do **not** consume quota —
//! under a flood the counter would otherwise never recover.
//!
//! The map of tracked keys grows with the number of distinct keys seen.
//! It is not self-bounding (a bound would either drop legitimate state
//! or silently fail open), so callers that accept untrusted keys should
//! invoke [`RateLimiter::purge_expired`] periodically — e.g. from a
//! housekeeping task. Keys whose window has fully elapsed are dead
//! weight: purging them changes nothing semantically.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Outcome of a rate limit check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Allow {
    /// The request may proceed; one unit of quota was consumed.
    Allowed,
    /// The request is denied. `retry_after` is how long the caller should
    /// wait until the current window resets.
    Denied {
        /// Time until the current window for this key elapses.
        retry_after: Duration,
    },
}

#[derive(Debug, Clone, Copy)]
struct WindowCounter {
    window_start: Instant,
    count: u64,
}

/// Pure fixed-window state machine with explicit time injection.
///
/// Every method takes `now: std::time::Instant`; tests drive it with
/// artificial instants and need no sleeps. A non-monotonic `now` (earlier
/// than a previous call) is handled with saturating arithmetic and can
/// neither mint quota nor extend a window.
///
/// `window` of zero makes every check start a fresh window (i.e. limiting
/// is effectively disabled). `limit` of zero denies everything — by
/// design, an operator-configured deny-all.
#[derive(Debug, Clone)]
pub struct RateLimiterState {
    window: Duration,
    limit: u64,
    windows: HashMap<String, WindowCounter>,
}

impl RateLimiterState {
    /// Creates an empty state with the given window and per-key limit.
    pub fn new(window: Duration, limit: u64) -> Self {
        Self {
            window,
            limit,
            windows: HashMap::new(),
        }
    }

    /// The configured window length.
    pub fn window(&self) -> Duration {
        self.window
    }

    /// The per-key request limit per window.
    pub fn limit(&self) -> u64 {
        self.limit
    }

    /// Number of keys currently tracked (including expired-but-not-purged
    /// ones; see [`RateLimiterState::purge_expired`]).
    pub fn len(&self) -> usize {
        self.windows.len()
    }

    /// Returns `true` when no keys are tracked.
    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }

    /// Checks (and consumes quota for) `key` at `now`.
    pub fn check(&mut self, key: &str, now: Instant) -> Allow {
        let window = self.window;
        match self.windows.get_mut(key) {
            Some(counter) => {
                let elapsed = now.saturating_duration_since(counter.window_start);
                if elapsed >= window {
                    counter.window_start = now;
                    counter.count = 1;
                } else if counter.count < self.limit {
                    counter.count += 1;
                } else {
                    return Allow::Denied {
                        retry_after: window.saturating_sub(elapsed),
                    };
                }
            }
            None => {
                self.windows.insert(
                    key.to_owned(),
                    WindowCounter {
                        window_start: now,
                        count: 1,
                    },
                );
            }
        }
        // a fresh window still counts 1 hit; limit 0 means deny-all
        if self.limit == 0 {
            return Allow::Denied {
                retry_after: window,
            };
        }
        Allow::Allowed
    }

    /// Checks a batch of keys in order, returning one [`Allow`] per key in
    /// the same order.
    ///
    /// Duplicate keys in the slice consume quota one by one, exactly as if
    /// [`RateLimiterState::check`] had been called repeatedly.
    pub fn check_many(&mut self, keys: &[String], now: Instant) -> Vec<Allow> {
        keys.iter().map(|key| self.check(key, now)).collect()
    }

    /// Drops all windows that fully elapsed before `now`.
    ///
    /// Returns the number of entries removed. Semantically a no-op for
    /// live traffic; use it to bound memory on long-running acceptors.
    pub fn purge_expired(&mut self, now: Instant) -> usize {
        let window = self.window;
        let before = self.windows.len();
        self.windows
            .retain(|_, counter| now.saturating_duration_since(counter.window_start) < window);
        before - self.windows.len()
    }

    /// Drops all tracked state (limits unchanged).
    pub fn clear(&mut self) {
        self.windows.clear();
    }
}

/// Fixed-window limiter reading [`Instant::now`] on every call.
///
/// Unlike [`crate::TokenBucket`] this type is **not** internally
/// synchronized: `check` takes `&mut self`. Share it across threads by
/// wrapping in `Arc<Mutex<_>>` (parking_lot), or keep it inside a
/// single-thread-per-role task. The proxy is expected to do the former
/// in the accept path.
///
/// For tests, use [`RateLimiterState`] directly with artificial instants.
#[derive(Debug, Clone)]
pub struct RateLimiter {
    state: RateLimiterState,
}

impl RateLimiter {
    /// Creates an empty limiter with the given window and per-key limit.
    pub fn new(window: Duration, limit: u64) -> Self {
        Self {
            state: RateLimiterState::new(window, limit),
        }
    }

    /// The configured window length.
    pub fn window(&self) -> Duration {
        self.state.window()
    }

    /// The per-key request limit per window.
    pub fn limit(&self) -> u64 {
        self.state.limit()
    }

    /// Number of keys currently tracked.
    pub fn len(&self) -> usize {
        self.state.len()
    }

    /// Returns `true` when no keys are tracked.
    pub fn is_empty(&self) -> bool {
        self.state.is_empty()
    }

    /// Checks (and consumes quota for) `key`.
    pub fn check(&mut self, key: &str) -> Allow {
        self.state.check(key, Instant::now())
    }

    /// Checks a batch of keys in order; see [`RateLimiterState::check_many`].
    pub fn check_many(&mut self, keys: &[String]) -> Vec<Allow> {
        self.state.check_many(keys, Instant::now())
    }

    /// Purges fully-elapsed windows; see [`RateLimiterState::purge_expired`].
    pub fn purge_expired(&mut self) -> usize {
        self.state.purge_expired(Instant::now())
    }

    /// Drops all tracked state (limits unchanged).
    pub fn clear(&mut self) {
        self.state.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "203.0.113.7";

    #[test]
    fn allows_up_to_limit_then_denies() {
        let t0 = Instant::now();
        let mut rl = RateLimiterState::new(Duration::from_secs(60), 3);
        assert_eq!(rl.check(KEY, t0), Allow::Allowed);
        assert_eq!(rl.check(KEY, t0), Allow::Allowed);
        assert_eq!(rl.check(KEY, t0), Allow::Allowed);
        match rl.check(KEY, t0) {
            Allow::Denied { retry_after } => {
                assert_eq!(retry_after, Duration::from_secs(60))
            }
            Allow::Allowed => panic!("expected deny after limit"),
        }
    }

    #[test]
    fn retry_after_shrinks_as_window_ages() {
        let t0 = Instant::now();
        let mut rl = RateLimiterState::new(Duration::from_secs(60), 1);
        assert_eq!(rl.check(KEY, t0), Allow::Allowed);
        let t1 = t0 + Duration::from_secs(45);
        match rl.check(KEY, t1) {
            Allow::Denied { retry_after } => {
                assert_eq!(retry_after, Duration::from_secs(15))
            }
            Allow::Allowed => panic!("expected deny"),
        }
    }

    #[test]
    fn window_reset_allows_again() {
        let t0 = Instant::now();
        let mut rl = RateLimiterState::new(Duration::from_secs(60), 2);
        assert_eq!(rl.check(KEY, t0), Allow::Allowed);
        assert_eq!(rl.check(KEY, t0), Allow::Allowed);
        assert!(matches!(rl.check(KEY, t0), Allow::Denied { .. }));
        // window elapses: fresh window, quota restored
        let t1 = t0 + Duration::from_secs(60);
        assert_eq!(rl.check(KEY, t1), Allow::Allowed);
        assert_eq!(rl.check(KEY, t1), Allow::Allowed);
        assert!(matches!(rl.check(KEY, t1), Allow::Denied { .. }));
    }

    #[test]
    fn denied_requests_do_not_consume_quota() {
        let t0 = Instant::now();
        let mut rl = RateLimiterState::new(Duration::from_secs(60), 1);
        assert_eq!(rl.check(KEY, t0), Allow::Allowed);
        // two denied hits must not extend anything or drain anything
        let t1 = t0 + Duration::from_secs(10);
        assert!(matches!(rl.check(KEY, t1), Allow::Denied { .. }));
        assert!(matches!(rl.check(KEY, t1), Allow::Denied { .. }));
        let t2 = t0 + Duration::from_secs(60);
        assert_eq!(rl.check(KEY, t2), Allow::Allowed);
    }

    #[test]
    fn keys_are_independent() {
        let t0 = Instant::now();
        let mut rl = RateLimiterState::new(Duration::from_secs(60), 1);
        assert_eq!(rl.check("10.0.0.1", t0), Allow::Allowed);
        assert_eq!(rl.check("10.0.0.2", t0), Allow::Allowed);
        assert!(matches!(rl.check("10.0.0.1", t0), Allow::Denied { .. }));
        assert_eq!(rl.check("2001:db8::dead:beef", t0), Allow::Allowed);
    }

    #[test]
    fn check_many_returns_per_key_results_in_order() {
        let t0 = Instant::now();
        let mut rl = RateLimiterState::new(Duration::from_secs(60), 1);
        let keys = vec![
            "10.0.0.1".to_string(),
            "10.0.0.2".to_string(),
            "10.0.0.1".to_string(), // second hit on same key: denied
        ];
        let out = rl.check_many(&keys, t0);
        assert_eq!(
            out,
            vec![
                Allow::Allowed,
                Allow::Allowed,
                Allow::Denied {
                    retry_after: Duration::from_secs(60)
                }
            ]
        );
    }

    #[test]
    fn purge_expired_drops_dead_windows() {
        let t0 = Instant::now();
        let mut rl = RateLimiterState::new(Duration::from_secs(60), 1);
        rl.check("10.0.0.1", t0);
        rl.check("10.0.0.2", t0);
        assert_eq!(rl.len(), 2);
        // not yet expired
        assert_eq!(rl.purge_expired(t0 + Duration::from_secs(59)), 0);
        assert_eq!(rl.len(), 2);
        // expired, purged
        assert_eq!(rl.purge_expired(t0 + Duration::from_secs(60)), 2);
        assert_eq!(rl.len(), 0);
        assert!(rl.is_empty());
    }

    #[test]
    fn zero_window_disables_limiting() {
        let t0 = Instant::now();
        let mut rl = RateLimiterState::new(Duration::ZERO, 1);
        assert_eq!(rl.check(KEY, t0), Allow::Allowed);
        assert_eq!(rl.check(KEY, t0), Allow::Allowed);
    }

    #[test]
    fn zero_limit_denies_everything() {
        let t0 = Instant::now();
        let mut rl = RateLimiterState::new(Duration::from_secs(60), 0);
        match rl.check(KEY, t0) {
            Allow::Denied { retry_after } => assert_eq!(retry_after, Duration::from_secs(60)),
            Allow::Allowed => panic!("zero-limit limiter must deny"),
        }
    }

    #[test]
    fn earlier_now_extends_nothing() {
        let t0 = Instant::now();
        let mut rl = RateLimiterState::new(Duration::from_secs(60), 1);
        assert_eq!(rl.check(KEY, t0), Allow::Allowed);
        // earlier instant: treated as zero elapsed, denies, same retry_after
        let earlier = t0; // artificial "backwards" is t0 itself here
        match rl.check(KEY, earlier + Duration::ZERO) {
            Allow::Denied { retry_after } => assert_eq!(retry_after, Duration::from_secs(60)),
            Allow::Allowed => panic!("expected deny"),
        }
    }

    #[test]
    fn wrapper_check_uses_system_clock() {
        let mut rl = RateLimiter::new(Duration::from_secs(60), 2);
        assert_eq!(rl.check("10.0.0.9"), Allow::Allowed);
        assert_eq!(rl.check("10.0.0.9"), Allow::Allowed);
        assert!(matches!(rl.check("10.0.0.9"), Allow::Denied { .. }));
        assert_eq!(rl.len(), 1);
        assert_eq!(rl.window(), Duration::from_secs(60));
        assert_eq!(rl.limit(), 2);
        rl.clear();
        assert!(rl.is_empty());
    }
}
