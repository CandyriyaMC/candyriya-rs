//! `/send <target> <server>` — move player(s) to another server.
//!
//! The `/server <server>` and `/goto <server>` literals are the same command
//! tree with a different default for the target: instead of naming it, they
//! switch the **invoking player** (velocity-style sender switch). All three
//! share the handler in `switch_to` and the `ALL` fan-out in `send`.
//!
//! None of the three is gated: switching yourself is the expected player
//! affordance, and listing/routing is left to server operators via
//! `/candyriya`. Non-player sources are told (in-chat) that only players
//! can switch themselves.
//!
//! `ALL` (case-insensitive) as the target switches every online player. A
//! named target must be online at dispatch time; anything else is rejected
//! before any switch request reaches the proxy. The server name is always
//! validated against the configured servers — unknown names are rejected,
//! and no switch is applied.

use azalea_brigadier::arguments::string_argument_type::{get_string, word};
use azalea_brigadier::builder::argument_builder::ArgumentBuilder;
use azalea_brigadier::context::CommandContext as BrigadierContext;

use crate::command::{CandyCommand, argument_with_suggestions};
use crate::source::CommandSource;
use crate::state::CommandState;
use crate::text::escape_minimessage;

use super::common::{out, resolve_server, switch_to, unknown_server_line};
use super::suggest::{PlayerNames, ServerNames};

/// Which literal this [`SendCommand`] instance backs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendMode {
    /// `/send <target> <server>` — the full form.
    Command,
    /// `/server <server>` — switch the invoking player.
    Server,
    /// `/goto <server>` — same as `/server`, kept as an alias.
    Goto,
}

/// The shared body of `/send`.
///
/// `server` must name a configured server, or the request is rejected before
/// anything happens. `target` is either a player name (matched
/// case-insensitively against the online list) or the literal `ALL`
/// (matched case-insensitively): the latter fans out one switch request per
/// online player and prints a moved/failed summary.
///
/// Returns the brigadier exit code (`1` when everything moved, `0` otherwise).
pub(crate) fn send(
    state: &CommandState,
    source: &CommandSource,
    target: &str,
    server: &str,
) -> i32 {
    let t = state.locales.pick(source);
    let Some(resolved) = resolve_server(&*state.directory, server) else {
        out(
            state,
            source,
            unknown_server_line(t, &*state.directory, server),
        );
        return 0;
    };

    let target = target.trim();
    if !target.eq_ignore_ascii_case("all") {
        return switch_to(state, source, target, &resolved);
    }

    let players = state.directory.online_players();
    if players.is_empty() {
        out(
            state,
            source,
            t.t("candyriya.command.send_none_online", &[]),
        );
        return 0;
    }
    out(
        state,
        source,
        t.t(
            "candyriya.command.send_all",
            &[
                ("count", players.len().to_string().as_str()),
                ("server", escape_minimessage(&resolved).as_str()),
            ],
        ),
    );
    let mut moved = 0_u32;
    let mut failed = 0_u32;
    for player in &players {
        let request = crate::SwitchRequest::new(
            crate::PlayerRef::new(player.uuid, player.name.clone()),
            resolved.clone(),
        );
        match state.effects.switch(request) {
            crate::SwitchResult::Switched { .. } => moved += 1,
            failure => {
                failed += 1;
                let (line, _) = super::common::switch_result_line(t, &*state.directory, failure);
                out(state, source, line);
            }
        }
    }
    out(
        state,
        source,
        t.t(
            "candyriya.command.send_summary",
            &[
                ("ok", moved.to_string().as_str()),
                ("failed", failed.to_string().as_str()),
            ],
        ),
    );
    if failed == 0 { 1 } else { 0 }
}

/// The switch command; one implementation behind three literals
/// (`/send`, `/server`, `/goto`).
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

    fn build(&self, lit: ArgumentBuilder<CommandSource>) -> ArgumentBuilder<CommandSource> {
        let state = self.state.clone();
        let directory = std::sync::Arc::clone(&self.state.directory);
        match self.mode {
            SendMode::Command => lit.then(
                argument_with_suggestions(
                    "target",
                    word(),
                    PlayerNames {
                        directory,
                        include_all: true,
                    },
                )
                .then(
                    argument_with_suggestions(
                        "server",
                        word(),
                        ServerNames {
                            directory: std::sync::Arc::clone(&self.state.directory),
                        },
                    )
                    .executes(move |ctx: &BrigadierContext<CommandSource>| {
                        let (Some(target), Some(server)) =
                            (get_string(ctx, "target"), get_string(ctx, "server"))
                        else {
                            return 0;
                        };
                        send(&state, &ctx.source, target.as_str(), server.as_str())
                    }),
                ),
            ),
            SendMode::Server | SendMode::Goto => lit.then(
                argument_with_suggestions("server", word(), ServerNames { directory }).executes(
                    move |ctx: &BrigadierContext<CommandSource>| {
                        let Some(server) = get_string(ctx, "server") else {
                            return 0;
                        };
                        match ctx.source.player_name() {
                            Some(name) => {
                                let name = name.to_owned();
                                switch_to(&state, &ctx.source, &name, server.as_str())
                            }
                            // velocity semantics: non-players cannot switch themselves
                            None => {
                                let source = &ctx.source;
                                let t = state.locales.pick(source);
                                out(&state, source, t.t("candyriya.command.players_only", &[]));
                                0
                            }
                        }
                    },
                ),
            ),
        }
    }
}
