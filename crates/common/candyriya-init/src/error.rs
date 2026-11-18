//! Error types returned by the supervision tree.

use thiserror::Error;

use crate::node::UnitState;
use crate::pid::Pid;

/// Error returned when the tree cannot perform an operation.
#[derive(Debug, Error)]
pub enum InitError {
    /// No unit with this pid exists (or it was already reaped).
    #[error("no such unit: pid {0}")]
    NoSuchUnit(Pid),

    /// The unit exists but is not in the `Zombie` state a reap requires.
    /// Carries the state observed at error time.
    #[error("unit {pid} is not a zombie (current state: {state})")]
    NotZombie {
        /// The pid that was not a zombie.
        pid: Pid,
        /// The state the unit was actually in.
        state: UnitState,
    },

    /// The parent pid given at spawn time does not name a live unit.
    #[error("no such parent: pid {0}")]
    NoSuchParent(Pid),

    /// The parent exists but is not `Running`, so it cannot supervise a child.
    #[error("parent {0} is not running")]
    ParentNotRunning(Pid),

    /// The unit name violates the naming contract (empty, overlong, or
    /// contains control characters). The payload explains which.
    #[error("invalid unit name: {0}")]
    InvalidName(String),

    /// A zombie without an exit status was found. Pure internal-invariant
    /// guard; a well-formed tree never produces this.
    #[error("zombie {0} carries no exit status (internal invariant violated)")]
    MissingExitStatus(Pid),

    /// The spawner asked to install a grant to `target` that it is not
    /// allowed to pass down: the spawner is neither an ancestor of `target`
    /// nor itself holds a grant to it.
    #[error(
        "unit {parent} cannot grant a handle to {target}: not an ancestor and not a grant holder"
    )]
    GrantNotHeld {
        /// The unit that tried to pass the handle down.
        parent: Pid,
        /// The unit the handle points at.
        target: Pid,
    },
}
