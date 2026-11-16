//! Task identity and scheduling priority.

use std::fmt;

/// Unique identifier of a task registered with the service.
///
/// Ids are allocated from a process-wide counter in the order tasks are
/// registered. They are cheap to copy and compare.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TaskId(u64);

impl TaskId {
    pub(crate) fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw numeric value of the id.
    pub fn as_u64(&self) -> u64 {
        self.0
    }
}

impl fmt::Debug for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TaskId({})", self.0)
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "task#{}", self.0)
    }
}

/// Scheduling priority of a task.
///
/// Priorities are stored on every task and are reflected in
/// [`crate::TaskStats::by_priority`] for observability. They are **advisory
/// hints only**: the scheduler does not preempt running tasks and does not
/// reorder the executor. `Low` is reserved for plugin (scoped) work, which
/// core subsystems must never wait on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TaskPriority {
    /// Background work that can afford to be delayed. Plugin scopes force this.
    Low = 0,
    /// The default for core tasks.
    #[default]
    Normal = 1,
    /// Latency-sensitive work the operator cares about.
    High = 2,
}
