//! Token bucket rate limiting.
//!
//! Two layers, see the crate-level docs for the rationale:
//!
//! - [`BucketState`] — pure state machine, every method takes
//!   `now: std::time::Instant` explicitly. Used by tests with artificial
//!   times and by [`TokenBucket`] internally.
//! - [`TokenBucket`] — thread-safe wrapper (`parking_lot::Mutex`) calling
//!   the pure core with [`Instant::now`]. This is what the proxy uses.

use std::time::Instant;

use parking_lot::Mutex;

/// Pure token bucket state machine with explicit time injection.
///
/// The bucket starts full (burst available immediately). Refill is lazy:
/// tokens accumulate based on the elapsed time between calls, capped at
/// the configured capacity. `now` is expected to be non-decreasing; a
/// call with `now` earlier than the previous one adds nothing and does
/// not rewind the internal clock, so wall-clock jumps backwards are
/// harmless.
///
/// Invalid configuration is sanitized at construction instead of
/// panicking: negative or non-finite `capacity`/`refill_per_sec` become
/// `0.0` (a bucket that never refills and starts empty).
#[derive(Debug, Clone)]
pub struct BucketState {
    capacity: f64,
    refill_per_sec: f64,
    tokens: f64,
    last: Instant,
}

impl BucketState {
    /// Creates a full bucket with the given capacity and refill rate.
    ///
    /// `now` is the reference point for the first refill calculation;
    /// pass [`Instant::now`] in production and an artificial instant in
    /// tests.
    pub fn new(capacity: f64, refill_per_sec: f64, now: Instant) -> Self {
        let capacity = sanitize(capacity);
        Self {
            capacity,
            refill_per_sec: sanitize(refill_per_sec),
            // start full: the whole burst is available right away
            tokens: capacity,
            last: now,
        }
    }

    /// Acquires a single token.
    pub fn try_acquire(&mut self, now: Instant) -> bool {
        self.try_acquire_n(1.0, now)
    }

    /// Acquires `n` tokens.
    ///
    /// Returns `false` (without consuming anything) when `n` is negative,
    /// non-finite, or larger than the tokens currently available.
    /// `n <= 0.0` is trivially allowed.
    pub fn try_acquire_n(&mut self, n: f64, now: Instant) -> bool {
        if !n.is_finite() || n < 0.0 {
            return false;
        }
        self.refill(now);
        if self.tokens >= n {
            self.tokens -= n;
            true
        } else {
            false
        }
    }

    /// Tokens available right after refilling to `now`.
    pub fn available(&self, now: Instant) -> f64 {
        let mut clone = self.clone();
        clone.refill(now);
        clone.tokens
    }

    /// Full burst capacity the bucket was configured with.
    pub fn capacity(&self) -> f64 {
        self.capacity
    }

    /// Steady-state refill rate in tokens per second.
    pub fn refill_per_sec(&self) -> f64 {
        self.refill_per_sec
    }

    fn refill(&mut self, now: Instant) {
        // never move the clock backwards; a rewind would double-count refill
        if now <= self.last {
            return;
        }
        let elapsed = now.duration_since(self.last);
        self.last = now;
        let add = elapsed.as_secs_f64() * self.refill_per_sec;
        self.tokens = (self.tokens + add).min(self.capacity);
    }
}

fn sanitize(value: f64) -> f64 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        0.0
    }
}

/// Thread-safe token bucket reading [`Instant::now`] on every call.
///
/// This is the type to use in the proxy. For tests, use [`BucketState`]
/// directly — it accepts artificial instants and needs no sleeps.
///
/// Refilling happens lazily on every acquisition attempt; there is no
/// background task. A full bucket denies nothing and a drained bucket
/// refills at `refill_per_sec` tokens per second up to `capacity`.
#[derive(Debug)]
pub struct TokenBucket {
    state: Mutex<BucketState>,
}

impl TokenBucket {
    /// Creates a full bucket.
    ///
    /// Negative or non-finite arguments are clamped to `0.0` (see
    /// [`BucketState::new`]); zero capacity means nothing is ever granted.
    pub fn new(capacity: f64, refill_per_sec: f64) -> Self {
        Self {
            state: Mutex::new(BucketState::new(capacity, refill_per_sec, Instant::now())),
        }
    }

    /// Attempts to take one token.
    pub fn try_acquire(&self) -> bool {
        self.state.lock().try_acquire(Instant::now())
    }

    /// Attempts to take `n` tokens (see [`BucketState::try_acquire_n`]
    /// for the exact rules).
    pub fn try_acquire_n(&self, n: f64) -> bool {
        self.state.lock().try_acquire_n(n, Instant::now())
    }

    /// Tokens available right now.
    pub fn available(&self) -> f64 {
        self.state.lock().available(Instant::now())
    }

    /// Full burst capacity the bucket was configured with.
    pub fn capacity(&self) -> f64 {
        self.state.lock().capacity()
    }

    /// Steady-state refill rate in tokens per second.
    pub fn refill_per_sec(&self) -> f64 {
        self.state.lock().refill_per_sec()
    }
}

/// Convenience constructor for handshake rate limiting per IP.
///
/// Returns a bucket with `burst = max` and `refill_per_sec = max`, i.e.
/// up to `max` instantaneous requests and a sustained `max` per second
/// afterwards. Suitable for the proxy's `[advanced]` handshake limiter.
///
/// `max = 0` yields a bucket that grants nothing.
pub fn bursts_per_second(max: u64) -> TokenBucket {
    let rate = max as f64;
    TokenBucket::new(rate, rate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    #[test]
    fn bucket_starts_full_and_drains() {
        let t0 = Instant::now();
        let mut b = BucketState::new(10.0, 0.0, t0);
        for _ in 0..10 {
            assert!(b.try_acquire(t0));
        }
        assert!(!b.try_acquire(t0));
    }

    #[test]
    fn refill_caps_at_capacity() {
        let t0 = Instant::now();
        let mut b = BucketState::new(5.0, 100.0, t0);
        for _ in 0..5 {
            assert!(b.try_acquire(t0));
        }
        // 10 seconds at 100/s would be 1000 tokens, capped at 5
        let t1 = t0 + Duration::from_secs(10);
        assert!(b.try_acquire_n(5.0, t1));
        assert!(!b.try_acquire(t1));
    }

    #[test]
    fn refill_amounts_match_elapsed_time() {
        let t0 = Instant::now();
        let mut b = BucketState::new(100.0, 10.0, t0);
        assert!(b.try_acquire_n(100.0, t0));
        assert!(!b.try_acquire(t0));
        // 0.5s at 10/s = 5 tokens
        let t1 = t0 + Duration::from_millis(500);
        assert!(b.try_acquire_n(5.0, t1));
        assert!(!b.try_acquire(t1));
        // another 2s at 10/s = 20 tokens (float dust and all, 19 always fits)
        let t2 = t1 + Duration::from_secs(2);
        assert!(b.try_acquire_n(19.0, t2));
    }

    #[test]
    fn partial_request_is_not_consumed_on_failure() {
        let t0 = Instant::now();
        let mut b = BucketState::new(3.0, 0.0, t0);
        assert!(!b.try_acquire_n(4.0, t0));
        assert_eq!(b.available(t0), 3.0);
    }

    #[test]
    fn denied_request_carries_no_side_effects() {
        let t0 = Instant::now();
        let mut b = BucketState::new(2.0, 0.0, t0);
        assert!(!b.try_acquire_n(f64::NAN, t0));
        assert!(!b.try_acquire_n(-1.0, t0));
        assert!(!b.try_acquire_n(f64::INFINITY, t0));
        assert_eq!(b.available(t0), 2.0);
    }

    #[test]
    fn zero_n_is_trivially_allowed() {
        let t0 = Instant::now();
        let mut b = BucketState::new(0.0, 0.0, t0);
        assert!(b.try_acquire_n(0.0, t0));
    }

    #[test]
    fn negative_n_is_rejected() {
        let t0 = Instant::now();
        let mut b = BucketState::new(5.0, 0.0, t0);
        assert!(!b.try_acquire_n(-1.0, t0));
        // untouched
        assert_eq!(b.available(t0), 5.0);
    }

    #[test]
    fn negative_and_nan_config_clamps_to_zero() {
        let t0 = Instant::now();
        let b = BucketState::new(-1.0, f64::NAN, t0);
        assert_eq!(b.capacity(), 0.0);
        assert_eq!(b.refill_per_sec(), 0.0);
        let mut b = b;
        assert!(!b.try_acquire(t0));
        let b = BucketState::new(f64::INFINITY, f64::INFINITY, t0);
        assert_eq!(b.capacity(), 0.0);
        assert_eq!(b.refill_per_sec(), 0.0);
    }

    #[test]
    fn backwards_time_does_not_rewind_or_mint_tokens() {
        let t0 = Instant::now();
        let mut b = BucketState::new(10.0, 1000.0, t0);
        assert!(b.try_acquire_n(10.0, t0));
        // jump far ahead: 1s at 1000/s = 1000 tokens, capped at 10
        let t1 = t0 + Duration::from_secs(1);
        assert!(b.try_acquire_n(10.0, t1));
        // a call with an earlier instant adds nothing and does not rewind
        let t_back = t0 + Duration::from_millis(1);
        assert!(!b.try_acquire(t_back));
        // and the next forward call only sees real elapsed time since t1
        assert!(b.try_acquire_n(10.0, t1 + Duration::from_secs(1)));
    }

    #[test]
    fn wrapper_works_with_system_clock() {
        let b = TokenBucket::new(3.0, 0.0);
        assert!(b.try_acquire());
        assert!(b.try_acquire());
        assert!(b.try_acquire());
        assert!(!b.try_acquire());
        assert!(!b.try_acquire_n(1.0));
        assert_eq!(b.capacity(), 3.0);
    }

    #[test]
    fn bursts_per_second_builds_burst_equals_refill() {
        let b = bursts_per_second(25);
        assert_eq!(b.capacity(), 25.0);
        assert_eq!(b.refill_per_sec(), 25.0);
    }

    #[test]
    fn concurrent_buckets_never_over_issue() {
        // 10 buckets, 100 threads, no refill: total grants must equal total capacity
        let buckets: Vec<Arc<TokenBucket>> = (0..10)
            .map(|_| Arc::new(TokenBucket::new(50.0, 0.0)))
            .collect();
        let granted: Vec<AtomicU64> = (0..10).map(|_| AtomicU64::new(0)).collect();
        std::thread::scope(|s| {
            for _ in 0..100 {
                let buckets = &buckets;
                let granted = &granted;
                s.spawn(move || {
                    for (i, bucket) in buckets.iter().enumerate() {
                        for _ in 0..30 {
                            if bucket.try_acquire() {
                                granted[i].fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                });
            }
        });
        for (i, counter) in granted.iter().enumerate() {
            assert_eq!(
                counter.load(Ordering::Relaxed),
                50,
                "bucket {i} over-issued or under-issued"
            );
        }
    }
}
