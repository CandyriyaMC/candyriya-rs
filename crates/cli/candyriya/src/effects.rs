//! Console implementations of the command-layer side effects.
//!
//! [`CliEffects`] is the single sink for command output and cross-cutting
//! actions when the invoker is the local console. Output is rendered from
//! MiniMessage to ANSI (or plain text) and written to stdout with a trailing
//! newline. Player switching is reported truthfully: the proxy core has no
//! session roster in play yet, so a switch is always `PlayerNotFound`
//! rather than a claimed-but-unrouted move.

use std::io::{LineWriter, Stdout, Write, stdout};
use std::sync::Mutex;

use candyriya_command::{CommandSource, OutputSender, ProxyEffects, SwitchRequest, SwitchResult};

use crate::ansi;

/// Output and effects sink for the local console.
pub struct CliEffects {
    /// Console writer; shared with the REPL for line-granular interleave.
    out: Mutex<LineWriter<Stdout>>,
    /// Whether to emit ANSI SGR sequences.
    use_ansi: bool,
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
    #[must_use]
    pub fn new(use_ansi: bool) -> Self {
        Self {
            out: Mutex::new(LineWriter::new(stdout())),
            use_ansi,
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
        tracing::warn!(
            player = %request.player.name,
            server = %request.server,
            "player switch requested from the console, but no session roster exists in the proxy core yet"
        );
        // the truthful answer: this player is not connected (there is no
        // live roster to find them in), never a claimed success
        SwitchResult::PlayerNotFound {
            player: request.player.name,
        }
    }
}
