//! The `CandyCommand` trait and helpers for building command trees.

use std::sync::Arc;

use azalea_brigadier::arguments::ArgumentType;
use azalea_brigadier::builder::argument_builder::ArgumentBuilder;
use azalea_brigadier::builder::required_argument_builder::Argument;
use azalea_brigadier::suggestion::SuggestionProvider;

use crate::source::CommandSource;

/// A command implemented in this workspace.
///
/// Implementors describe one command's root literal and tree; the dispatcher
/// ([`crate::Dispatcher`]) wires them into Azalea Brigadier with permission
/// gating. There is no inheritance hierarchy — Brigadier's literal/argument
/// builders are the hierarchy.
///
/// `build` receives the already-validated root literal builder (permission
/// requirement attached when [`CandyCommand::permission`] is set) and returns
/// the fully-assembled subtree.
pub trait CandyCommand: Send + Sync {
    /// The root literal of the command, e.g. `server` (no leading slash).
    ///
    /// Validated by [`crate::build`]: lowercase ASCII, digits and `_`/`-`,
    /// must start with a lowercase letter, at most 64 bytes.
    fn name(&self) -> &str;

    /// The permission node required to see and execute the command.
    ///
    /// Checked via [`CommandSource::has_permission`] when the command tree is
    /// built. `None` (the default) means the command is always usable.
    fn permission(&self) -> Option<&str> {
        None
    }

    /// Assemble the command tree on top of the root literal.
    fn build(&self, lit: ArgumentBuilder<CommandSource>) -> ArgumentBuilder<CommandSource>;
}

/// Builds a required-argument node with a custom suggestion provider attached.
///
/// Azalea Brigadier has no `.suggests()` builder, so commands that want tab
/// completion go through this. The provider's suggestions are offered for the
/// argument's position.
///
/// ```
/// use azalea_brigadier::prelude::*;
/// use azalea_brigadier::suggestion::SuggestionProvider;
/// use azalea_brigadier::suggestion::Suggestions;
/// use azalea_brigadier::context::CommandContext;
/// use azalea_brigadier::suggestion::SuggestionsBuilder;
/// use candyriya_command::{argument_with_suggestions, CommandSource};
///
/// struct ServerNames;
/// impl SuggestionProvider<CommandSource> for ServerNames {
///     fn get_suggestions(
///         &self,
///         _context: CommandContext<CommandSource>,
///         builder: SuggestionsBuilder,
///     ) -> Suggestions {
///         builder.suggest("lobby").suggest("pvp").build()
///     }
/// }
///
/// let node = argument_with_suggestions("target", word(), ServerNames);
/// ```
pub fn argument_with_suggestions<P>(
    name: &str,
    parser: impl ArgumentType + Send + Sync + 'static,
    provider: P,
) -> ArgumentBuilder<CommandSource>
where
    P: SuggestionProvider<CommandSource> + Send + Sync + 'static,
{
    ArgumentBuilder::new(Argument::new(name, Arc::new(parser), Some(Arc::new(provider))).into())
}
