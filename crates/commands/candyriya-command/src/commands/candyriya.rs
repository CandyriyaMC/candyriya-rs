//! `/candyriya` — the proxy admin root.
//!
//! Subcommands: `info`, `servers`, `switch`, `end`. The root itself carries
//! no permission; each subcommand carries its own gate, so an unauthorized
//! source cannot even parse the subtree it may not use.
//!
//! Output goes through the shared effects sink as i18n'd MiniMessage lines;
//! `/candyriya end` flips the shared
//! [`ShutdownHandle`](crate::ShutdownHandle) instead of touching the
//! scheduler itself — the cli watches the same flag and owns the shutdown.

use std::sync::Arc;
use std::time::Duration;

use azalea_brigadier::arguments::string_argument_type::{get_string, word};
use azalea_brigadier::builder::argument_builder::ArgumentBuilder;
use azalea_brigadier::builder::literal_argument_builder::literal;
use azalea_brigadier::context::CommandContext as BrigadierContext;

use candyriya_i18n::I18n;
use candyriya_utils::format_uptime;

use crate::command::{CandyCommand, argument_with_suggestions};
use crate::source::CommandSource;
use crate::state::CommandState;
use crate::text::escape_minimessage;

use super::common::{out, switch_to};
use super::suggest::{PlayerNames, ServerNames};
use crate::snapshot::{BackendSnapshot, BackendState};

fn permission_check(node: &'static str) -> impl Fn(&CommandSource) -> bool + Send + Sync + 'static {
    move |source: &CommandSource| source.has_permission(node)
}

fn state_fragment(t: &I18n, state: BackendState) -> String {
    match state {
        BackendState::Online => t.t("candyriya.state.online", &[]),
        BackendState::Offline => t.t("candyriya.state.offline", &[]),
        BackendState::Unknown => t.t("candyriya.state.unknown", &[]),
    }
}

fn ping_text(t: &I18n, ping: Option<Duration>) -> String {
    match ping {
        Some(duration) => t.t(
            "candyriya.ping.value",
            &[("ms", duration.as_millis().to_string().as_str())],
        ),
        None => t.t("candyriya.ping.unknown", &[]),
    }
}

fn backend_line(t: &I18n, backend: &BackendSnapshot, key: &str) -> String {
    let state = state_fragment(t, backend.state);
    let ping = ping_text(t, backend.ping);
    t.t(
        key,
        &[
            ("name", escape_minimessage(&backend.name).as_str()),
            ("address", escape_minimessage(&backend.address).as_str()),
            ("state", state.as_str()),
            ("ping", ping.as_str()),
            ("players", backend.players.to_string().as_str()),
            ("max_players", backend.max_players.to_string().as_str()),
        ],
    )
}

fn handle_info(state: &CommandState, source: &CommandSource) -> i32 {
    let t = state.locales.pick(source);
    let snapshot = state.directory.snapshot();
    out(
        state,
        source,
        t.t(
            "candyriya.info.header",
            &[("version", escape_minimessage(&snapshot.version).as_str())],
        ),
    );
    out(
        state,
        source,
        t.t(
            "candyriya.info.uptime",
            &[("uptime", format_uptime(snapshot.uptime).as_str())],
        ),
    );
    out(
        state,
        source,
        t.t(
            "candyriya.info.config",
            &[
                ("config", escape_minimessage(&snapshot.config_name).as_str()),
                (
                    "level",
                    escape_minimessage(&snapshot.effective_log_level).as_str(),
                ),
            ],
        ),
    );
    out(
        state,
        source,
        t.t(
            "candyriya.info.players",
            &[("count", snapshot.players.len().to_string().as_str())],
        ),
    );
    out(
        state,
        source,
        t.t(
            "candyriya.info.connections",
            &[
                ("active", snapshot.connections.active.to_string().as_str()),
                ("total", snapshot.connections.accepted.to_string().as_str()),
                (
                    "rejected",
                    snapshot.connections.rejected.to_string().as_str(),
                ),
            ],
        ),
    );
    if snapshot.backends.is_empty() {
        out(state, source, t.t("candyriya.info.no_backends", &[]));
    } else {
        out(state, source, t.t("candyriya.info.backends_header", &[]));
        for backend in &snapshot.backends {
            let line = backend_line(t, backend, "candyriya.info.backend");
            out(state, source, line);
        }
    }
    1
}

fn handle_servers(state: &CommandState, source: &CommandSource) -> i32 {
    let t = state.locales.pick(source);
    let snapshot = state.directory.snapshot();
    out(state, source, t.t("candyriya.servers.header", &[]));
    if snapshot.backends.is_empty() {
        out(state, source, t.t("candyriya.servers.none", &[]));
        return 1;
    }
    for backend in &snapshot.backends {
        let line = backend_line(t, backend, "candyriya.servers.entry");
        out(state, source, line);
    }
    1
}

fn handle_end(
    state: &CommandState,
    source: &CommandSource,
    shutdown: &crate::ShutdownHandle,
) -> i32 {
    let t = state.locales.pick(source);
    if shutdown.trigger() {
        out(state, source, t.t("candyriya.command.end", &[]));
        1
    } else {
        out(state, source, t.t("candyriya.command.end_already", &[]));
        0
    }
}

/// `/candyriya` — the proxy admin root. Subcommands carry their own gates.
pub(crate) struct CandyriyaCommand {
    state: CommandState,
    shutdown: crate::ShutdownHandle,
}

impl CandyriyaCommand {
    pub(crate) fn new(state: CommandState, shutdown: crate::ShutdownHandle) -> Self {
        Self { state, shutdown }
    }
}

impl CandyCommand for CandyriyaCommand {
    fn name(&self) -> &str {
        "candyriya"
    }

    fn build(&self, lit: ArgumentBuilder<CommandSource>) -> ArgumentBuilder<CommandSource> {
        let parts = self.state.clone();
        let info_parts = parts.clone();
        let servers_parts = parts.clone();
        let switch_parts = parts.clone();
        let end_parts = parts.clone();
        let end_shutdown = self.shutdown.clone();
        let directory = Arc::clone(&parts.directory);

        lit.then(
            literal::<CommandSource>("info")
                .requires(permission_check("candyriya.command.info"))
                .executes(move |ctx: &BrigadierContext<CommandSource>| {
                    handle_info(&info_parts, &ctx.source)
                }),
        )
        .then(
            literal::<CommandSource>("servers")
                .requires(permission_check("candyriya.command.servers"))
                .executes(move |ctx: &BrigadierContext<CommandSource>| {
                    handle_servers(&servers_parts, &ctx.source)
                }),
        )
        .then(
            literal::<CommandSource>("switch")
                .requires(permission_check("candyriya.command.switch"))
                .then(
                    argument_with_suggestions(
                        "player",
                        word(),
                        PlayerNames {
                            directory,
                            include_all: false,
                        },
                    )
                    .then(
                        argument_with_suggestions(
                            "server",
                            word(),
                            ServerNames {
                                directory: Arc::clone(&parts.directory),
                            },
                        )
                        .executes(
                            move |ctx: &BrigadierContext<CommandSource>| {
                                let player = get_string(ctx, "player").unwrap_or_default();
                                let server = get_string(ctx, "server").unwrap_or_default();
                                switch_to(&switch_parts, &ctx.source, &player, &server)
                            },
                        ),
                    ),
                ),
        )
        .then(
            literal::<CommandSource>("end")
                .requires(permission_check("candyriya.command.end"))
                .executes(move |ctx: &BrigadierContext<CommandSource>| {
                    handle_end(&end_parts, &ctx.source, &end_shutdown)
                }),
        )
    }
}
