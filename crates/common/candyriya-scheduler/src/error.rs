//! Error types returned by the scheduler API.

use thiserror::Error;

/// Error returned when a [`crate::TaskService`] cannot be built.
#[derive(Debug, Error)]
pub enum BuildError {
    /// No runtime handle was provided and none is active on the current thread.
    ///
    /// Pass a handle explicitly with [`crate::TaskServiceBuilder::handle`] or
    /// build the service from inside the runtime that should own the tasks.
    #[error(
        "no tokio runtime handle: pass TaskServiceBuilder::handle(..) or build inside a runtime"
    )]
    NoRuntime,
    /// The fixed blocking thread pool could not be created.
    #[error("failed to build the blocking thread pool: {0}")]
    BlockingPool(String),
}

/// Error returned when a cron schedule cannot be registered.
#[derive(Debug, Error)]
pub enum ScheduleError {
    /// The expression could not be parsed.
    ///
    /// Schedules use the six-field format `sec min hour day-of-month month
    /// day-of-week`, evaluated in UTC. Seconds are required.
    #[error("invalid cron expression: {0}")]
    InvalidCron(String),
    /// The underlying cron scheduler rejected the job or could not be created.
    #[error("cron scheduler error: {0}")]
    Scheduler(String),
    /// The service is already draining and no longer accepts new cron jobs.
    #[error("scheduler is draining; new cron jobs are rejected")]
    Draining,
}

/// Error returned when a scoped (plugin) task is submitted.
#[derive(Debug, Error)]
pub enum ScopeError {
    /// The bounded wait queue is full while `max_concurrent` tasks are running.
    ///
    /// The submission is rejected (an error, never a silent drop or an unbounded
    /// wait), which is what plugin code should key off to decide whether to
    /// shed work.
    #[error("scope quota exceeded: {running}/{running_max} running, {queued}/{queued_max} queued")]
    QuotaExceeded {
        /// Tasks currently running in the scope.
        running: u32,
        /// Configured concurrent-task limit.
        running_max: u32,
        /// Tasks currently waiting in the scope's bounded queue.
        queued: u32,
        /// Configured queue-length limit.
        queued_max: u32,
    },
    /// The scope was closed with [`crate::ScopedTaskService::close`] and no
    /// longer accepts work.
    #[error("scope `{0}` is closed")]
    Closed(String),
    /// The whole service is draining and no longer accepts new work.
    #[error("scheduler is draining; scope `{0}` cannot start new tasks")]
    Draining(String),
}
