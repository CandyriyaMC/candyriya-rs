//! A single backend server and its health state.

use std::net::SocketAddr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Instant;

/// Health state of a backend.
///
/// The state is advisory: it reflects the outcome of the last health check
/// and nothing else. The login flow does not hard-block offline backends on
/// its own — operators that want that behaviour gate it in their own logic —
/// but the state is kept up to date so the proxy and its metrics can see
/// what is happening.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum BackendState {
    /// The backend has never been checked (or its last check did not produce
    /// a verdict, e.g. it was marked manually).
    #[default]
    Unknown = 0,
    /// The last health check completed a full status ping.
    Online = 1,
    /// The last health check failed to connect, timed out, or got a reply
    /// that was not a valid status response.
    Offline = 2,
}

impl BackendState {
    /// Decodes a raw byte back into a state.
    ///
    /// Any unrecognized value maps to [`BackendState::Unknown`], so a health
    /// check reading a garbled byte never panics and never misroutes.
    pub const fn from_u8(raw: u8) -> Self {
        match raw {
            1 => Self::Online,
            2 => Self::Offline,
            _ => Self::Unknown,
        }
    }

    /// Encodes the state as a raw byte.
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

/// One backend server the proxy can route players to.
///
/// `state` is an [`AtomicU8`] so readers never take a lock, and
/// `last_check` is a short-held [`Mutex`] that only the health checker
/// touches (a mutex is fine here — checks run once every few seconds,
/// nowhere near the copy hot path). If the mutex is ever poisoned (we never
/// panic while holding it), the poison is recovered transparently instead of
/// panicking.
#[derive(Debug)]
pub struct Backend {
    name: String,
    addr: SocketAddr,
    state: AtomicU8,
    last_check: Mutex<Instant>,
}

impl Backend {
    /// Creates a backend in [`BackendState::Unknown`] state, with
    /// `last_check` set to now (nothing has been observed yet, but the clock
    /// starts when the backend is registered).
    pub fn new(name: String, addr: SocketAddr) -> Self {
        Self {
            name,
            addr,
            state: AtomicU8::new(BackendState::Unknown.as_u8()),
            last_check: Mutex::new(Instant::now()),
        }
    }

    /// The configured name of the backend (its key in `[servers]`).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The socket address the proxy dials for this backend.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The last observed health state.
    pub fn state(&self) -> BackendState {
        BackendState::from_u8(self.state.load(Ordering::Acquire))
    }

    /// Overwrites the health state without touching `last_check`.
    ///
    /// This is the escape hatch for operators and tests (e.g. marking a
    /// backend offline because it is draining). The health checker itself
    /// goes through a dedicated path so both pieces of bookkeeping stay
    /// consistent.
    pub fn set_state(&self, state: BackendState) {
        self.state.store(state.as_u8(), Ordering::Release);
    }

    /// The time of the last health check (or the moment the backend was
    /// registered if it was never checked).
    pub fn last_check(&self) -> Instant {
        self.with_last_check(|instant| *instant)
    }

    /// Stores the outcome of a health check: new state and a fresh
    /// `last_check` timestamp.
    pub(crate) fn mark_checked(&self, state: BackendState) {
        self.set_state(state);
        self.with_last_check(|instant| *instant = Instant::now());
    }

    fn with_last_check<R>(&self, f: impl FnOnce(&mut Instant) -> R) -> R {
        match self.last_check.lock() {
            Ok(mut guard) => f(&mut guard),
            Err(poisoned) => f(&mut poisoned.into_inner()),
        }
    }
}

impl Clone for Backend {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            addr: self.addr,
            state: AtomicU8::new(self.state().as_u8()),
            last_check: Mutex::new(self.last_check()),
        }
    }
}
