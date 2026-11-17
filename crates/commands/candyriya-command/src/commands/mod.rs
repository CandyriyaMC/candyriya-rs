//! The player-facing built-in commands.
//!
//! The player command set is intentionally small: `/glist`, `/send`, and its
//! `/server` and `/goto` aliases. Admin and introspection tooling live in
//! their own commands and are not part of this module.
//!
//! [`player_commands`] is the entry point the proxy uses to register the
//! whole set; the individual types stay public so tests can wire them one by
//! one.

pub mod glist;
pub mod send;

pub use glist::{GlistCommand, GlistReport, GlistServerEntry};
pub use send::{SendCommand, SendMode};

use crate::command::CandyCommand;
use crate::state::CommandState;

/// Builds every player-facing command in this crate.
///
/// Registration order matches the natural reading order (`glist`, then the
/// switch commands). The `/send` full form, `/server` and `/goto` share one
/// implementation; only their root literal, permission gate and self-target
/// resolution differ.
#[must_use]
pub fn player_commands(state: &CommandState) -> Vec<Box<dyn CandyCommand>> {
    vec![
        Box::new(GlistCommand::new(state.clone())),
        Box::new(SendCommand::command(state.clone())),
        Box::new(SendCommand::server(state.clone())),
        Box::new(SendCommand::goto(state.clone())),
    ]
}
