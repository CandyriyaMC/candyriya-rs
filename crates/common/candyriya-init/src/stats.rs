//! Per-connection byte accounting: the honest ps columns for live sessions.
//!
//! A unit of kind [`UnitKind::Connection`](crate::UnitKind::Connection) gets
//! counters bound at spawn time, shared between the tree (which keeps them
//! for [`crate::Init::connection_info`]) and the running body (which bumps
//! them through the task-local [`current_conn_stats`] handed out by the
//! runner). Nothing else pays for them: non-connection units get no counters
//! and the hot path is two relaxed atomic increments per pumped chunk.
//!
//! The accounting is deliberately asymmetric in what it claims: bytes are
//! exact, CPU is not measured at all, and `Instant`s come from one process.
//! `last_activity` is stored as millis since `connected_at` behind an atomic
//! so readers never wait on the hot path; ps renders it back into an
//! [`Instant`] and nobody has to take the registry lock to print a table.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::node::UnitState;
use crate::pid::Pid;

/// A point-in-time snapshot of one connection unit, for ps rendering.
///
/// Like [`crate::UnitInfo`] this owns nothing. The counters keep counting
/// until the unit exits; a connection unit stays visible (as a zombie, for
/// a brief reap window) with its final counters so ps never shows a torn
/// pair of numbers. Field order is the render order, roughly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectionInfo {
    /// The connection unit's pid.
    pub pid: Pid,
    /// The unit's name (`conn-<n>`).
    pub name: String,
    /// The unit's state at snapshot time.
    pub state: UnitState,
    /// Whether the unit is parked by [`crate::Init::freeze`].
    pub parked: bool,
    /// When the connection was accepted (the unit's start, not the tree's).
    pub connected_at: Instant,
    /// Bytes pumped client → backend in the play state so far.
    pub bytes_rx: u64,
    /// Bytes pumped backend → client in the play state so far.
    pub bytes_tx: u64,
    /// When either direction last moved a byte (never before `connected_at`).
    pub last_activity: Instant,
}

/// Shared per-connection counters, handed to the body via the task-local
/// [`current_conn_stats`] and to the tree through [`Init::connection_info`](crate::Init::connection_info).
///
/// This is the write path for a session; ps should read
/// [`ConnectionInfo`](crate::ConnectionInfo) instead. Cheap to clone.
#[derive(Clone)]
pub struct ConnStats {
    inner: Arc<ConnStatsInner>,
}

struct ConnStatsInner {
    bytes_rx: AtomicU64,
    bytes_tx: AtomicU64,
    connected_at: Instant,
    last_activity_millis: AtomicU64,
}

impl ConnStats {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(ConnStatsInner {
                bytes_rx: AtomicU64::new(0),
                bytes_tx: AtomicU64::new(0),
                connected_at: Instant::now(),
                last_activity_millis: AtomicU64::new(0),
            }),
        }
    }

    /// Record `n` bytes received from the client (pumped toward the backend).
    pub fn add_bytes_rx(&self, n: u64) {
        self.inner.bytes_rx.fetch_add(n, Ordering::Relaxed);
        self.touch();
    }

    /// Record `n` bytes received from the backend (pumped toward the client).
    pub fn add_bytes_tx(&self, n: u64) {
        self.inner.bytes_tx.fetch_add(n, Ordering::Relaxed);
        self.touch();
    }

    fn touch(&self) {
        let millis = self.inner.connected_at.elapsed().as_millis();
        self.inner
            .last_activity_millis
            .store(u64::try_from(millis).unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    pub(crate) fn snapshot(&self) -> (u64, u64, Instant, Instant) {
        let bytes_rx = self.inner.bytes_rx.load(Ordering::Relaxed);
        let bytes_tx = self.inner.bytes_tx.load(Ordering::Relaxed);
        let connected_at = self.inner.connected_at;
        let last_activity = connected_at
            + Duration::from_millis(self.inner.last_activity_millis.load(Ordering::Relaxed));
        (bytes_rx, bytes_tx, connected_at, last_activity)
    }
}

impl std::fmt::Debug for ConnStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (bytes_rx, bytes_tx, connected_at, last_activity) = self.snapshot();
        f.debug_struct("ConnStats")
            .field("bytes_rx", &bytes_rx)
            .field("bytes_tx", &bytes_tx)
            .field("connected_at", &connected_at)
            .field("last_activity", &last_activity)
            .finish()
    }
}

tokio::task_local! {
    /// The counters bound to the unit run currently executing (if any).
    pub(crate) static CONN: Option<ConnStats>;
}

/// The connection counters bound to the unit whose body is executing.
///
/// `None` outside connection units (non-connection units, foreign tasks,
/// tests). Bump them with [`ConnStats::add_bytes_rx`] /
/// [`ConnStats::add_bytes_tx`]; nothing else is expected of a body.
#[must_use]
pub fn current_conn_stats() -> Option<ConnStats> {
    CONN.try_with(Clone::clone).ok().flatten()
}
