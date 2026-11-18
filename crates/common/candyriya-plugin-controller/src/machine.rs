//! The state machine, as a design target with a stub to grow into.
//!
//! **This module is a placeholder.** The real machine — dependency gates,
//! deadlines as transition data, stale-generation rejection, the
//! `disable_failed` latch, the `UNLOADING` teardown contract — belongs to
//! the next phase and is intentionally absent here. What exists today:
//!
//! - [`PluginEvent`]: the lifecycle event vocabulary, one variant per
//!   wire state, named like the state it carries the machine towards;
//! - [`apply`]: the always-`Ok` stub with the exact signature the real
//!   implementation will keep.
//!
//! `apply` currently does **not** consult the allow-list: it is the
//! un-gated escape hatch the stub needs to exist at all, and nothing in
//! this crate routes through it yet. The registry's
//! [`crate::PluginRegistry::set_state`] remains the only checked writer
//! until the machine replaces the stub.

use crate::error::TransitionError;
use crate::record::PluginRecord;
use crate::state::PluginWireState;

/// A lifecycle event, one variant per wire state.
///
/// Names mirror [`PluginWireState`] exactly. The payloadless shape is
/// deliberate: the real machine will decide what each event carries
/// (acks, receipts, deadlines) and that decision is explicitly the next
/// phase's job, not something to freeze prematurely.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PluginEvent {
    /// Discovery completed; the record exists and nothing else does.
    Discovered,
    /// The load handshake begins.
    Loading,
    /// The load handshake acknowledged.
    Loaded,
    /// The init callback begins.
    Initializing,
    /// The init callback acknowledged.
    Initialized,
    /// The enable callback begins.
    Enabling,
    /// The enable callback acknowledged; the plugin is live.
    Running,
    /// The disable callback begins.
    Disabling,
    /// The disable callback acknowledged; the plugin is parked.
    Disabled,
    /// The teardown begins (or ran); the deadline machinery starts here.
    Unloading,
    /// The teardown finished; terminal.
    Unloaded,
}

impl PluginEvent {
    /// The wire state this event carries the machine towards.
    pub const fn wire_state(self) -> PluginWireState {
        match self {
            Self::Discovered => PluginWireState::Discovered,
            Self::Loading => PluginWireState::Loading,
            Self::Loaded => PluginWireState::Loaded,
            Self::Initializing => PluginWireState::Initializing,
            Self::Initialized => PluginWireState::Initialized,
            Self::Enabling => PluginWireState::Enabling,
            Self::Running => PluginWireState::Running,
            Self::Disabling => PluginWireState::Disabling,
            Self::Disabled => PluginWireState::Disabled,
            Self::Unloading => PluginWireState::Unloading,
            Self::Unloaded => PluginWireState::Unloaded,
        }
    }
}

impl From<PluginEvent> for PluginWireState {
    fn from(event: PluginEvent) -> Self {
        event.wire_state()
    }
}

/// Applies a lifecycle event to a record and returns the new state.
///
/// **Stub — the signature is the contract, the body is not.** Always
/// `Ok`: writes the event's state onto the record and returns it. The
/// allow-list, the dep gates, the deadlines and the generation checks are
/// the next phase's machine; this stub exists so the integration can be
/// typed against `apply` without the real logic landing first.
pub fn apply(
    record: &mut PluginRecord,
    event: PluginEvent,
) -> Result<PluginWireState, TransitionError> {
    let new_state = event.wire_state();
    record.set_state(new_state);
    Ok(new_state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::PluginManifest;
    use std::path::PathBuf;

    fn demo_record() -> PluginRecord {
        PluginRecord::from_manifest(
            &PluginManifest::new("greeting", "1.0.0", "Greeting Demo", None),
            PathBuf::from("plugins/greeting/greeting.dll"),
        )
    }

    #[test]
    fn apply_always_succeeds_and_lands_the_event_state() {
        for event in [
            PluginEvent::Discovered,
            PluginEvent::Loading,
            PluginEvent::Loaded,
            PluginEvent::Initializing,
            PluginEvent::Initialized,
            PluginEvent::Enabling,
            PluginEvent::Running,
            PluginEvent::Disabling,
            PluginEvent::Disabled,
            PluginEvent::Unloading,
            PluginEvent::Unloaded,
        ] {
            let mut record = demo_record();
            let new_state = apply(&mut record, event).expect("stub apply is always Ok");
            assert_eq!(new_state, event.wire_state());
            assert_eq!(record.state(), event.wire_state());
        }
    }

    #[test]
    fn event_and_state_stay_in_lockstep() {
        // the two enums have the same number of variants and the same names;
        // this test pins that so a variant never gets renamed on one side only
        let events = [
            PluginEvent::Discovered,
            PluginEvent::Loading,
            PluginEvent::Loaded,
            PluginEvent::Initializing,
            PluginEvent::Initialized,
            PluginEvent::Enabling,
            PluginEvent::Running,
            PluginEvent::Disabling,
            PluginEvent::Disabled,
            PluginEvent::Unloading,
            PluginEvent::Unloaded,
        ];
        for event in events {
            let via_from: PluginWireState = event.into();
            assert_eq!(via_from, event.wire_state());
        }
    }
}
