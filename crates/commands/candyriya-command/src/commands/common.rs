//! Shared handlers behind the switch commands.
//!
//! `/candyriya switch`, `/server`, `/goto` and single-target `/send` are the
//! same code path with different entry points and permission gates: resolve
//! the server (exactly as spelled first, then case-insensitively), look the
//! target up in the online roster, ask the effects sink to move the player
//! and render the outcome. `/send ALL` fans out through the same path, once
//! per online player.

use candyriya_i18n::I18n;

use crate::source::{CommandSource, PlayerRef};
use crate::state::{CommandState, NetworkDirectory};
use crate::text::escape_minimessage;

/// Emit one line to `source`. Thin wrapper kept so handlers read as
/// `out(state, source, line)` without spelling the trait every time.
pub(crate) fn out(state: &CommandState, source: &CommandSource, line: String) {
    state.effects.send(source, &line);
}

/// The configured server spelling for `name` — exact match first, then
/// case-insensitive. Server names are configuration-controlled and
/// suggestions hand out the canonical spelling; this is just tolerant.
pub(crate) fn resolve_server(directory: &dyn NetworkDirectory, name: &str) -> Option<String> {
    let servers = directory.servers();
    servers
        .iter()
        .find(|server| server.name == name)
        .or_else(|| {
            servers
                .iter()
                .find(|server| server.name.eq_ignore_ascii_case(name))
        })
        .map(|server| server.name.clone())
}

/// The configured servers, comma-separated, for the unknown-server line.
pub(crate) fn known_servers(directory: &dyn NetworkDirectory) -> String {
    let mut names: Vec<String> = directory
        .servers()
        .into_iter()
        .map(|server| server.name)
        .collect();
    names.sort();
    names.dedup();
    names.join(", ")
}

/// The i18n'd unknown-server line, with the known-servers list baked in.
pub(crate) fn unknown_server_line(
    t: &I18n,
    directory: &dyn NetworkDirectory,
    name: &str,
) -> String {
    t.t(
        "candyriya.command.server_unknown",
        &[
            ("name", escape_minimessage(name).as_str()),
            (
                "known",
                escape_minimessage(&known_servers(directory)).as_str(),
            ),
        ],
    )
}

/// Render the outcome of a switch as the line plus the invoker's exit code.
///
/// `1` when the switch was applied, `0` otherwise — brigadier-style, so
/// tests and callers can branch on the outcome.
pub(crate) fn switch_result_line(
    t: &I18n,
    directory: &dyn NetworkDirectory,
    result: crate::SwitchResult,
) -> (String, i32) {
    use crate::SwitchResult;
    match result {
        SwitchResult::Switched { player, server } => (
            t.t(
                "candyriya.route.switched",
                &[
                    ("player", escape_minimessage(&player).as_str()),
                    ("server", escape_minimessage(&server).as_str()),
                ],
            ),
            1,
        ),
        SwitchResult::PlayerNotFound { player } => (
            t.t(
                "candyriya.command.player_offline",
                &[("name", escape_minimessage(&player).as_str())],
            ),
            0,
        ),
        SwitchResult::ServerNotFound { server } => (
            // resolved before dispatch, so this is the proxy reporting the
            // server vanished mid-flight; render it like an unknown server
            unknown_server_line(t, directory, &server),
            0,
        ),
        SwitchResult::Failed {
            player,
            server,
            reason,
        } => (
            t.t(
                "candyriya.command.switch_failed",
                &[
                    ("player", escape_minimessage(&player).as_str()),
                    ("server", escape_minimessage(&server).as_str()),
                    ("reason", escape_minimessage(&reason).as_str()),
                ],
            ),
            0,
        ),
    }
}

/// Move one player (by name, case-insensitive) to `server`, rendering the
/// outcome to the invoker. Shared by `/candyriya switch`, `/server`,
/// `/goto` and single-target `/send`.
pub(crate) fn switch_to(
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
    let players = state.directory.online_players();
    let Some(player) = players
        .iter()
        .find(|player| player.name.eq_ignore_ascii_case(target))
    else {
        out(
            state,
            source,
            t.t(
                "candyriya.command.player_offline",
                &[("name", escape_minimessage(target).as_str())],
            ),
        );
        return 0;
    };
    let request =
        crate::SwitchRequest::new(PlayerRef::new(player.uuid, player.name.clone()), resolved);
    let result = state.effects.switch(request);
    let (line, code) = switch_result_line(t, &*state.directory, result);
    out(state, source, line);
    code
}
