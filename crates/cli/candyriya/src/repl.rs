//! The interactive console REPL.
//!
//! When stdin is a TTY, the proxy hosts a live console: `candyriya > ` is
//! printed (ANSI colors when enabled), one line is read, and the line is
//! dispatched through the built-in command tree with a console source.
//! Command output goes to stdout through the shared effects sink; command
//! errors are printed in red. `/candyriya end` flips the shared shutdown
//! flag, which the main loop observes to begin graceful shutdown.
//!
//! When stdin is not a TTY the REPL never starts: logs go to stdout and
//! stderr as usual and there is no interactive prompt (services, CI and
//! pipes must not get one). The read loop is a blocking stdin read, so it
//! runs on the task service's blocking pool — cancellation flips the
//! wrapper's token and the loop is no longer tracked, which is what a
//! process exiting needs.

use std::io::{BufRead, IsTerminal, LineWriter, Write, stdin, stdout};
use std::sync::Arc;

use candyriya_command::{CommandSource, Dispatcher, ShutdownHandle};
use candyriya_scheduler::{TaskId, TaskService};

use crate::effects::CliEffects;

/// Spawn the interactive console on `service`, if this process has a TTY.
///
/// `shutdown_tx` is flipped instead of [`ShutdownHandle::trigger`] being the
/// only signal: both paths land in the same `watch` the accept loop and the
/// main loop wait on. Returns the spawned task id, or `None` when the REPL
/// was not started (stdin is not a terminal).
pub fn spawn_if_interactive(
    service: &TaskService,
    dispatcher: Arc<Dispatcher>,
    effects: Arc<CliEffects>,
    shutdown_handle: ShutdownHandle,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    use_ansi: bool,
) -> Option<TaskId> {
    if !stdin().is_terminal() {
        tracing::info!("stdin is not a terminal; interactive console is disabled");
        return None;
    }
    let id = service.spawn_blocking("console.repl", move || {
        run_loop(
            &dispatcher,
            &effects,
            &shutdown_handle,
            &shutdown_tx,
            use_ansi,
        );
    });
    Some(id)
}

fn run_loop(
    dispatcher: &Dispatcher,
    effects: &CliEffects,
    shutdown_handle: &ShutdownHandle,
    shutdown_tx: &tokio::sync::watch::Sender<bool>,
    use_ansi: bool,
) {
    let stdin = stdin();
    let mut out = LineWriter::new(stdout());

    prompt(&mut out, use_ansi);
    for line in stdin.lock().lines() {
        match line {
            Ok(line) => {
                let line = line.trim_end_matches(['\r', '\n']);
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    run_command(dispatcher, effects, trimmed, use_ansi);
                }
            }
            Err(error) => {
                tracing::error!(%error, "failed to read from stdin; closing the console");
                break;
            }
        }
        if shutdown_handle.is_requested() {
            // /candyriya end was issued; wake the accept loop and the main loop
            let _ = shutdown_tx.send(true);
            break;
        }
        prompt(&mut out, use_ansi);
    }
}

fn run_command(dispatcher: &Dispatcher, effects: &CliEffects, line: &str, use_ansi: bool) {
    match dispatcher.execute(line, CommandSource::console()) {
        Ok(_) => {}
        Err(error) => {
            let plain = format!("command failed: {error}");
            if use_ansi {
                effects.line(&format!("<red>{}</red>", escape_angle_brackets(&plain)));
            } else {
                effects.line(&escape_angle_brackets(&plain));
            }
            // brigadier's raw complaint is a byte offset, not guidance; a
            // known built-in with a missing argument gets a one-line hint
            if let Some(hint) = usage_hint(line) {
                if use_ansi {
                    effects.line(&format!(
                        "<gray>Try: {}</gray>",
                        escape_angle_brackets(hint)
                    ));
                } else {
                    effects.line(&format!("Try: {hint}"));
                }
            }
        }
    }
}

/// One-line usage hint for a built-in command, or `None` when the first
/// token is not one we know. Brigadier rejects a bare `/send` with
/// "Unknown command at position 4" — correct, but useless for a human
/// staring at the console; the hint is what they actually needed.
fn usage_hint(line: &str) -> Option<&'static str> {
    let first = line
        .trim()
        .trim_start_matches('/')
        .split_whitespace()
        .next()?;
    Some(match first {
        "send" => "/send <target|ALL> <server>",
        "server" => "/server <server>",
        "goto" => "/goto <server>",
        "glist" => "/glist",
        "candyriya" => "/candyriya <info|servers|switch|end|reload>",
        _ => return None,
    })
}

/// Red lines use the MiniMessage path too; `<` `>` from an error message
/// must not become tags.
fn escape_angle_brackets(text: &str) -> String {
    candyriya_command::escape_minimessage(text)
}

fn prompt(out: &mut LineWriter<std::io::Stdout>, use_ansi: bool) {
    if use_ansi {
        let _ = write!(out, "\x1b[1;36mcandyriya > \x1b[0m");
    } else {
        let _ = write!(out, "candyriya > ");
    }
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::usage_hint;

    #[test]
    fn usage_hint_matches_built_ins_by_first_token() {
        assert_eq!(usage_hint("send"), Some("/send <target|ALL> <server>"));
        assert_eq!(usage_hint("/send"), Some("/send <target|ALL> <server>"));
        assert_eq!(usage_hint("goto nether"), Some("/goto <server>"));
        assert_eq!(usage_hint("server"), Some("/server <server>"));
        assert_eq!(usage_hint("glist"), Some("/glist"));
        assert_eq!(
            usage_hint("  /candyriya  "),
            Some("/candyriya <info|servers|switch|end|reload>")
        );
        assert_eq!(usage_hint("notacommand"), None);
        assert_eq!(usage_hint("   "), None);
    }
}
