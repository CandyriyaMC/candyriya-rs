//! Concurrent connection limiting with an RAII guard.
//!
//! [`ConnectionLimiter`] caps how many connections may be alive at once.
//! On a global limit it returns an [`AtCapacity`] error instead of
//! handing out a guard; dropping the [`ConnectionGuard`] releases the
//! slot. Guards are the *only* way to hold a slot — they cannot be
//! cloned, so a slot can never be released twice.
//!
//! # Global and per-IP caps live in one struct under one lock
//!
//! A connection slot is either granted in full or not at all: the global
//! counter and the per-IP counter are checked and incremented atomically
//! inside a single `parking_lot::Mutex`. Splitting them into two locks
//! would invite lock-ordering bugs and half-granted slots. The critical
//! section is a few integer compares and (when enabled) one hash map
//! lookup — short enough that contention is a non-issue here.
//!
//! Per-IP accounting is enabled only when the limiter is constructed via
//! [`ConnectionLimiter::with_per_ip_max`]; otherwise no per-IP map is kept
//! (no memory spent on data nobody enforces) and connections opened
//! through [`ConnectionLimiter::try_new_connection`] count against the
//! global cap only.

use std::collections::HashMap;
use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;

use parking_lot::Mutex;

/// Which cap rejected a new connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AtCapacity {
    /// The global concurrent connection cap was reached.
    #[error("global connection limit reached")]
    Global,
    /// The per-IP concurrent connection cap for this IP was reached.
    #[error("per-IP connection limit reached")]
    PerIp,
}

#[derive(Debug)]
struct ConnState {
    global_used: u64,
    /// Only maintained when `per_ip_max` is set.
    per_ip_used: HashMap<IpAddr, u64>,
    global_max: u64,
    per_ip_max: Option<u64>,
}

impl ConnState {
    fn release(&mut self, ip: Option<IpAddr>) {
        self.global_used = self.global_used.saturating_sub(1);
        if self.per_ip_max.is_some()
            && let Some(ip) = ip
            && let Some(entry) = self.per_ip_used.get_mut(&ip)
        {
            *entry = entry.saturating_sub(1);
            if *entry == 0 {
                self.per_ip_used.remove(&ip);
            }
        }
    }
}

/// Concurrent connection limiter (global cap, optional per-IP cap).
///
/// Cheap to clone — clones share the same counters, so hand the clone to
/// the accept loop and keep the original elsewhere.
#[derive(Debug, Clone)]
pub struct ConnectionLimiter {
    state: Arc<Mutex<ConnState>>,
}

impl ConnectionLimiter {
    /// Creates a limiter with only a global cap of `global_max` concurrent
    /// connections. Per-IP accounting is disabled.
    ///
    /// `global_max = 0` denies everything — by design.
    pub fn new(global_max: u64) -> Self {
        Self {
            state: Arc::new(Mutex::new(ConnState {
                global_used: 0,
                per_ip_used: HashMap::new(),
                global_max,
                per_ip_max: None,
            })),
        }
    }

    /// Creates a limiter with a global cap of `global_max` and a per-IP
    /// cap of `per_ip_max`, enforced for connections opened via
    /// [`ConnectionLimiter::try_new_connection_ip`].
    pub fn with_per_ip_max(global_max: u64, per_ip_max: u64) -> Self {
        Self {
            state: Arc::new(Mutex::new(ConnState {
                global_used: 0,
                per_ip_used: HashMap::new(),
                global_max,
                per_ip_max: Some(per_ip_max),
            })),
        }
    }

    /// Attempts to open a connection counting against the global cap only.
    ///
    /// On success returns a guard that releases its slot on drop.
    pub fn try_new_connection(&self) -> Result<ConnectionGuard, AtCapacity> {
        self.acquire(None)
    }

    /// Attempts to open a connection for `ip`, counting against both the
    /// global cap and (when configured via [`ConnectionLimiter::with_per_ip_max`])
    /// the per-IP cap.
    ///
    /// If the limiter was built with [`ConnectionLimiter::new`], only the
    /// global cap applies and no per-IP bookkeeping is kept.
    pub fn try_new_connection_ip(&self, ip: IpAddr) -> Result<ConnectionGuard, AtCapacity> {
        self.acquire(Some(ip))
    }

    fn acquire(&self, ip: Option<IpAddr>) -> Result<ConnectionGuard, AtCapacity> {
        let mut state = self.state.lock();
        if state.global_used >= state.global_max {
            return Err(AtCapacity::Global);
        }
        if let (Some(limit), Some(ip)) = (state.per_ip_max, ip) {
            let used = state.per_ip_used.get(&ip).copied().unwrap_or(0);
            if used >= limit {
                return Err(AtCapacity::PerIp);
            }
            state.per_ip_used.insert(ip, used + 1);
        }
        state.global_used += 1;
        drop(state);
        Ok(ConnectionGuard {
            state: Arc::clone(&self.state),
            ip,
        })
    }

    /// Connections currently open (global view).
    pub fn current_connections(&self) -> u64 {
        self.state.lock().global_used
    }

    /// Connections currently open from `ip`.
    ///
    /// Returns `0` when per-IP accounting is disabled.
    pub fn per_ip_connections(&self, ip: IpAddr) -> u64 {
        let state = self.state.lock();
        if state.per_ip_max.is_none() {
            return 0;
        }
        state.per_ip_used.get(&ip).copied().unwrap_or(0)
    }

    /// The configured global cap.
    pub fn global_max(&self) -> u64 {
        self.state.lock().global_max
    }

    /// The configured per-IP cap, or `None` when disabled.
    pub fn per_ip_max(&self) -> Option<u64> {
        self.state.lock().per_ip_max
    }
}

/// RAII handle for one acquired connection slot.
///
/// Not `Clone` on purpose: one guard, one slot. The slot is released when
/// the guard is dropped, whether that happens via normal drop, end of
/// scope, or stack unwinding.
pub struct ConnectionGuard {
    state: Arc<Mutex<ConnState>>,
    ip: Option<IpAddr>,
}

impl fmt::Debug for ConnectionGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectionGuard")
            .field("ip", &self.ip)
            .finish_non_exhaustive()
    }
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.state.lock().release(self.ip);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn ip4(last_octet: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, last_octet))
    }

    #[test]
    fn global_cap_is_enforced_and_released() {
        let limiter = ConnectionLimiter::new(2);
        let g1 = limiter.try_new_connection().unwrap();
        let g2 = limiter.try_new_connection().unwrap();
        assert_eq!(limiter.current_connections(), 2);
        assert_eq!(
            limiter.try_new_connection().unwrap_err(),
            AtCapacity::Global
        );
        drop(g1);
        assert_eq!(limiter.current_connections(), 1);
        let _g3 = limiter.try_new_connection().unwrap();
        drop(g2);
        assert_eq!(limiter.current_connections(), 1); // only _g3 held
    }

    #[test]
    fn dropping_all_guards_returns_to_zero() {
        let limiter = ConnectionLimiter::new(3);
        {
            let _guards: Vec<_> = (0..3)
                .map(|_| limiter.try_new_connection().unwrap())
                .collect();
            assert_eq!(limiter.current_connections(), 3);
        }
        assert_eq!(limiter.current_connections(), 0);
        assert!(limiter.try_new_connection().is_ok());
    }

    #[test]
    fn zero_global_max_denies_everything() {
        let limiter = ConnectionLimiter::new(0);
        assert_eq!(
            limiter.try_new_connection().unwrap_err(),
            AtCapacity::Global
        );
    }

    #[test]
    fn per_ip_cap_is_independent_of_global() {
        let limiter = ConnectionLimiter::with_per_ip_max(10, 1);
        let _g1 = limiter.try_new_connection_ip(ip4(1)).unwrap();
        // same IP again: per-IP deny, global untouched
        assert_eq!(
            limiter.try_new_connection_ip(ip4(1)).unwrap_err(),
            AtCapacity::PerIp
        );
        assert_eq!(limiter.current_connections(), 1);
        assert_eq!(limiter.per_ip_connections(ip4(1)), 1);
        // other IP fine
        let _g2 = limiter.try_new_connection_ip(ip4(2)).unwrap();
        assert_eq!(limiter.current_connections(), 2);
    }

    #[test]
    fn per_ip_slot_releases_on_drop() {
        let limiter = ConnectionLimiter::with_per_ip_max(10, 1);
        {
            let _g = limiter.try_new_connection_ip(ip4(1)).unwrap();
            assert_eq!(
                limiter.try_new_connection_ip(ip4(1)).unwrap_err(),
                AtCapacity::PerIp
            );
        }
        assert_eq!(limiter.per_ip_connections(ip4(1)), 0);
        assert!(limiter.try_new_connection_ip(ip4(1)).is_ok());
    }

    #[test]
    fn per_ip_map_is_cleaned_up_when_entry_hits_zero() {
        let limiter = ConnectionLimiter::with_per_ip_max(10, 2);
        let g = limiter.try_new_connection_ip(ip4(1)).unwrap();
        drop(g);
        // no stale entry lingers in the map
        assert_eq!(limiter.per_ip_connections(ip4(1)), 0);
        assert!(limiter.state.lock().per_ip_used.is_empty());
    }

    #[test]
    fn global_cap_applies_before_per_ip() {
        // global fills first: per-IP check must not mask the global error
        let limiter = ConnectionLimiter::with_per_ip_max(1, 5);
        let _g1 = limiter.try_new_connection_ip(ip4(1)).unwrap();
        assert_eq!(
            limiter.try_new_connection_ip(ip4(2)).unwrap_err(),
            AtCapacity::Global
        );
    }

    #[test]
    fn ipv6_ips_are_tracked() {
        let limiter = ConnectionLimiter::with_per_ip_max(10, 1);
        let ip = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1));
        let _g1 = limiter.try_new_connection_ip(ip).unwrap();
        assert_eq!(
            limiter.try_new_connection_ip(ip).unwrap_err(),
            AtCapacity::PerIp
        );
    }

    #[test]
    fn per_ip_disabled_by_default() {
        let limiter = ConnectionLimiter::new(10);
        let ip = ip4(1);
        let _g = limiter.try_new_connection_ip(ip).unwrap();
        // no accounting kept, unlimited per-IP
        let _g2 = limiter.try_new_connection_ip(ip).unwrap();
        assert_eq!(limiter.per_ip_connections(ip), 0);
        assert_eq!(limiter.per_ip_max(), None);
        assert_eq!(limiter.global_max(), 10);
    }

    #[test]
    fn cloned_limiter_shares_counters() {
        let limiter = ConnectionLimiter::new(1);
        let clone = limiter.clone();
        let _g = clone.try_new_connection().unwrap();
        assert_eq!(limiter.current_connections(), 1);
        assert_eq!(
            limiter.try_new_connection().unwrap_err(),
            AtCapacity::Global
        );
    }

    #[test]
    fn at_capacity_displays_reasonable_messages() {
        assert_eq!(
            AtCapacity::Global.to_string(),
            "global connection limit reached"
        );
        assert_eq!(
            AtCapacity::PerIp.to_string(),
            "per-IP connection limit reached"
        );
    }
}
