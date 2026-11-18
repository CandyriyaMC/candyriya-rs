//! The 11-state wire machine vocabulary.
//!
//! The wire machine is the lifecycle a native plugin moves through, as
//! observed by the controller. Three rules shape this module:
//!
//! - the state set is **closed**: exactly the eleven variants below, no
//!   `FAILED` state (failure is `reason` on the record plus the
//!   `UNLOADING -> UNLOADED` teardown path), no extension by other crates;
//! - transitions are checked against [`ALLOWED_TRANSITIONS`], a static
//!   allow-list — anything not listed there is rejected, never guessed;
//! - the machine itself (dep gates, deadlines, stale-generation rejection)
//!   lands in a later phase; this module only carries the vocabulary and
//!   the legal-move table.

use std::fmt;

use serde::{Deserialize, Serialize};

/// The lifecycle state of a native plugin, as observed by the controller.
///
/// The set is exactly the 11 states below — nothing may be added here by
/// hand. `DISCOVERED` is where a freshly scanned record starts; `UNLOADED`
/// is terminal (the record is kept for ps/metrics, the unit is gone).
/// A plugin that fails anywhere lands on the `UNLOADING -> UNLOADED`
/// teardown path with `reason` set on the record, never on a `Failed`
/// state — there is none.
///
/// Serde maps variants to their screaming snake wire spelling
/// (`"DISCOVERED"`, `"RUNNING"`, ...), so dumps stay greppable and stable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PluginWireState {
    /// The manifest was scanned and accepted-or-noted, but no unit, thread
    /// or mapping exists yet.
    Discovered,
    /// The load handshake is in flight (dlopen + symbol resolution + ABI
    /// handshake happen here, on the plugin thread).
    Loading,
    /// The library is mapped and handshaken; no plugin code has run yet.
    Loaded,
    /// The `on_load` callback is in flight.
    Initializing,
    /// `on_load` returned; the context is fully usable and the plugin is
    /// eligible for enabling (deps first).
    Initialized,
    /// The `on_enable` callback is in flight.
    Enabling,
    /// The plugin is live and receiving work.
    Running,
    /// The `on_disable` callback is in flight; the plugin is pausing and
    /// draining. The library stays mapped.
    Disabling,
    /// `on_disable` acknowledged; the plugin is parked but still mapped,
    /// still resident, eligible for re-enable or unload.
    Disabled,
    /// The `on_unload` callback ran (or the teardown decided to detach);
    /// the mailbox is closing and the join is bounded.
    Unloading,
    /// Terminal. The unit is reaped, the context dropped, the record kept
    /// with counters and `reason` for ps/tree.
    Unloaded,
}

/// All 11 wire states, in lifecycle order.
///
/// Pinned so a test can assert the set never grows or shrinks silently;
/// the epic mandates exactly eleven states and no `Failed` pseudo-state.
pub const ALL_STATES: [PluginWireState; 11] = [
    PluginWireState::Discovered,
    PluginWireState::Loading,
    PluginWireState::Loaded,
    PluginWireState::Initializing,
    PluginWireState::Initialized,
    PluginWireState::Enabling,
    PluginWireState::Running,
    PluginWireState::Disabling,
    PluginWireState::Disabled,
    PluginWireState::Unloading,
    PluginWireState::Unloaded,
];

/// Allow-list of legal wire-state transitions, as `(from, to)` pairs.
///
/// This table is the *only* gate the registry applies before moving a
/// record. Anything not listed here is rejected, including plausible-looking
/// jumps like `DISCOVERED -> RUNNING` or leaving the terminal `UNLOADED`.
/// The real machine (next phase) layers its own semantics on top of this
/// table; this table is an upper bound of what the machine may ever emit,
/// never a relaxation of it. Entries are documented per pair:
///
/// | from | to | notes |
/// |------|----|-------|
/// | `DISCOVERED` | `LOADING` | discovery done; the load handshake begins |
/// | `DISCOVERED` | `UNLOADING` | teardown before any unit existed (issues at discovery) |
/// | `LOADING` | `LOADED` | dlopen + ABI handshake acknowledged |
/// | `LOADING` | `UNLOADING` | dlopen/symbol/handshake/ctor failure; skip `ENABLING` |
/// | `LOADED` | `INITIALIZING` | marker passed; `on_load` is next |
/// | `LOADED` | `UNLOADING` | crash between load ack and init call |
/// | `INITIALIZING` | `INITIALIZED` | `on_load` acknowledged |
/// | `INITIALIZING` | `UNLOADING` | init ack err/timeout/crash; skip `ENABLING` |
/// | `INITIALIZED` | `ENABLING` | enable begins (dep gates enforced by the machine) |
/// | `INITIALIZED` | `UNLOADING` | crash while armed; no callback ever ran |
/// | `ENABLING` | `RUNNING` | `on_enable` acknowledged |
/// | `ENABLING` | `UNLOADING` | enable ack err/timeout/crash; `on_disable` never runs |
/// | `RUNNING` | `DISABLING` | operator or graceful stop begins |
/// | `RUNNING` | `UNLOADING` | runtime panic / thread dead; `on_disable` skipped |
/// | `DISABLING` | `DISABLED` | `on_disable` acknowledged |
/// | `DISABLING` | `RUNNING` | disable failed but the plugin still runs (latch; machine decides) |
/// | `DISABLING` | `UNLOADING` | operator-forced teardown after a disable failure; never automatic |
/// | `DISABLED` | `ENABLING` | re-enable without reload |
/// | `DISABLED` | `UNLOADING` | eligible for teardown |
/// | `UNLOADING` | `UNLOADED` | teardown done; terminal; also the `mark_failed` landing |
///
/// `UNLOADED` is terminal — it has no outgoing rows by design. Teardown
/// from any live state funnels into `UNLOADING` exactly once, which is
/// what keeps the teardown path single-exit.
pub const ALLOWED_TRANSITIONS: &[(PluginWireState, PluginWireState)] = &[
    (PluginWireState::Discovered, PluginWireState::Loading),
    (PluginWireState::Discovered, PluginWireState::Unloading),
    (PluginWireState::Loading, PluginWireState::Loaded),
    (PluginWireState::Loading, PluginWireState::Unloading),
    (PluginWireState::Loaded, PluginWireState::Initializing),
    (PluginWireState::Loaded, PluginWireState::Unloading),
    (PluginWireState::Initializing, PluginWireState::Initialized),
    (PluginWireState::Initializing, PluginWireState::Unloading),
    (PluginWireState::Initialized, PluginWireState::Enabling),
    (PluginWireState::Initialized, PluginWireState::Unloading),
    (PluginWireState::Enabling, PluginWireState::Running),
    (PluginWireState::Enabling, PluginWireState::Unloading),
    (PluginWireState::Running, PluginWireState::Disabling),
    (PluginWireState::Running, PluginWireState::Unloading),
    (PluginWireState::Disabling, PluginWireState::Disabled),
    (PluginWireState::Disabling, PluginWireState::Running),
    (PluginWireState::Disabling, PluginWireState::Unloading),
    (PluginWireState::Disabled, PluginWireState::Enabling),
    (PluginWireState::Disabled, PluginWireState::Unloading),
    (PluginWireState::Unloading, PluginWireState::Unloaded),
];

/// A wire state that knows which moves the allow-list permits from it.
///
/// This is the "transitionable discipline" the registry applies: every
/// state change goes through this check, never through a hand-rolled
/// match in some caller.
pub trait Transitionable {
    /// Whether `target` may be entered from `self` under
    /// [`ALLOWED_TRANSITIONS`].
    fn can_transition_to(self, target: PluginWireState) -> bool;
}

impl Transitionable for PluginWireState {
    fn can_transition_to(self, target: PluginWireState) -> bool {
        ALLOWED_TRANSITIONS.contains(&(self, target))
    }
}

impl PluginWireState {
    /// Whether this state is terminal (`UNLOADED`).
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Unloaded)
    }

    /// The screaming-snake wire spelling, stable for logs and metrics.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Discovered => "DISCOVERED",
            Self::Loading => "LOADING",
            Self::Loaded => "LOADED",
            Self::Initializing => "INITIALIZING",
            Self::Initialized => "INITIALIZED",
            Self::Enabling => "ENABLING",
            Self::Running => "RUNNING",
            Self::Disabling => "DISABLING",
            Self::Disabled => "DISABLED",
            Self::Unloading => "UNLOADING",
            Self::Unloaded => "UNLOADED",
        }
    }
}

impl fmt::Display for PluginWireState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_states_pins_exactly_eleven() {
        assert_eq!(ALL_STATES.len(), 11);
        for (i, state) in ALL_STATES.iter().enumerate() {
            for other in &ALL_STATES[i + 1..] {
                assert_ne!(state, other, "duplicate state in ALL_STATES");
            }
        }
    }

    #[test]
    fn unloaded_is_terminal_and_has_no_outgoing_rows() {
        assert!(PluginWireState::Unloaded.is_terminal());
        for target in ALL_STATES {
            assert!(
                !PluginWireState::Unloaded.can_transition_to(target),
                "UNLOADED must not transition to {target}"
            );
        }
    }

    #[test]
    fn every_live_state_can_reach_unloaded() {
        // teardown is the single-exit hatch: any live state funnels into
        // UNLOADING, and UNLOADING lands on UNLOADED
        for state in ALL_STATES {
            if state.is_terminal() || state == PluginWireState::Unloading {
                continue;
            }
            assert!(
                state.can_transition_to(PluginWireState::Unloading),
                "{state} must be able to enter UNLOADING (teardown hatch)"
            );
        }
        assert!(PluginWireState::Unloading.can_transition_to(PluginWireState::Unloaded));
    }

    #[test]
    fn table_rejects_impossible_jumps() {
        assert!(!PluginWireState::Discovered.can_transition_to(PluginWireState::Running));
        assert!(!PluginWireState::Running.can_transition_to(PluginWireState::Loading));
        assert!(!PluginWireState::Enabling.can_transition_to(PluginWireState::Disabled));
        assert!(!PluginWireState::Disabled.can_transition_to(PluginWireState::Unloaded));
        assert!(!PluginWireState::Discovered.can_transition_to(PluginWireState::Discovered));
    }

    #[test]
    fn serde_round_trip_keeps_wire_spelling() {
        for state in ALL_STATES {
            let json = serde_json::to_string(&state).expect("serialize");
            assert_eq!(json, format!("\"{state}\""));
            let back: PluginWireState = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, state);
        }
    }
}
