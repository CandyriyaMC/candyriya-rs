//! # candyriya-plugin-controller
//!
//! The contract crate of the native plugin controller: the 11-state wire
//! machine vocabulary, the controller's record for one plugin, the
//! in-memory registry and the wire vocabulary for the
//! controller <-> plugin-unit channel. The same philosophy as
//! `candyriya-native-abi`: boring, reviewable, zero runtime.
//!
//! What this crate is **not**: the machine. The transition table that
//! decides whether a plugin may move `DISCOVERED -> LOADING` lives here as
//! data ([`ALLOWED_TRANSITIONS`]) and is enforced by the registry, but the
//! semantics around it — dep gates, deadlines as transition data,
//! stale-generation rejection, the `disable_failed` latch — land in a
//! later phase as the real `machine::apply`. Today's [`machine::apply`] is
//! an always-`Ok` stub with the final signature, kept deliberately so the
//! integration can type against it.
//!
//! Hard rules, inherited from the epic:
//!
//! - **No unsafe.** The crate is `#![forbid(unsafe_code)]`.
//! - **No dlopen, no loader logic, no dynamic library machinery.** That
//!   boundary lives elsewhere and arrives in a later phase.
//! - **No async, no tokio, no runtime.** This is a pure data crate; the
//!   supervision tree and the task service wire it in from outside.
//! - **No state outside the 11 variants.** Failure is `reason` on the
//!   record plus the `UNLOADING -> UNLOADED` teardown path; there is no
//!   `Failed` state and there never will be.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod error;
mod machine;
mod record;
mod registry;
mod state;
mod wire;

pub use crate::error::{RegistryError, TransitionError};
pub use crate::machine::{PluginEvent, apply};
pub use crate::record::{ExitStatus, LoadedThreadHandle, PluginManifest, PluginRecord};
pub use crate::registry::{PluginRegistry, RegistrySnapshot};
pub use crate::state::{ALL_STATES, ALLOWED_TRANSITIONS, PluginWireState, Transitionable};
pub use crate::wire::{AckStatus, Generation, MAX_ACK_MESSAGE_BYTES, PluginAck, PluginMsg};
