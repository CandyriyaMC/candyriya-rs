//! # candyriya-command
//!
//! Brigadier command wiring for the proxy: the [`CommandSource`] type Azalea
//! Brigadier dispatches with, the console source, the `CandyCommand` trait,
//! [`Dispatcher`] — a thin adapter over `azalea_brigadier` that builds the
//! command tree from command implementations — and the player-facing
//! commands in [`player_commands`].
//!
//! ## What this crate is
//!
//! - **The registry.** [`build`] takes a slice of [`CandyCommand`]s and hands
//!   back a fully-assembled `azalea_brigadier::CommandDispatcher<CommandSource>`,
//!   rejecting duplicate and malformed root literals up front.
//! - **The source of truth for invocation context.** [`CommandSource`] carries
//!   the origin (console / player / backend), the locale and the permission
//!   state; [`CommandContext`] is the small audit-friendly view
//!   (`{origin, args_raw}`) of a parsed invocation.
//! - **Permission enforcement at the tree level.** Commands that declare a
//!   permission node get a `requires` clause on their root literal, backed by
//!   the `candyriya-permissions` engine — no per-handler after-the-fact
//!   checking.
//! - **The player command set.** [`player_commands`] builds `/glist`,
//!   `/send <target> <server>` and its `/server` / `/goto` sender-switch
//!   aliases as [`CandyCommand`]s over a [`CommandState`] the proxy injects
//!   (i18n tables, read-only server/online-player view, effects sink). No
//!   network, no async, no I/O inside this crate — parsing and tree building
//!   are pure, effects are calls into whatever the proxy wires in.
//!
//! ## What this crate is not
//!
//! - No network, no async, no I/O. Commands never touch sockets; the effects
//!   sink ([`ProxyEffects`]) and the directory ([`NetworkDirectory`]) are
//!   implemented elsewhere.
//! - No other command implementations. Admin and introspection commands live
//!   in their own work, not here.
//! - No Java-like class hierarchies. Brigadier's literal/argument builders
//!   are the hierarchy; the adapter stays thin.
//!
//! ## Example
//!
//! ```
//! use azalea_brigadier::builder::argument_builder::ArgumentBuilder;
//! use candyriya_command::{CandyCommand, CommandSource, Dispatcher};
//!
//! struct FooCommand;
//! impl CandyCommand for FooCommand {
//!     fn name(&self) -> &str {
//!         "foo"
//!     }
//!
//!     fn build(&self, lit: ArgumentBuilder<CommandSource>) -> ArgumentBuilder<CommandSource> {
//!         lit.executes(|_| 42)
//!     }
//! }
//!
//! let dispatcher = Dispatcher::new(&[&FooCommand]).expect("valid command");
//! assert_eq!(dispatcher.execute("/foo", CommandSource::console()).unwrap(), 42);
//! ```
#![forbid(unsafe_code)]

mod command;
mod context;
mod dispatcher;
mod error;
mod source;
mod state;
mod text;

pub mod commands;

pub use azalea_brigadier::builder::argument_builder::ArgumentBuilder;

pub use command::{CandyCommand, argument_with_suggestions};
pub use commands::{
    GlistCommand, GlistReport, GlistServerEntry, SendCommand, SendMode, player_commands,
};
pub use context::CommandContext;
pub use dispatcher::{Dispatcher, build};
pub use error::CommandError;
pub use source::{CommandOrigin, CommandSource, PlayerRef, SourceKind};
pub use state::{
    CommandState, NetworkDirectory, OnlinePlayer, ProxyEffects, ServerInfo, SwitchRequest,
    SwitchTarget,
};
pub use text::escape_minimessage;
