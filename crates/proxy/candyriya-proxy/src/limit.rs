//! Connection-limit wiring for the accept loop.
//!
//! Two caps guard the proxy from drowning in inbound connections:
//!
//! 1. **Per-IP concurrent cap** — enforced by
//!    [`candyriya_ratelimit::ConnectionLimiter`]. The limiter's own global
//!    cap is set to "unbounded" on purpose: the global cap is owned by the
//!    semaphore below, so each layer has exactly one job and neither can
//!    mask the other's rejection. The limiter is configured with
//!    [`ConnectionLimiter::with_per_ip_max`], which keeps the global and
//!    per-IP counters under a single lock — one atomic decision, never a
//!    half-granted slot.
//! 2. **Global concurrent cap** — enforced by a [`tokio::sync::Semaphore`]
//!    with `max_connections` permits, matching the `[advanced]
//!    max-connections` config key. `ConnectionGuard` cannot model a
//!    semaphore, and the config documents the global cap as a semaphore,
//!    so the two mechanisms are kept: one guard from the limiter, one
//!    permit from the semaphore, both tied to the connection lifetime.
//!
//! Acquisition order is limiter first, semaphore second. If the
//! semaphore is exhausted, the limiter guard is dropped on the spot, so
//! the per-IP slot is released rather than leaked.

use std::net::IpAddr;
use std::sync::Arc;

use thiserror::Error;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use candyriya_ratelimit::{AtCapacity, ConnectionGuard, ConnectionLimiter};

/// Per-IP concurrent-connection cap used when the configuration does not
/// say otherwise.
///
/// There is no `[advanced]` key for a per-IP cap yet (the `[advanced]`
/// schema is owned by the config crate and has not grown one), so the
/// accept loop falls back to this value. 64 concurrent connections from
/// a single IPv4/IPv6 address is already generous for a
/// player-facing proxy: a real client opens 1.
pub const DEFAULT_PER_IP_MAX: u64 = 64;

/// One connection's worth of limit slots.
///
/// Holding a `ConnectionSlot` keeps both the per-IP slot (an RAII
/// [`ConnectionGuard`]) and the global permit (an
/// [`OwnedSemaphorePermit`]) alive. Drop it — by ending the connection
/// task — and both slots are released. The struct is not `Clone`:
/// one connection, one slot, never two.
#[derive(Debug)]
pub struct ConnectionSlot {
    guard: ConnectionGuard,
    permit: OwnedSemaphorePermit,
}

impl ConnectionSlot {
    /// The per-IP guard held for this connection.
    pub fn connection_guard(&self) -> &ConnectionGuard {
        &self.guard
    }

    /// The global semaphore permit held for this connection.
    pub fn permit(&self) -> &OwnedSemaphorePermit {
        &self.permit
    }
}

/// Why acquiring a connection slot failed.
#[derive(Debug, Error)]
pub enum SlotAcquireError {
    /// The per-IP (or the limiter's own accounting) cap rejected the
    /// connection. Carries the limiter's reason.
    #[error(transparent)]
    Limiter(#[from] AtCapacity),

    /// The global semaphore had no permits left.
    #[error("global connection limit reached")]
    Semaphore,
}

/// Try to acquire both slots for one inbound connection from `ip`.
///
/// On success the caller owns the slot until it drops it. On failure no
/// slot remains held: a semaphore failure drops the limiter guard before
/// returning, so the per-IP accounting never drifts.
pub fn try_acquire_slot(
    limiter: &ConnectionLimiter,
    semaphore: &Arc<Semaphore>,
    ip: IpAddr,
) -> Result<ConnectionSlot, SlotAcquireError> {
    let guard = limiter.try_new_connection_ip(ip)?;
    match semaphore.clone().try_acquire_owned() {
        Ok(permit) => Ok(ConnectionSlot { guard, permit }),
        Err(_) => {
            // the guard drops here, releasing the per-IP slot again
            Err(SlotAcquireError::Semaphore)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    fn ip4(last_octet: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, last_octet))
    }

    fn ip6(last_byte: u8) -> IpAddr {
        IpAddr::V6(Ipv6Addr::new(
            0x2001,
            0xdb8,
            0,
            0,
            0,
            0,
            0,
            u16::from(last_byte),
        ))
    }

    #[test]
    fn per_ip_cap_rejects_second_connection_when_global_is_higher() {
        // global cap huge on the limiter's accounting (semaphore owns the
        // global side), per-IP cap = 1: the second same-IP attempt fails
        // on the per-IP cap while another IP is unaffected.
        let limiter = ConnectionLimiter::with_per_ip_max(u64::MAX, 1);
        let semaphore = Arc::new(Semaphore::new(1024));

        let slot = try_acquire_slot(&limiter, &semaphore, ip4(1)).expect("first slot");
        let ip = ip4(1);
        let err = try_acquire_slot(&limiter, &semaphore, ip).expect_err("second slot must fail");
        assert!(matches!(err, SlotAcquireError::Limiter(AtCapacity::PerIp)));

        // a different ip is fine
        let _other = try_acquire_slot(&limiter, &semaphore, ip4(2)).expect("other ip slot");

        drop(slot);
        let slot2 = try_acquire_slot(&limiter, &semaphore, ip4(1)).expect("slot after release");
        drop(slot2);
    }

    #[test]
    fn semaphore_exhaustion_releases_the_limiter_slot() {
        let limiter = ConnectionLimiter::with_per_ip_max(u64::MAX, 10);
        let semaphore = Arc::new(Semaphore::new(1));

        let slot = try_acquire_slot(&limiter, &semaphore, ip4(1)).expect("first slot");
        // the semaphore is now exhausted; the limiter must not be left
        // holding a phantom per-IP slot for ip 2
        let err = try_acquire_slot(&limiter, &semaphore, ip4(2)).expect_err("semaphore exhausted");
        assert!(matches!(err, SlotAcquireError::Semaphore));
        assert_eq!(
            limiter.per_ip_connections(ip4(2)),
            0,
            "limiter slot must be released when the semaphore rejects"
        );

        drop(slot);
        let slot2 = try_acquire_slot(&limiter, &semaphore, ip4(2)).expect("slot after release");
        drop(slot2);
    }

    #[test]
    fn ipv6_peers_are_capped_like_ipv4() {
        let limiter = ConnectionLimiter::with_per_ip_max(u64::MAX, 1);
        let semaphore = Arc::new(Semaphore::new(64));
        let ip = ip6(1);
        let _slot = try_acquire_slot(&limiter, &semaphore, ip).expect("first slot");
        let err = try_acquire_slot(&limiter, &semaphore, ip).expect_err("second slot must fail");
        assert!(matches!(err, SlotAcquireError::Limiter(AtCapacity::PerIp)));
    }
}
