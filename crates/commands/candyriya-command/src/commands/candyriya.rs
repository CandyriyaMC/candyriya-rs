//! `/candyriya` — the proxy admin root.
//!
//! Subcommands: `info`, `servers`, `switch`, `end`, `reload`, `ps`, `tree`, `plugin`.
//! The watch variant of `ps` is console-only; the REPL runs it after
//! dispatch through the shared `PsWatchHandle` latch.
//! The root itself carries no permission; each subcommand carries its own
//! gate, so an unauthorized source cannot even parse the subtree it may not use.
//!
//! Output goes through the shared effects sink as i18n'd MiniMessage lines;
//! `/candyriya end` flips the shared
//! [`ShutdownHandle`](crate::ShutdownHandle) instead of touching the
//! scheduler itself — the cli watches the same flag and owns the shutdown.
//! `/candyriya reload` runs through the [`ReloadFn`](crate::ReloadFn) the
//! composition root wires in; the command layer only parses, gates and
//! renders — never the mechanism itself. `/candyriya plugin` runs
//! through the [`PluginViewPrefix`](crate::PluginViewPrefix) the
//! composition root wires the same way; without one it answers with a
//! localized notice.

use std::sync::Arc;
use std::time::{Duration, Instant};

use azalea_brigadier::arguments::string_argument_type::{get_string, word};
use azalea_brigadier::builder::argument_builder::ArgumentBuilder;
use azalea_brigadier::builder::literal_argument_builder::literal;
use azalea_brigadier::context::CommandContext as BrigadierContext;

use candyriya_i18n::I18n;
use candyriya_utils::format_uptime;

use crate::command::{CandyCommand, argument_with_suggestions, argument_without_suggestions};
use crate::plugin::{
    PluginViewPrefix, ReceiptVerb, handle_plugin_info, handle_plugin_list, handle_plugin_receipt,
    handle_plugin_tree, handle_plugin_unload,
};
use crate::source::{CommandSource, SourceKind};
use crate::state::CommandState;
use crate::text::escape_minimessage;

use super::common::{out, switch_to};
use super::reload::handle_reload;
use super::suggest::{PlayerNames, ServerNames};
use crate::effects::{ReloadFn, ReloadScope};
use crate::ps::render_ps;
use crate::snapshot::{BackendSnapshot, BackendState};
use crate::tree::render_tree;

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

fn handle_ps(state: &CommandState, source: &CommandSource, full: bool) -> i32 {
    let t = state.locales.pick(source);
    let snapshot = state.directory.ps_snapshot();
    for line in render_ps(&snapshot, full, Instant::now(), t) {
        out(state, source, line);
    }
    1
}

/// `/candyriya ps --watch [--full]` — a console-only live view.
///
/// The handler cannot repaint the screen itself (the REPL owns stdin and
/// the terminal), so it records the request in the shared latch and
/// returns; the REPL takes the latch after dispatch and paints. Non-console
/// sources get one ordinary ps frame instead — the latch is console-only
/// by construction.
fn handle_ps_watch(state: &CommandState, source: &CommandSource, full: bool) -> i32 {
    if source.source_kind() != SourceKind::Console {
        return handle_ps(state, source, full);
    }
    state.watch.request(full);
    1
}

fn handle_tree(state: &CommandState, source: &CommandSource) -> i32 {
    let t = state.locales.pick(source);
    let snapshot = state.directory.ps_snapshot();
    for line in render_tree(&snapshot, t) {
        out(state, source, line);
    }
    1
}

/// `/candyriya` — the proxy admin root. Subcommands carry their own gates.
pub(crate) struct CandyriyaCommand {
    state: CommandState,
    shutdown: crate::ShutdownHandle,
    reload: Option<ReloadFn>,
    plugin_view: Option<PluginViewPrefix>,
}

impl CandyriyaCommand {
    pub(crate) fn new(
        state: CommandState,
        shutdown: crate::ShutdownHandle,
        reload: Option<ReloadFn>,
        plugin_view: Option<PluginViewPrefix>,
    ) -> Self {
        Self {
            state,
            shutdown,
            reload,
            plugin_view,
        }
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
        let reload_parts = parts.clone();
        let reload_config_parts = parts.clone();
        let reload_plugins_parts = parts.clone();
        let ps_parts = parts.clone();
        let ps_full_parts = parts.clone();
        let ps_watch_parts = parts.clone();
        let ps_watch_full_parts = parts.clone();
        let ps_full_watch_parts = parts.clone();
        let tree_parts = parts.clone();
        let plugin_parts = parts.clone();
        let plugin_list_parts = parts.clone();
        let plugin_info_parts = parts.clone();
        let plugin_enable_parts = parts.clone();
        let plugin_disable_parts = parts.clone();
        let plugin_reload_parts = parts.clone();
        let plugin_unload_parts = parts.clone();
        let plugin_tree_parts = parts.clone();
        let plugin_root_view = self.plugin_view.clone();
        let plugin_list_view = self.plugin_view.clone();
        let plugin_info_view = self.plugin_view.clone();
        let plugin_enable_view = self.plugin_view.clone();
        let plugin_disable_view = self.plugin_view.clone();
        let plugin_reload_view = self.plugin_view.clone();
        let plugin_unload_view = self.plugin_view.clone();
        let plugin_tree_view = self.plugin_view.clone();
        let end_shutdown = self.shutdown.clone();
        let reload_all = self.reload.clone();
        let reload_config = self.reload.clone();
        let reload_plugins = self.reload.clone();
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
        .then(
            literal::<CommandSource>("ps")
                .requires(permission_check("candyriya.command.ps"))
                .executes(move |ctx: &BrigadierContext<CommandSource>| {
                    handle_ps(&ps_parts, &ctx.source, false)
                })
                .then(
                    literal::<CommandSource>("--full")
                        .executes(move |ctx: &BrigadierContext<CommandSource>| {
                            handle_ps(&ps_full_parts, &ctx.source, true)
                        })
                        .then(literal::<CommandSource>("--watch").executes(
                            move |ctx: &BrigadierContext<CommandSource>| {
                                handle_ps_watch(&ps_full_watch_parts, &ctx.source, true)
                            },
                        )),
                )
                .then(
                    literal::<CommandSource>("--watch")
                        .executes(move |ctx: &BrigadierContext<CommandSource>| {
                            handle_ps_watch(&ps_watch_parts, &ctx.source, false)
                        })
                        .then(literal::<CommandSource>("--full").executes(
                            move |ctx: &BrigadierContext<CommandSource>| {
                                handle_ps_watch(&ps_watch_full_parts, &ctx.source, true)
                            },
                        )),
                ),
        )
        .then(
            literal::<CommandSource>("tree")
                .requires(permission_check("candyriya.command.tree"))
                .executes(move |ctx: &BrigadierContext<CommandSource>| {
                    handle_tree(&tree_parts, &ctx.source)
                }),
        )
        .then(
            literal::<CommandSource>("reload")
                .requires(permission_check("candyriya.command.reload"))
                .executes(move |ctx: &BrigadierContext<CommandSource>| {
                    handle_reload(&reload_parts, &ctx.source, &reload_all, ReloadScope::All)
                })
                .then(literal::<CommandSource>("config").executes(
                    move |ctx: &BrigadierContext<CommandSource>| {
                        handle_reload(
                            &reload_config_parts,
                            &ctx.source,
                            &reload_config,
                            ReloadScope::Config,
                        )
                    },
                ))
                .then(literal::<CommandSource>("plugins").executes(
                    move |ctx: &BrigadierContext<CommandSource>| {
                        handle_reload(
                            &reload_plugins_parts,
                            &ctx.source,
                            &reload_plugins,
                            ReloadScope::Plugins,
                        )
                    },
                )),
        )
        .then(
            literal::<CommandSource>("plugin")
                .requires(permission_check("candyriya.command.plugin.list"))
                .executes(move |ctx: &BrigadierContext<CommandSource>| {
                    handle_plugin_list(&plugin_parts, &ctx.source, &plugin_root_view)
                })
                .then(
                    literal::<CommandSource>("list")
                        .requires(permission_check("candyriya.command.plugin.list"))
                        .executes(move |ctx: &BrigadierContext<CommandSource>| {
                            handle_plugin_list(&plugin_list_parts, &ctx.source, &plugin_list_view)
                        }),
                )
                .then(
                    literal::<CommandSource>("info")
                        .requires(permission_check("candyriya.command.plugin.info"))
                        .then(argument_without_suggestions("plugin", word()).executes(
                            move |ctx: &BrigadierContext<CommandSource>| {
                                let id = get_string(ctx, "plugin").unwrap_or_default();
                                handle_plugin_info(
                                    &plugin_info_parts,
                                    &ctx.source,
                                    &plugin_info_view,
                                    &id,
                                )
                            },
                        )),
                )
                .then(
                    literal::<CommandSource>("enable")
                        .requires(permission_check("candyriya.command.plugin.enable"))
                        .then(argument_without_suggestions("plugin", word()).executes(
                            move |ctx: &BrigadierContext<CommandSource>| {
                                let id = get_string(ctx, "plugin").unwrap_or_default();
                                handle_plugin_receipt(
                                    &plugin_enable_parts,
                                    &ctx.source,
                                    &plugin_enable_view,
                                    &id,
                                    ReceiptVerb::Enable,
                                )
                            },
                        )),
                )
                .then(
                    literal::<CommandSource>("disable")
                        .requires(permission_check("candyriya.command.plugin.disable"))
                        .then(argument_without_suggestions("plugin", word()).executes(
                            move |ctx: &BrigadierContext<CommandSource>| {
                                let id = get_string(ctx, "plugin").unwrap_or_default();
                                handle_plugin_receipt(
                                    &plugin_disable_parts,
                                    &ctx.source,
                                    &plugin_disable_view,
                                    &id,
                                    ReceiptVerb::Disable,
                                )
                            },
                        )),
                )
                .then(
                    literal::<CommandSource>("reload")
                        .requires(permission_check("candyriya.command.plugin.reload"))
                        .then(argument_without_suggestions("plugin", word()).executes(
                            move |ctx: &BrigadierContext<CommandSource>| {
                                let id = get_string(ctx, "plugin").unwrap_or_default();
                                handle_plugin_receipt(
                                    &plugin_reload_parts,
                                    &ctx.source,
                                    &plugin_reload_view,
                                    &id,
                                    ReceiptVerb::Reload,
                                )
                            },
                        )),
                )
                .then(
                    literal::<CommandSource>("unload")
                        .requires(permission_check("candyriya.command.plugin.unload"))
                        .then(argument_without_suggestions("plugin", word()).executes(
                            move |ctx: &BrigadierContext<CommandSource>| {
                                let id = get_string(ctx, "plugin").unwrap_or_default();
                                handle_plugin_unload(
                                    &plugin_unload_parts,
                                    &ctx.source,
                                    &plugin_unload_view,
                                    &id,
                                )
                            },
                        )),
                )
                .then(
                    literal::<CommandSource>("tree")
                        .requires(permission_check("candyriya.command.plugin.tree"))
                        .executes(move |ctx: &BrigadierContext<CommandSource>| {
                            handle_plugin_tree(&plugin_tree_parts, &ctx.source, &plugin_tree_view)
                        }),
                ),
        )
    }
}
