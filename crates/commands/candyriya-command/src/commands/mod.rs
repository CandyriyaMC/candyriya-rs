//! The built-in command set: `/candyriya` (info, servers, switch, end,
//! reload), `/glist`, `/send`, and its `/server` and `/goto` sender-switch
//! aliases.
//!
//! `/candyriya` is the admin-facing root; its subcommands are permission
//! gated. The player-facing commands are ungated, velocity-style: listing
//! the network (`/glist`) and routing players (`/send`, `/server`, `/goto`)
//! carry no permission node of their own.
//!
//! [`crate::CommandRegistry`] is the entry point the proxy uses to register
//! the whole set; the individual types stay public so tests can wire them
//! one by one.

pub mod glist;
pub mod send;

pub(crate) mod candyriya;
mod common;
mod hidden;
mod reload;
mod suggest;

pub(crate) use candyriya::CandyriyaCommand;

pub(crate) use hidden::StaticHidden;

pub use glist::{GlistCommand, GlistReport, GlistServerEntry};
pub use send::{SendCommand, SendMode};
