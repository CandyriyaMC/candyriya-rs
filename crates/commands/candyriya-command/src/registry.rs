//! The proxy's built-in command registry.
//!
//! Nothing here is wired by magic, everything by hand:
//!
//! - [`CommandRegistry`] holds the shared [`CommandState`] (locales,
//!   directory, effects) and the one-way [`ShutdownHandle`] `/candyriya end`
//!   flips; the cli watches the same flag and initiates the real shutdown.
//!   A [`ReloadFn`](crate::ReloadFn) can be wired in with
//!   [`CommandRegistry::with_reload`] — the cli hands over the real reload;
//!   without one `/candyriya reload` reports that reload is not wired.
//!   A [`PluginViewPrefix`](crate::PluginViewPrefix) can be wired in with
//!   [`CommandRegistry::with_plugin_view`] the same way — the cli hands
//!   over the plugin control surface; without one every `/candyriya plugin`
//!   subcommand reports that plugin control is not wired.
//! - [`CommandRegistry::register`] validates and attaches every built-in
//!   command to a [`Dispatcher`] as one atomic batch, then attaches the
//!   internal pre-dispatch hook. A failure leaves the dispatcher tree
//!   untouched; the hook is attached only when registration succeeds.
//! - [`CommandRegistry::dispatcher`] builds a standalone [`Dispatcher`]
//!   carrying exactly the built-in set.

use std::sync::Arc;

use crate::command::CandyCommand;
use crate::commands::{CandyriyaCommand, GlistCommand, SendCommand, StaticHidden};
use crate::dispatcher::Dispatcher;
use crate::effects::ReloadFn;
use crate::error::CommandError;
use crate::plugin::PluginViewPrefix;
use crate::state::CommandState;

/// Shared registry for every built-in command.
///
/// Constructed with the state the commands render from and the shutdown
/// flag the cli watches. The individual commands are assembled at
/// registration time, so the registry is cheap to build and clone-free.
pub struct CommandRegistry {
    state: CommandState,
    shutdown: crate::ShutdownHandle,
    reload: Option<ReloadFn>,
    plugin_view: Option<PluginViewPrefix>,
}

impl CommandRegistry {
    /// Creates the registry from the shared state and the shutdown flag.
    ///
    /// `shutdown` is the flag the cli hands to its drain path; the same flag
    /// drives `/candyriya end`.
    pub fn new(state: CommandState, shutdown: crate::ShutdownHandle) -> Self {
        Self {
            state,
            shutdown,
            reload: None,
            plugin_view: None,
        }
    }

    /// Wires the reload closure `/candyriya reload` runs through.
    ///
    /// The closure is called synchronously with the requested scope and
    /// must never panic; see [`ReloadFn`]. Without one, the command
    /// answers that reload is not wired in this build.
    #[must_use]
    pub fn with_reload(mut self, reload: ReloadFn) -> Self {
        self.reload = Some(reload);
        self
    }

    /// Wires the plugin control surface `/candyriya plugin` runs through.
    ///
    /// The handle is the composition root's to build (and to drop): the
    /// command layer only parses, gates and renders. Without one, every
    /// `/candyriya plugin` subcommand answers that plugin control is not
    /// wired in this build.
    #[must_use]
    pub fn with_plugin_view(mut self, plugin_view: PluginViewPrefix) -> Self {
        self.plugin_view = Some(plugin_view);
        self
    }

    /// The shared shutdown flag, cloned for the drain path to watch.
    pub fn shutdown_handle(&self) -> crate::ShutdownHandle {
        self.shutdown.clone()
    }

    fn built_in_commands(&self) -> Vec<Box<dyn CandyCommand>> {
        vec![
            Box::new(GlistCommand::new(self.state.clone())),
            Box::new(SendCommand::command(self.state.clone())),
            Box::new(SendCommand::server(self.state.clone())),
            Box::new(SendCommand::goto(self.state.clone())),
            Box::new(CandyriyaCommand::new(
                self.state.clone(),
                self.shutdown.clone(),
                self.reload.clone(),
                self.plugin_view.clone(),
            )),
        ]
    }

    /// Registers every built-in command on `dispatcher`, then attaches the
    /// internal pre-dispatch hook.
    ///
    /// The command batch is validated and registered all-or-nothing (see
    /// `Dispatcher::register_slice`); on error the tree is unchanged and the
    /// hook is not attached.
    ///
    /// # Errors
    ///
    /// The same validation errors [`crate::Dispatcher::new`] reports:
    /// duplicate root literals (including against commands already on the
    /// dispatcher), invalid names and invalid permission nodes.
    pub fn register(&self, dispatcher: &mut Dispatcher) -> Result<(), CommandError> {
        let commands = self.built_in_commands();
        let refs: Vec<&dyn CandyCommand> = commands.iter().map(Box::as_ref).collect();
        dispatcher.register_slice(&refs)?;
        dispatcher.attach_hidden(StaticHidden {
            effects: Arc::clone(&self.state.effects),
        });
        Ok(())
    }

    /// Builds a standalone [`Dispatcher`] carrying exactly the built-in
    /// command set.
    ///
    /// # Errors
    ///
    /// The same errors as [`CommandRegistry::register`].
    pub fn dispatcher(&self) -> Result<Dispatcher, CommandError> {
        let mut dispatcher = Dispatcher::empty();
        self.register(&mut dispatcher)?;
        Ok(dispatcher)
    }
}
