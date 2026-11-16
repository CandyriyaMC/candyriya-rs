//! Point-in-time statistics and the shutdown report.

use std::collections::HashMap;
use std::time::Duration;

use crate::TaskPriority;

/// Point-in-time snapshot of scheduler activity.
///
/// Returned by [`crate::TaskService::stats`]. The `active`-family fields are
/// instantaneous; all other counters accumulate since the service was built
/// and never decrease.
#[derive(Debug, Clone)]
pub struct TaskStats {
    /// Tasks currently registered with the service.
    ///
    /// Includes tasks whose cancellation was requested but whose body has not
    /// finished yet (cancellation is cooperative).
    pub active: u32,
    /// Tasks that ran to completion, in total.
    pub completed: u64,
    /// Tasks that were cancelled, in total (via [`crate::TaskService::cancel`],
    /// scope closure, or [`crate::TaskService::drain`]).
    pub cancelled: u64,
    /// Tasks that panicked, in total. A panicking task is contained: its
    /// counter is bumped and the rest of the scheduler is unaffected.
    pub panicked: u64,
    /// Tasks that exceeded their time limit, in total.
    ///
    /// Covers scoped tasks that ran into [`crate::ScopeLimits::task_timeout`]
    /// and tasks that were still running when [`crate::TaskService::drain`]
    /// hit its deadline (drain accounts for those as timed out).
    pub timed_out: u64,
    /// Currently active tasks per task name. Sums to [`Self::active`].
    pub by_name: HashMap<String, u32>,
    /// Currently active tasks per priority. Sums to [`Self::active`].
    pub by_priority: HashMap<TaskPriority, u32>,
    /// Average wall-clock duration of all finished tasks, or `None` if no
    /// task has finished yet.
    pub avg_duration: Option<Duration>,
}

/// Outcome of [`crate::TaskService::drain`].
///
/// `completed` and `cancelled` are deltas over the drain call itself.
/// `timed_out` counts tasks still active when the deadline hit; their cancel
/// tokens were flipped (and their bookkeeping released) by drain, so their
/// final moments are not double-counted in the counters above.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShutdownReport {
    /// Tasks that completed normally during the drain.
    pub completed: u64,
    /// Tasks cancelled during the drain.
    pub cancelled: u64,
    /// Tasks still active when the deadline hit.
    ///
    /// For async work, the cancel token is observed at the next await point;
    /// destructors still run. For blocking work, the underlying closure runs
    /// to completion in the background. Entries and scoped permits are
    /// released immediately by drain, so statistics stay consistent.
    pub timed_out: u64,
}
