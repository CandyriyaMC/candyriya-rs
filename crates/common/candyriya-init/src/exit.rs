//! Exit statuses and the bounded history of unit exits.
//!
//! The task service does not report a per-task outcome to its callers, so the
//! wrapper that runs a unit body has to classify the exit itself and hand it
//! to the tree. [`ExitStatus`] is the unit-facing vocabulary for that
//! classification; [`ExitRecord`] is what lands in the bounded ring once the
//! exit is observed.

use std::fmt;
use std::time::Instant;

use candyriya_utils::truncate_to_char_boundary;

use crate::UnitKind;
use crate::pid::Pid;

/// Maximum size of a [`ExitStatus::Failure`] message, in bytes.
///
/// The message travels into the exit ring, the process-ps listing and error
/// surfaces; a runaway body must not be able to balloon any of those.
/// Overlong messages are truncated at a char boundary.
pub const MAX_FAILURE_MESSAGE_BYTES: usize = 1024;

/// How a unit finished.
///
/// Mirrors the outcomes the central task service can produce for a task,
/// plus [`ExitStatus::Failure`] for bodies that report their own failure.
/// Cancellation is always a *requested* stop and is therefore never treated
/// as a failure by restart policies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExitStatus {
    /// The unit body ran to completion and reported success.
    Success,
    /// The unit body finished but reported a failure. Carries a bounded,
    /// human-readable message (see [`ExitStatus::failure`]).
    Failure(String),
    /// The unit body panicked. The panic is contained by the scheduler;
    /// the tree only learns that it happened.
    Panicked,
    /// The unit was cancelled through the tree (a requested stop).
    Cancelled,
    /// The unit exceeded its per-unit timeout (see [`crate::UnitSpec::with_timeout`]).
    TimedOut,
}

impl ExitStatus {
    /// Build a failure status with a bounded message.
    ///
    /// The message is truncated to [`MAX_FAILURE_MESSAGE_BYTES`] at a char
    /// boundary, so constructing one is always cheap and safe for the ring.
    #[must_use]
    pub fn failure(message: impl Into<String>) -> Self {
        let message = message.into();
        Self::Failure(truncate_to_char_boundary(&message, MAX_FAILURE_MESSAGE_BYTES).to_owned())
    }

    /// Whether the exit is a success.
    #[must_use]
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success)
    }

    /// Whether restart policies treat this exit as a failure.
    ///
    /// `Success` is not, `Cancelled` is not (it is a requested stop), the
    /// rest are.
    #[must_use]
    pub(crate) fn is_failure(&self) -> bool {
        matches!(self, Self::Failure(_) | Self::Panicked | Self::TimedOut)
    }
}

impl fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Success => f.write_str("success"),
            Self::Failure(message) => write!(f, "failure({message})"),
            Self::Panicked => f.write_str("panicked"),
            Self::Cancelled => f.write_str("cancelled"),
            Self::TimedOut => f.write_str("timed-out"),
        }
    }
}

/// One finished run of a unit, as recorded in the exit ring.
///
/// Records are created at the moment the tree observes the exit (a unit
/// becoming a zombie), not at reap time, so nothing is lost even if the
/// reaper is slow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExitRecord {
    /// The unit's pid. Kept across restarts, so a pid can appear more than
    /// once in the ring, once per exit.
    pub pid: Pid,
    /// The unit's parent at exit time. Orphans are re-parented to the root
    /// before the record is created, so a pid of [`Pid::INIT`] means the
    /// unit was adopted.
    pub ppid: Pid,
    /// The unit's name at exit time.
    pub name: String,
    /// The unit's kind.
    pub kind: UnitKind,
    /// How the unit exited.
    pub status: ExitStatus,
    /// How many restarts the unit had performed *before* this exit.
    pub restart_count: u32,
    /// When the run started.
    pub started_at: Instant,
    /// When the tree observed the exit.
    pub exited_at: Instant,
}
