//! Unit identity inside the supervision tree.

use std::fmt;

/// Identifier of a unit in the supervision tree.
///
/// Pids are allocated sequentially from a process-wide counter in the order
/// units are registered; init, the root supervisor, is always pid 1 and pids
/// are never reused, not even across unit restarts (a restarting unit keeps
/// its pid, see [`crate::Init::spawn_unit`]). Cheap to copy and compare.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Pid(u64);

impl Pid {
    /// The root supervisor; the target of orphan re-parenting.
    pub const INIT: Self = Self(1);

    /// Sentinel for "no parent". Only the root carries it.
    pub const NONE: Self = Self(0);

    /// Creates a pid from a raw value.
    ///
    /// Only the tree itself allocates pids; this exists so tests and the
    /// registry can name already-allocated values without round-trips.
    pub(crate) fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// The raw numeric value of the pid.
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

impl fmt::Debug for Pid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Pid({})", self.0)
    }
}

impl fmt::Display for Pid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
