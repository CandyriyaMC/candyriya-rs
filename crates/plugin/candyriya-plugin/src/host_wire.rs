//! Wire messages for the supervised `host` unit.
//!
//! The `host` unit is the supervised owner of the plugin-facing core
//! services: the plugin-to-plugin `callApi` fan-out, the live play-state
//! roster and the backend name list. This module is the contract between
//! the two ends of that link — the plugin crate (whose host functions
//! build the messages) and the proxy crate (which serves them). It is
//! deliberately small: three request variants, three reply variants,
//! zero serialization — the tree type-erases envelopes and downcasts on
//! the callee. Internal workspace wiring, not a stabilized API; adding a
//! variant is an explicit, reviewable change.
//!
//! # Reply cost
//!
//! Replies carry owned data: call outputs cross as raw bytes, the roster
//! as fresh [`PlayerRef`]s (one string pair per online player), names as
//! cloned `String`s. Nothing borrows from the state the unit holds, so a
//! caller never pins an old snapshot by accident.
//!
//! # Capability
//!
//! The `host` unit is a sibling of its callers' other services but a
//! *child of the root*: code outside any unit body attributes its calls
//! to the root, which is an ancestor of the unit — the subtree rule is
//! enough and no grant is needed. The unit's own outbound calls (the
//! `Names` fetch into the `route` unit) run inside the unit body with the
//! unit itself as the caller, so the spawner installs a grant to the
//! route unit at spawn time — the same inherited-fd shape every other
//! sibling call in the tree uses.

use candyriya_plugin_api::PlayerRef;

/// Messages the `host` unit answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostMsg {
    /// Fan `args` into plugin `target_plugin`'s exported `function` on
    /// behalf of `caller`.
    ///
    /// `args` is the JSON array as the calling plugin sent it, already
    /// serialized — the target's export receives these exact bytes.
    /// `caller` is the calling plugin's id (the host installs it on the
    /// target for the duration of the call); it is absent only when the
    /// caller is not a plugin.
    CallApi {
        /// The id of the plugin to call.
        target_plugin: String,
        /// The export to invoke on the target.
        function: String,
        /// Raw JSON array passed to the target's export.
        args: Vec<u8>,
        /// The calling plugin's id, when there is one.
        caller: Option<String>,
    },
    /// Snapshot of the live play-state roster: every session currently
    /// in the play state, sorted by `(name, conn_id)`.
    Roster,
    /// Names of the configured backends, in registration order. Empty
    /// when routing is disabled (no `[servers]` configured).
    Names,
}

/// Replies from the `host` unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostResp {
    /// The dispatcher's outcome, mirroring the `callApi` answer semantics:
    /// `Ok(output)` is the target's output bytes; `Err(message)` is the
    /// human-readable reason the call did not happen (or the target
    /// failed). The message is the dispatcher's own — host-side transport
    /// failures are reported by the tree as `CallError` instead and never
    /// masquerade as a target answer.
    CallApi(Result<Vec<u8>, String>),
    /// The live roster at the time the handler ran. Empty when no player
    /// is in the play state.
    Roster(Vec<PlayerRef>),
    /// The configured backend names, in registration order. Empty when
    /// routing is disabled or the names could not be read — the caller
    /// decides what an empty list means for its own contract.
    Names(Vec<String>),
}
