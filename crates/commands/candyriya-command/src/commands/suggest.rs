//! Tab-completion providers for command arguments.
//!
//! Server names come from the directory in `try`-chain priority order;
//! player names come from the live roster on every completion request, so
//! they never go stale.

use std::sync::Arc;

use azalea_brigadier::context::CommandContext as BrigadierContext;
use azalea_brigadier::suggestion::{SuggestionProvider, Suggestions, SuggestionsBuilder};

use crate::source::CommandSource;
use crate::state::NetworkDirectory;

/// Suggests configured server names, priority order first, filtered by the
/// typed prefix.
pub(crate) struct ServerNames {
    pub(crate) directory: Arc<dyn NetworkDirectory>,
}

impl SuggestionProvider<CommandSource> for ServerNames {
    fn get_suggestions(
        &self,
        _context: BrigadierContext<CommandSource>,
        builder: SuggestionsBuilder,
    ) -> Suggestions {
        let remaining = builder.remaining_lowercase().to_owned();
        let mut servers = self.directory.servers();
        servers.sort_by(|a, b| {
            a.priority
                .cmp(&b.priority)
                .then_with(|| a.name.cmp(&b.name))
        });
        servers.dedup_by(|a, b| a.name == b.name);
        servers
            .iter()
            .filter(|server| server.name.to_lowercase().starts_with(remaining.as_str()))
            .fold(builder, |builder, server| builder.suggest(&server.name))
            .build()
    }
}

/// Suggests online player names plus, when `include_all`, the `ALL` token,
/// filtered by the typed prefix.
pub(crate) struct PlayerNames {
    pub(crate) directory: Arc<dyn NetworkDirectory>,
    pub(crate) include_all: bool,
}

impl SuggestionProvider<CommandSource> for PlayerNames {
    fn get_suggestions(
        &self,
        _context: BrigadierContext<CommandSource>,
        builder: SuggestionsBuilder,
    ) -> Suggestions {
        let remaining = builder.remaining_lowercase().to_owned();
        let mut names: Vec<String> = self
            .directory
            .online_players()
            .into_iter()
            .map(|player| player.name)
            .collect();
        names.sort();
        names.dedup();
        let builder = names
            .iter()
            .filter(|name| name.to_lowercase().starts_with(remaining.as_str()))
            .fold(builder, |builder, name| builder.suggest(name));
        // ALL is a first-class token for /send, not offered elsewhere
        if self.include_all && "all".starts_with(remaining.as_str()) {
            return builder.suggest("ALL").build();
        }
        builder.build()
    }
}
