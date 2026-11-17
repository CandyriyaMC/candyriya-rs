//! What a unit is, and what state it is in.

use std::fmt;
use std::time::Instant;

use crate::exit::ExitStatus;
use crate::pid::Pid;

/// The role a unit plays in the tree.
///
/// The kind is bookkeeping: it drives process-ps rendering and tells a
/// reader what shape of work a unit carries. It does not change how the
/// wrapper supervises the body.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UnitKind {
    /// Runs once and exits (e.g. a one-shot bootstrap step).
    Oneshot,
    /// A long-lived service expected to run until stopped.
    LongLived,
    /// A periodic job re-created on an interval.
    Every,
    /// One network connection.
    Connection,
    /// A plugin (host) instance.
    Plugin,
}

impl fmt::Display for UnitKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Oneshot => f.write_str("oneshot"),
            Self::LongLived => f.write_str("long-lived"),
            Self::Every => f.write_str("every"),
            Self::Connection => f.write_str("connection"),
            Self::Plugin => f.write_str("plugin"),
        }
    }
}

/// Bounded vocabulary of service kinds, used as the `target` label on IPC
/// metrics.
///
/// The label must never carry a unit name, a pid or any per-instance value,
/// so the vocabulary is deliberately closed: adding a variant is an explicit,
/// reviewable change, which keeps the label cardinality bounded by
/// construction. Units that declare no service kind are reported as
/// [`ServiceKind::Unit`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ServiceKind {
    /// The config holder.
    Config,
    /// The cache (cachegirl) front.
    Cache,
    /// Authentication / session verification.
    Auth,
    /// The routing plane (backend selection, forced hosts).
    Router,
    /// The plugin set.
    Plugins,
    /// The TCP accept loop.
    Accept,
    /// The console REPL.
    Repl,
    /// The stats collector.
    Stats,
    /// The player roster.
    Roster,
    /// The operator console surface.
    Console,
    /// Fallback for units that declare no service kind.
    Unit,
}

impl ServiceKind {
    /// The bounded `target` label value for this kind.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::Cache => "cache",
            Self::Auth => "auth",
            Self::Router => "router",
            Self::Plugins => "plugins",
            Self::Accept => "accept",
            Self::Repl => "repl",
            Self::Stats => "stats",
            Self::Roster => "roster",
            Self::Console => "console",
            Self::Unit => "unit",
        }
    }
}

impl fmt::Display for ServiceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Lifecycle state of a unit.
///
/// The happy path is `Running` → `Zombie` → removed at reap. A unit with a
/// restart policy passes through `Restarting` between a zombie reap and the
/// backoff expiring, and lands in `FailedTerminal` when its start limit is
/// breached.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UnitState {
    /// The body is running.
    Running,
    /// The body finished and the exit status is kept on the node, but the
    /// parent has not reaped it yet. A stuck zombie is a bug signal.
    Zombie,
    /// The body finished, the reaper scheduled a restart and the unit is
    /// waiting out its backoff before the body is re-created.
    Restarting,
    /// The restart start limit was breached; the unit stays visible in the
    /// registry forever with its last exit status. Terminal.
    FailedTerminal,
}

impl fmt::Display for UnitState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Running => f.write_str("running"),
            Self::Zombie => f.write_str("zombie"),
            Self::Restarting => f.write_str("restarting"),
            Self::FailedTerminal => f.write_str("failed-terminal"),
        }
    }
}

/// A point-in-time snapshot of a unit, for ps/tree rendering and tests.
///
/// Unlike the internal registry entry this owns nothing: it is a cloneable
/// read model and it carries no behaviour.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnitInfo {
    /// The unit's pid.
    pub pid: Pid,
    /// The pid of the unit's parent at snapshot time.
    pub ppid: Pid,
    /// The unit's name.
    pub name: String,
    /// The unit's kind.
    pub kind: UnitKind,
    /// The unit's current state.
    pub state: UnitState,
    /// When this run of the unit started.
    pub started_at: Instant,
    /// How many times this unit has been restarted so far.
    pub restart_count: u32,
    /// The status of the last exit, kept for zombies, failed-terminal units
    /// and units that were restarted since they last exited.
    pub last_exit: Option<ExitStatus>,
    /// When the current run exited, if it has. Used to compute zombie age.
    pub exited_at: Option<Instant>,
}
