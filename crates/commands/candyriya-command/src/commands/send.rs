//! `/send <target> <server>` — move player(s) to another server.
//!
//! The `/server <server>` and `/goto <server>` literals are the same command
//! tree with a different default for the target: instead of naming it, they
//! switch the **invoking player** (velocity-style sender switch). All three
//! share the implementation in [`send`].
//!
//! - `/send` requires the `candyriya.command.send` permission (default-deny,
//!   matching the admin namespace) and takes an explicit target.
//! - `/server` and `/goto` are open to everyone: switching yourself is the
//!   expected player affordance. Non-player sources are told (in-chat) that
//!   only players can switch themselves.
//!
//! `ALL` as the target switches every online player. A named target must be
//! online at dispatch time; anything else is rejected before any switch
//! request reaches the proxy. The server name is always validated against
//! the configured servers — unknown names are rejected, and no switch is
//! applied.

use azalea_brigadier::arguments::string_argument_type::{get_string, word};
use azalea_brigadier::builder::argument_builder::ArgumentBuilder;
use azalea_brigadier::context::CommandContext as BrigadierContext;
use azalea_brigadier::errors::{BuiltInError, CommandSyntaxError};
use azalea_brigadier::suggestion::{SuggestionProvider, Suggestions, SuggestionsBuilder};

use crate::command::{CandyCommand, argument_with_suggestions};
use crate::source::{CommandSource, PlayerRef};
use crate::state::{CommandState, NetworkDirectory, SwitchRequest, SwitchTarget};
use crate::text::escape_minimessage;

/// Which literal this [`SendCommand`] instance backs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendMode {
    /// `/send <target> <server>` — the full, permission-gated form.
    Command,
    /// `/server <server>` — switch the invoking player.
    Server,
    /// `/goto <server>` — same as `/server`, kept as an alias.
    Goto,
}

/// The shared body of `/send`, `/server` and `/goto`.
///
/// `target` is either a player name (matched case-insensitively against the
/// online list) or the literal `ALL` (matched case-insensitively). `server`
/// must name a configured server, or the request is rejected before anything
/// happens.
///
/// The same code path serves the aliases: they pass the invoking player's
/// name as `target`.
///
/// # Errors
///
/// Returns the user-facing problem as a [`CommandSyntaxError`] of kind
/// `DispatcherParseException { message }`. The message is the MiniMessage
/// line that was already delivered to the invoking source via the effects
/// sink, so the error path is for programmatic callers and tests — the
/// player always sees the rendered chat line.
pub fn send(
    state: &CommandState,
    ctx: &BrigadierContext<CommandSource>,
    target: &str,
    server: &str,
) -> Result<i32, CommandSyntaxError> {
    let origin = ctx.source.origin().clone();
    let server = server.trim();
    let servers = state.directory.servers();
    if !servers.iter().any(|known| known.name == server) {
        let message = state.i18n.t(
            "proxy.send.unknown_server",
            &[("server", escape_minimessage(server).as_str())],
        );
        state.effects.message(&origin, message.clone());
        // no suitable BuiltInError kind exists for execution-time user errors;
        // DispatcherParseException carries the (already rendered) message
        return Err(user_error(message));
    }

    let target = target.trim();
    if target.eq_ignore_ascii_case("ALL") {
        state
            .effects
            .switch(SwitchRequest::new(SwitchTarget::All, server));
        let message = state.i18n.t(
            "proxy.send.transferring_all",
            &[("server", escape_minimessage(server).as_str())],
        );
        state.effects.message(&origin, message);
        return Ok(1);
    }

    let players = state.directory.online_players();
    let Some(player) = players
        .iter()
        .find(|player| player.name.eq_ignore_ascii_case(target))
    else {
        let message = state.i18n.t(
            "proxy.send.player_not_online",
            &[("player", escape_minimessage(target).as_str())],
        );
        state.effects.message(&origin, message.clone());
        return Err(user_error(message));
    };
    let recipient = PlayerRef::new(player.uuid, player.name.clone());
    state
        .effects
        .switch(SwitchRequest::new(SwitchTarget::One(recipient), server));
    let message = state.i18n.t(
        "proxy.send.transferring",
        &[
            ("player", escape_minimessage(&player.name).as_str()),
            ("server", escape_minimessage(server).as_str()),
        ],
    );
    state.effects.message(&origin, message);
    Ok(1)
}

/// Wraps a rendered user-facing line in a brigadier syntax error, for the
/// dispatcher's error path. The kind carries no parse position — the command
/// simply failed to run.
fn user_error(message: String) -> CommandSyntaxError {
    CommandSyntaxError::create(
        BuiltInError::DispatcherParseException {
            message: message.clone(),
        },
        message,
    )
}

/// The player-facing switch command; one implementation behind three
/// literals (`/send`, `/server`, `/goto`).
pub struct SendCommand {
    state: CommandState,
    mode: SendMode,
}

impl SendCommand {
    /// The full `/send <target> <server>` form.
    pub fn command(state: CommandState) -> Self {
        Self {
            state,
            mode: SendMode::Command,
        }
    }

    /// `/server <server>` — switch the invoking player.
    pub fn server(state: CommandState) -> Self {
        Self {
            state,
            mode: SendMode::Server,
        }
    }

    /// `/goto <server>` — alias of `/server`.
    pub fn goto(state: CommandState) -> Self {
        Self {
            state,
            mode: SendMode::Goto,
        }
    }

    /// Which literal this instance backs.
    pub fn mode(&self) -> SendMode {
        self.mode
    }
}

impl CandyCommand for SendCommand {
    fn name(&self) -> &str {
        match self.mode {
            SendMode::Command => "send",
            SendMode::Server => "server",
            SendMode::Goto => "goto",
        }
    }

    fn permission(&self) -> Option<&str> {
        match self.mode {
            // switching strangers is an admin-ish power; switching yourself is not
            SendMode::Command => Some("candyriya.command.send"),
            SendMode::Server | SendMode::Goto => None,
        }
    }

    fn build(&self, lit: ArgumentBuilder<CommandSource>) -> ArgumentBuilder<CommandSource> {
        let state = self.state.clone();
        let directory = std::sync::Arc::clone(&self.state.directory);
        match self.mode {
            SendMode::Command => lit.then(
                argument_with_suggestions("target", word(), TargetNames { directory }).then(
                    argument_with_suggestions(
                        "server",
                        word(),
                        ServerNames {
                            directory: std::sync::Arc::clone(&self.state.directory),
                        },
                    )
                    .executes_result(move |ctx| {
                        let (Some(target), Some(server)) =
                            (get_string(ctx, "target"), get_string(ctx, "server"))
                        else {
                            return Err(user_error("missing argument".to_owned()));
                        };
                        send(&state, ctx, &target, &server)
                    }),
                ),
            ),
            SendMode::Server | SendMode::Goto => lit.then(
                argument_with_suggestions("server", word(), ServerNames { directory })
                    .executes_result(move |ctx| {
                        let Some(server) = get_string(ctx, "server") else {
                            return Err(user_error("missing argument".to_owned()));
                        };
                        match ctx.source.player_name() {
                            Some(name) => send(&state, ctx, name, &server),
                            // velocity semantics: non-players cannot switch themselves
                            None => {
                                let origin = ctx.source.origin().clone();
                                let message = state.i18n.t("proxy.send.only_players", &[]);
                                state.effects.message(&origin, message.clone());
                                Err(user_error(message))
                            }
                        }
                    }),
            ),
        }
    }
}

/// Suggests configured server names, priority order first, filtered by the
/// typed prefix.
struct ServerNames {
    directory: std::sync::Arc<dyn NetworkDirectory>,
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

/// Suggests online player names plus the `ALL` token, filtered by the typed
/// prefix.
struct TargetNames {
    directory: std::sync::Arc<dyn NetworkDirectory>,
}

impl SuggestionProvider<CommandSource> for TargetNames {
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
        // ALL first-class token, always offered when the prefix matches
        if "all".starts_with(remaining.as_str()) {
            return builder.suggest("ALL").build();
        }
        builder.build()
    }
}
