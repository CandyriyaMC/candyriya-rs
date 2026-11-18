//! The interactive console REPL.
//!
//! When stdin is a TTY, the proxy hosts a live console: `candyriya > ` is
//! printed (ANSI colors when enabled), one line is read, and the line is
//! dispatched through the built-in command tree with a console source.
//! Command output goes to stdout through the shared effects sink; command
//! errors are printed in red. `/candyriya end` flips the shared shutdown
//! flag, which the main loop observes to begin graceful shutdown.
//!
//! The console is a long-lived unit in the supervision tree (`console`).
//! Its body spawns the blocking stdin loop on the task service's blocking
//! pool (rayon) and awaits the oneshot completion signal it fires; a stop
//! requested through the tree drops the body at that await, which is what
//! a process exit requires. The blocking stdin thread itself stays as it
//! was — a rayon worker parked in read(2), detached by design.
//!
//! When stdin is not a TTY the REPL never starts: logs go to stdout and
//! stderr as usual and there is no interactive prompt (services, CI and
//! pipes must not get one). The unit then exits `Success` immediately
//! instead of parking forever.

use std::io::{BufRead, IsTerminal, LineWriter, Write, stdin, stdout};
use std::sync::Arc;

use candyriya_command::{CommandSource, Dispatcher, ShutdownHandle};
use candyriya_init::{ExitStatus, RestartPolicy, ServiceKind, UnitKind, UnitSpec};
use candyriya_scheduler::{TaskId, TaskService};

use crate::effects::CliEffects;

/// Unregisters the parked stdin thread from the kernel on body drop.
///
/// A tree stop drops the unit body at the `rx` await point; the rayon
/// worker parked on `read(2)` cannot be interrupted, so the honest move
/// is to take its registration off the books here — otherwise the final
/// `TaskService::drain` waits the full timeout for a thread that can
/// never finish, and Ctrl-C appears to hang. The thread itself stays
/// parked (by design, see the module docs) and dies with the process.
struct ReplCancelGuard(TaskService, Option<TaskId>);

impl Drop for ReplCancelGuard {
    fn drop(&mut self) {
        if let Some(id) = self.1.take() {
            let _ = self.0.cancel(id);
        }
    }
}

/// Build the spec for the `console` unit.
///
/// The unit is cancel-shaped: on a tree stop it ends at the oneshot
/// await between the blocking stdin loop and the unit body. The watch it
/// flips on `/candyriya end` is the bridge the accept loop and the main
/// loop still wait on; the tree-wide cancel the composition root fires
/// alongside it is what stops this unit.
///
/// `shutdown_tx` is flipped instead of [`ShutdownHandle::trigger`] being
/// the only signal: both paths land in the same `watch` the accept loop
/// and the main loop wait on. When stdin is not a terminal the unit body
/// is a no-op that exits `Success` and logs why.
// ipc-migration: Init.Shutdown — `/candyriya end` should stop the tree,
// not just flip this bridge; the watch dies with the accept half of it in phase 3
pub fn console_spec(
    service: TaskService,
    dispatcher: Arc<Dispatcher>,
    effects: Arc<CliEffects>,
    shutdown_handle: ShutdownHandle,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    use_ansi: bool,
) -> UnitSpec {
    UnitSpec::new(
        "console",
        UnitKind::LongLived,
        RestartPolicy::Never,
        move || {
            let service = service.clone();
            let dispatcher = Arc::clone(&dispatcher);
            let effects = Arc::clone(&effects);
            let shutdown_handle = shutdown_handle.clone();
            let shutdown_tx = shutdown_tx.clone();
            async move {
                if !stdin().is_terminal() {
                    tracing::info!("stdin is not a terminal; interactive console is disabled");
                    return ExitStatus::Success;
                }
                // the unit awaits the blocking loop's completion signal; a
                // tree stop drops the body at this await (Cancelled) and
                // leaves the parallel stdin thread parked, which is what a
                // process exit needs
                let (tx, rx) = tokio::sync::oneshot::channel::<()>();
                let repl_id = service.spawn_blocking("console.repl", move || {
                    run_loop(
                        &dispatcher,
                        &effects,
                        &shutdown_handle,
                        &shutdown_tx,
                        use_ansi,
                    );
                    let _ = tx.send(());
                });
                let _guard = ReplCancelGuard(service.clone(), Some(repl_id));
                let _ = rx.await;
                ExitStatus::Success
            }
        },
    )
    .with_service_kind(ServiceKind::Console)
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
            // ipc-migration: Init.Shutdown — same bridge flip as pressing Ctrl-C; see console_spec
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
        "candyriya" => "/candyriya <info|servers|switch|end|reload|ps|tree>",
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
            Some("/candyriya <info|servers|switch|end|reload|ps|tree>")
        );
        assert_eq!(usage_hint("notacommand"), None);
        assert_eq!(usage_hint("   "), None);
    }
}
