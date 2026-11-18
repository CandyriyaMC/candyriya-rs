//! Console implementations of the command-layer side effects.
//!
//! [`CliEffects`] is the single sink for command output and cross-cutting
//! actions when the invoker is the local console. Output is rendered from
//! MiniMessage to ANSI (or plain text) and written to stdout with a trailing
//! newline. Player switching is reported truthfully: the live play-state
//! roster is consulted, so a switch reports `PlayerNotFound` only when the
//! player is genuinely absent, and an honest not-implemented failure
//! otherwise — never a claimed-but-unrouted move.

use std::io::{LineWriter, Stdout, Write, stdout};
use std::sync::{Arc, Mutex};

use candyriya_command::{CommandSource, OutputSender, ProxyEffects, SwitchRequest, SwitchResult};
use candyriya_proxy::ProxyState;

use crate::ansi;

/// Output and effects sink for the local console.
pub struct CliEffects {
    /// Console writer; shared with the REPL for line-granular interleave.
    out: Mutex<LineWriter<Stdout>>,
    /// Whether to emit ANSI SGR sequences.
    use_ansi: bool,
    /// The shared proxy state, read for the live session roster.
    state: Arc<ProxyState>,
}

impl std::fmt::Debug for CliEffects {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CliEffects")
            .field("use_ansi", &self.use_ansi)
            .finish_non_exhaustive()
    }
}

impl CliEffects {
    /// Create the sink; `use_ansi` should already honor `NO_COLOR` and TTY detection.
    ///
    /// `state` is the shared proxy state, used to consult the live session
    /// roster when a switch is requested.
    #[must_use]
    pub fn new(use_ansi: bool, state: Arc<ProxyState>) -> Self {
        Self {
            out: Mutex::new(LineWriter::new(stdout())),
            use_ansi,
            state,
        }
    }

    /// Render and write one MiniMessage line to stdout.
    ///
    /// Used by the REPL for its error lines as well, so every console line
    /// goes through the same renderer.
    pub fn line(&self, line: &str) {
        let rendered = ansi::render(line, self.use_ansi);
        if let Ok(mut out) = self.out.lock() {
            let _ = writeln!(out, "{rendered}");
            let _ = out.flush();
        }
    }
}

impl OutputSender for CliEffects {
    fn send(&self, _source: &CommandSource, line: &str) {
        self.line(line);
    }
}

impl ProxyEffects for CliEffects {
    fn switch(&self, request: SwitchRequest) -> SwitchResult {
        // the roster is authoritative for the live population; anyone in it
        // is online by definition (match on uuid, fall back to a
        // case-insensitive name, like the command layer does)
        let online = self.state.roster.snapshot().into_iter().any(|entry| {
            entry.uuid == request.player.uuid
                || entry.name.eq_ignore_ascii_case(&request.player.name)
        });
        if !online {
            tracing::warn!(
                player = %request.player.name,
                server = %request.server,
                "player switch requested from the console for a player who is not in the live roster"
            );
            return SwitchResult::PlayerNotFound {
                player: request.player.name,
            };
        }
        tracing::warn!(
            player = %request.player.name,
            server = %request.server,
            "player switch requested from the console, but switching is not implemented yet"
        );
        // the player is genuinely online; the truthful answer is that the
        // move is not implemented, never a claimed success and never a
        // bogus "not connected"
        SwitchResult::Failed {
            player: request.player.name,
            server: request.server,
            reason: "player switching is not implemented yet".to_string(),
        }
    }
}
