//! `/glist` — the player-visible player list across the whole network.
//!
//! Shows the network-wide online count followed by one line per configured
//! backend, in `try`-chain order. The per-backend lines carry a **priority
//! color**: priority 0 (first `try` target) renders green, priority 1 renders
//! gold, everything else renders gray. The report itself is [`GlistReport`] —
//! built by [`GlistReport::from_directory`] and rendered by
//! [`GlistReport::render`].

use azalea_brigadier::builder::argument_builder::ArgumentBuilder;

use crate::command::CandyCommand;
use crate::source::CommandSource;
use crate::state::{CommandState, NetworkDirectory};
use crate::text::escape_minimessage;

/// One backend's online count, with its `try`-chain priority.
///
/// Priority drives the per-backend color in [`GlistReport::render`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlistServerEntry {
    /// The configured server name.
    pub name: String,
    /// Players currently on this backend.
    pub count: usize,
    /// Index into the `try` chain (0 = first). Rendered colors: 0 → green,
    /// 1 → gold, 2+ → gray.
    pub priority: usize,
}

/// Online players across the network: a network-wide total plus one entry
/// per configured backend.
///
/// One shared implementation builds and renders the report; the command is a
/// thin shell around it. There is intentionally exactly one output shape for
/// the player list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlistReport {
    /// Players online across the whole network (including players who are
    /// between servers).
    pub total: usize,
    /// One entry per configured server, in rendered (`try`) order.
    pub servers: Vec<GlistServerEntry>,
}

impl GlistReport {
    /// Builds the report from a directory snapshot.
    ///
    /// Servers are sorted by their `try` priority before counting, so the
    /// rendered output is stable regardless of the directory's own order.
    /// Players whose `server` is `None` count towards [`Self::total`] only.
    pub fn from_directory(directory: &dyn NetworkDirectory) -> Self {
        let players = directory.online_players();
        let mut servers = directory.servers();
        servers.sort_by_key(|server| server.priority);
        let entries = servers
            .into_iter()
            .map(|server| GlistServerEntry {
                count: players
                    .iter()
                    .filter(|player| player.server.as_deref() == Some(server.name.as_str()))
                    .count(),
                name: server.name,
                priority: server.priority,
            })
            .collect();
        Self {
            total: players.len(),
            servers: entries,
        }
    }

    /// Renders the report as a sequence of MiniMessage lines for the locale.
    ///
    /// Always emits the total line first. On an empty network a
    /// `candyriya.glist.empty` line follows it; otherwise each configured
    /// backend gets its own priority-colored line (green first, gold second,
    /// gray after that).
    pub fn render(&self, i18n: &candyriya_i18n::I18n) -> Vec<String> {
        let total = self.total.to_string();
        let mut lines = vec![i18n.t("candyriya.glist.total", &[("count", total.as_str())])];
        if self.total == 0 {
            lines.push(i18n.t("candyriya.glist.empty", &[]));
            return lines;
        }
        for server in &self.servers {
            let count = server.count.to_string();
            lines.push(i18n.t(
                priority_line_key(server.priority),
                &[
                    ("server", escape_minimessage(&server.name).as_str()),
                    ("count", count.as_str()),
                ],
            ));
        }
        lines
    }
}

/// Maps a `try`-chain priority to the i18n key for that backend's line.
/// Colors live inside the key values, not here, so every embedded locale
/// value stays a parseable MiniMessage string.
fn priority_line_key(priority: usize) -> &'static str {
    match priority {
        0 => "candyriya.glist.backend.first",
        1 => "candyriya.glist.backend.second",
        _ => "candyriya.glist.backend.rest",
    }
}

/// The `/glist` command. Open to everyone: listing the network is the
/// player-facing counterpart of the tab list.
pub struct GlistCommand {
    state: CommandState,
}

impl GlistCommand {
    /// Creates the command with the given shared state.
    pub fn new(state: CommandState) -> Self {
        Self { state }
    }
}

impl CandyCommand for GlistCommand {
    fn name(&self) -> &str {
        "glist"
    }

    fn build(&self, lit: ArgumentBuilder<CommandSource>) -> ArgumentBuilder<CommandSource> {
        let state = self.state.clone();
        lit.executes(move |ctx| {
            let source = &ctx.source;
            let report = GlistReport::from_directory(&*state.directory);
            let lines = report.render(state.locales.pick(source));
            for line in lines {
                state.effects.send(source, &line);
            }
            1
        })
    }
}
