//! Error vocabulary shared by the registry and the machine.
//!
//! Kept in one place so both halves of the crate agree on the failure
//! words; every variant names the plugin id when there is one, so ps and
//! logs never have to guess which record failed.

use crate::state::PluginWireState;

/// A wire-state transition that the allow-list refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TransitionError {
    /// The `(from, to)` pair is not listed in [`crate::ALLOWED_TRANSITIONS`].
    #[error("illegal wire-state transition {from} -> {to}")]
    IllegalTransition {
        /// The state the record was in.
        from: PluginWireState,
        /// The state the caller asked for.
        to: PluginWireState,
    },
}

/// Everything the registry can refuse to do.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    /// An `insert` for an id that is already registered.
    #[error("plugin `{id}` is already registered")]
    DuplicatePlugin {
        /// The colliding plugin id.
        id: String,
    },
    /// The id is not in the registry.
    #[error("plugin `{id}` is not registered")]
    UnknownPlugin {
        /// The id nobody registered.
        id: String,
    },
    /// The record is not in the state a strict operation expects.
    #[error("plugin `{id}` is in state {found}, expected {expected}")]
    UnexpectedState {
        /// The plugin the operation targeted.
        id: String,
        /// The state the operation required.
        expected: PluginWireState,
        /// The state the record was actually in.
        found: PluginWireState,
    },
    /// The state change was refused by the allow-list; carries the id so
    /// logs do not have to reconstruct it.
    #[error("illegal transition for plugin `{id}`: {source}")]
    Transition {
        /// The plugin the transition was attempted on.
        id: String,
        /// The allow-list refusal.
        source: TransitionError,
    },
}
