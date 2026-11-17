//! # candyriya-command
//!
//! Brigadier command wiring for the proxy: the [`CommandSource`] type Azalea
//! Brigadier dispatches with, the console source, the `CandyCommand` trait,
//! [`Dispatcher`] — a thin adapter over `azalea_brigadier` that builds the
//! command tree from command implementations — and the built-in command
//! registry.
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
//!   permission node get a `requires` clause on their node, backed by the
//!   `candyriya-permissions` engine — no per-handler after-the-fact checking.
//! - **The built-in commands.** [`CommandRegistry`] assembles the whole set:
//!   `/candyriya` (`info`, `servers`, `switch`, `end`, `reload`), `/glist`,
//!   `/send` and its `/server` and `/goto` aliases. Output goes through the
//!   [`OutputSender`] seam as MiniMessage lines built from the embedded
//!   locales ([`Locales`]); live effects (switching a player) go through
//!   [`ProxyEffects`]; the shutdown flag (`/candyriya end`) is a shared
//!   [`ShutdownHandle`]; `/candyriya reload` runs through a [`ReloadFn`]
//!   wired with [`CommandRegistry::with_reload`]. See the `registry`
//!   module docs for the wiring model.
//!
//! ## What this crate is not
//!
//! - No network, no async, no I/O. Parsing and tree building are pure; the
//!   commands read the world through [`NetworkDirectory`] and mutate it
//!   through [`ProxyEffects`] — both implemented by the composition root
//!   (the cli), never here.
//! - No proxy-crate dependency. The registry is assembled over traits
//!   precisely so this crate builds standalone.
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
mod effects;
mod error;
mod locales;
mod registry;
mod snapshot;
mod source;
mod state;
mod text;

pub mod commands;

pub use azalea_brigadier::builder::argument_builder::ArgumentBuilder;

pub use command::{CandyCommand, argument_with_suggestions};
pub use commands::{GlistCommand, GlistReport, GlistServerEntry, SendCommand, SendMode};
pub use context::CommandContext;
pub use dispatcher::{Dispatcher, build};
pub use effects::{
    OutputSender, PluginPhaseReport, ProxyEffects, ReloadFn, ReloadReport, ReloadScope,
    ShutdownHandle, SwitchRequest, SwitchResult,
};
pub use error::CommandError;
pub use locales::Locales;
pub use registry::CommandRegistry;
pub use snapshot::{BackendSnapshot, BackendState, ConnectionStats, ProxySnapshot};
pub use source::{CommandOrigin, CommandSource, PlayerRef, SourceKind};
pub use state::{CommandState, NetworkDirectory, OnlinePlayer, ServerInfo};
pub use text::escape_minimessage;
