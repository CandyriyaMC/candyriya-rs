//! How a unit body's fate is observed and reported into the tree.
//!
//! The central task service does not expose per-task outcomes to its
//! callers, so the future that runs a unit body has to classify the exit
//! itself and hand it to the registry. This module builds that future.
//!
//! The classification rules, one code path for all of them:
//!
//! - the body returns normally → whatever it returned (defaulting to
//!   `Success` for well-behaved bodies);
//! - the per-unit timeout fires → `TimedOut`, and the body future is
//!   dropped at the point where it is pending (destructors run);
//! - the body panics → the task-service wrapper's poll unwinds, and during
//!   unwinding `thread::panicking()` reports `true`, so the guard records
//!   `Panicked`;
//! - the task service drops the wrapper at an await point (a requested
//!   cancellation) → normal drop, `thread::panicking()` reports `false`,
//!   so the guard records `Cancelled`.
//!
//! The guard does the recording exactly once, at drop time; there is no
//! window where an exit goes unnoticed.

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

use candyriya_scheduler::{TaskId, TaskService};

use crate::exit::ExitStatus;
use crate::ipc::CURRENT_UNIT;
use crate::park::{PARK, ParkGate};
use crate::pid::Pid;
use crate::spec::UnitBody;
use crate::stats::{CONN, ConnStats};
use crate::tree::TreeState;

/// Everything the wrapper needs to scope one body run, bundled so the
/// spawn call site stays readable: the run's exit-classification clone
/// slot is built inside, and `pid`/`name` arrive alongside.
pub(crate) struct BodyWire {
    /// The gate bound to this run (fresh receiver subscribed at spawn / respawn).
    pub park: ParkGate,
    /// The connection counters, present only for connection-kind units.
    pub conn: Option<ConnStats>,
    /// The per-run timeout, if the unit was built with one.
    pub timeout: Option<Duration>,
}

/// Spawn one run of a unit body through the task service and return the id
/// it assigned.
///
/// The body arrives already built — the factory is invoked synchronously at
/// registration time (see `build_run` in `tree.rs`), never inside the spawned
/// task. This function wraps the body in the exit-classification future and
/// arms the `CURRENT_UNIT` task-local for the duration of the run, so
/// [`crate::ipc::current_unit`] works inside unit bodies. The [`crate::ParkGate`]
/// and (for connection units) the [`crate::ConnStats`] are bound alongside
/// it: they belong to the run, like the caller pid does. Bodies created
/// inside this scope inherit nothing beyond their own code; helpers they
/// spawn belong to them.
///
/// The wrapper future carries an [`ExitGuard`]; when the future completes
/// (or unwinds, or is dropped) the guard records the outcome into the tree.
/// Machinery, not a unit: the wrapper itself never registers in the tree.
pub(crate) fn spawn_wrapper(
    service: &TaskService,
    tree: Arc<Mutex<TreeState>>,
    pid: Pid,
    name: &str,
    body: UnitBody,
    wire: BodyWire,
) -> TaskId {
    let slot = Arc::new(Mutex::new(None::<ExitStatus>));
    let guard = ExitGuard {
        pid,
        tree,
        slot: slot.clone(),
    };
    let task_name = name.to_string();
    let BodyWire {
        park,
        conn,
        timeout,
    } = wire;
    let future = PARK.scope(
        park,
        CONN.scope(
            conn,
            CURRENT_UNIT.scope(pid, async move {
                let guard = guard;
                let status = match timeout {
                    Some(limit) => match tokio::time::timeout(limit, body).await {
                        Ok(status) => status,
                        Err(_) => ExitStatus::TimedOut,
                    },
                    None => body.await,
                };
                // normal path: the guard takes over from here and records
                *guard.slot.lock() = Some(status);
            }),
        ),
    );
    service.spawn_named(task_name, future)
}

/// Records the unit's exit into the tree when the wrapper future is gone.
///
/// The only disarm signal it understands is a filled slot; anything else
/// (a panic unwind, a cancellation drop) is classified from
/// `thread::panicking()`.
struct ExitGuard {
    pid: Pid,
    tree: Arc<Mutex<TreeState>>,
    slot: Arc<Mutex<Option<ExitStatus>>>,
}

impl Drop for ExitGuard {
    fn drop(&mut self) {
        let slot_status = self.slot.lock().take();
        let status = slot_status.unwrap_or_else(|| {
            if std::thread::panicking() {
                ExitStatus::Panicked
            } else {
                ExitStatus::Cancelled
            }
        });
        // the tree lock could be held by a reaper on another thread; the
        // record is a handful of map/counter writes, never an await point
        self.tree.lock().record_exit(self.pid, status);
    }
}
