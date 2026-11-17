//! Recursive supervision tree for the proxy, on top of the central task
//! service (`candyriya-scheduler`).
//!
//! The task service owns execution: named tasks, cancellation, containment
//! and drain. This crate owns the layer above it — the supervision tree —
//! and nothing else:
//!
//! - **pid allocation** — sequential `u64`s, the root supervisor (`init`)
//!   is pid 1, pids are never reused;
//! - **the node registry** — every unit records pid, ppid, name, kind,
//!   state, restart count and last exit status;
//! - **the zombie lifecycle** — a finished unit that nobody has scooped up
//!   yet is a `Zombie` (its exit status is kept); reaping harvests it and
//!   either removes it, moves it to `FailedTerminal` or schedules a restart
//!   per its policy;
//! - **orphan re-parenting** — when a unit exits, its children are
//!   re-parented to the root, which reaps them;
//! - **a bounded ring of recent exits** — bounded history for ps/metrics;
//! - **stuck-zombie accounting** — zombies older than a configurable
//!   threshold are counted and exposed via the `candyriya_zombies` /
//!   `candyriya_zombies_stuck` gauges.
//!
//! Everything that runs, runs through the task service; nothing in this
//! crate spawns tasks or threads ad hoc. Unit bodies are re-created from a
//! factory per restart, and a one-shot unit never re-runs.
//!
//! The crate is intentionally agnostic of *who* talks to whom: it knows
//! nothing about messages or capabilities. Higher layers build their
//! call planes next to this core.
//!
//! # A note on module shape
//!
//! Public types that are part of the spawn/reap API are re-exported at the
//! crate root; everything else is private. The public surface is kept
//! minimal on purpose — [`Init`] is the only stateful entry point most
//! callers need.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod error;
mod exit;
mod node;
mod pid;
mod policy;
mod ring;
mod runner;
mod spec;
mod tree;

pub use crate::error::InitError;
pub use crate::exit::{ExitRecord, ExitStatus, MAX_FAILURE_MESSAGE_BYTES};
pub use crate::node::{UnitInfo, UnitKind, UnitState};
pub use crate::pid::Pid;
pub use crate::policy::{RestartLimits, RestartPolicy};
pub use crate::spec::{UnitBody, UnitBodyFactory, UnitSpec};
pub use crate::tree::{Init, InitOptions, MAX_UNIT_NAME_BYTES, ReapOutcome, TreeSnapshot};
