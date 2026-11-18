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
//!
//! `/candyriya ps --watch` is special: the command handler only records
//! the request in the shared [`PsWatchHandle`] latch and returns, and
//! this loop (still on the blocking console thread) takes it after
//! dispatch and enters watch mode. The watch repaints the ordinary
//! `ps` frame once per second — the exact same renderer the command
//! uses — until Enter (any line ends the watch), the shared shutdown
//! flag flips (Ctrl-C) or stdin gives up; one final ordinary frame is
//! printed on exit. With `NO_COLOR`/plain output the frames are
//! newline-separated and carry no escape bytes; on an ANSI terminal
//! each repaint homes the cursor and erases below the frame.

use std::io::{BufRead, IsTerminal, LineWriter, Write, stdin, stdout};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use candyriya_command::{
    CommandSource, Dispatcher, Locales, NetworkDirectory, PsSnapshot, PsWatchHandle, ShutdownHandle,
};
use candyriya_i18n::I18n;
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

/// Everything the console unit is built from.
///
/// One bundle instead of a dozen parameters; the composition root fills it
/// once and the REPL reads from it. Cloned per spawn.
#[derive(Clone)]
pub struct ConsoleWiring {
    /// The task service the blocking stdin loop (and the watch's parked
    /// reader) are spawned on.
    pub service: TaskService,
    /// The shared flag `/candyriya end` flips; the REPL and the watch
    /// observe it (Ctrl-C flips it too, via the signal arm in the main
    /// loop).
    pub shutdown_handle: ShutdownHandle,
    /// The bridge the accept loop and the main loop wait on.
    pub shutdown_tx: tokio::sync::watch::Sender<bool>,
    /// Whether to emit ANSI SGR sequences.
    pub use_ansi: bool,
    /// The live tree view, taken once per watch frame.
    pub directory: Arc<dyn NetworkDirectory>,
    /// Locales bound to the console.
    pub locales: Locales,
    /// The latch `/candyriya ps --watch` drops its request in; the REPL
    /// takes it after dispatch.
    pub watch: PsWatchHandle,
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
// ipc-migration: Init.Shutdown - `/candyriya end` should stop the tree,
// not just flip this bridge; the watch dies with the accept half of it in phase 3
pub fn console_spec(
    dispatcher: Arc<Dispatcher>,
    effects: Arc<CliEffects>,
    wiring: ConsoleWiring,
) -> UnitSpec {
    UnitSpec::new(
        "console",
        UnitKind::LongLived,
        RestartPolicy::Never,
        move || {
            let dispatcher = Arc::clone(&dispatcher);
            let effects = Arc::clone(&effects);
            let wiring = wiring.clone();
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
                let block = wiring.clone();
                let repl_id = wiring.service.spawn_blocking("console.repl", move || {
                    run_loop(&dispatcher, &effects, &block);
                    let _ = tx.send(());
                });
                let _guard = ReplCancelGuard(wiring.service.clone(), Some(repl_id));
                let _ = rx.await;
                ExitStatus::Success
            }
        },
    )
    .with_service_kind(ServiceKind::Console)
}

fn run_loop(dispatcher: &Dispatcher, effects: &CliEffects, wiring: &ConsoleWiring) {
    let use_ansi = wiring.use_ansi;
    let shutdown_handle = &wiring.shutdown_handle;
    let shutdown_tx = &wiring.shutdown_tx;
    let mut out = LineWriter::new(stdout());

    prompt(&mut out, use_ansi);
    // one line per stdin lock: the watch parks a blocking reader on
    // stdin() while it repaints, so the outer loop must never hold the
    // lock across a dispatch
    loop {
        let line = {
            let stdin = stdin();
            let lock = stdin.lock();
            match lock.lines().next() {
                Some(result) => result,
                None => break,
            }
        };
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
        if let Some(full) = wiring.watch.take() {
            let result = watch_mode(
                use_ansi,
                full,
                &wiring.service,
                &wiring.directory,
                &wiring.locales,
                &wiring.shutdown_handle,
            );
            if let Err(error) = result {
                // a paint failure must not kill the console; drop back to
                // the prompt and let the operator decide
                tracing::error!(%error, "ps --watch stopped after a write failure");
            }
            // entering watch may have ended via the shared shutdown flag
            // (Ctrl-C during the repaint); propagate it before prompting
            if wiring.shutdown_handle.is_requested() {
                let _ = wiring.shutdown_tx.send(true);
                break;
            }
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

/// How long one watch frame stays on screen before the next repaint.
const WATCH_INTERVAL: Duration = Duration::from_secs(1);

/// What the parked stdin reader (or a scripted test) reports to the
/// watch loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WatchSignal {
    /// A line was read. Any line ends the watch; its content is never
    /// looked at.
    Line,
    /// The repaint budget elapsed with no line.
    Tick,
    /// The reader finished without a line (stdin hit EOF or errored).
    Closed,
}

/// The ANSI sequence printed once, at watch entry: clear the screen and
/// home the cursor, so the first frame paints over a known-empty canvas.
fn watch_entry(use_ansi: bool) -> &'static str {
    if use_ansi { "\x1b[2J\x1b[H" } else { "" }
}

/// The prefix printed before every frame: home the cursor, without
/// clearing. Erase-below (see [`watch_erase_below`]) wipes any tail left
/// by a previous, taller frame, which is all that is needed to make
/// repaints look in-place.
fn watch_home(use_ansi: bool) -> &'static str {
    if use_ansi { "\x1b[H" } else { "" }
}

/// The suffix printed after every frame: erase everything below the
/// cursor.
fn watch_erase_below(use_ansi: bool) -> &'static str {
    if use_ansi { "\x1b[J" } else { "" }
}

/// One frame block: home (ANSI only) + the rendered table lines + erase
/// below (ANSI only) or a blank separator line (plain). Plain frames never
/// contain an escape byte — they are MiniMessage-stripped lines plus
/// newlines, one frame per block.
fn frame_block(lines: &[String], use_ansi: bool) -> String {
    let mut out = String::with_capacity(1024);
    out.push_str(watch_home(use_ansi));
    for line in lines {
        out.push_str(&crate::ansi::render(line, use_ansi));
        out.push('\n');
    }
    out.push_str(watch_erase_below(use_ansi));
    if !use_ansi {
        out.push('\n');
    }
    out
}

/// The watch body: paint the table once, then repaint every
/// [`WATCH_INTERVAL`] until Enter (any line), the shared shutdown flag
/// (Ctrl-C) or the reader giving up — then paint one final ordinary
/// frame so the screen ends with a full table.
///
/// The loop is deliberately dumb about where the snapshot and the
/// line-arrival signal come from: production wires the live tree and the
/// parked stdin reader, tests script both. Nothing here sleeps — the
/// pacing lives in the caller's `next` closure.
fn watch_run(
    full: bool,
    use_ansi: bool,
    out: &mut impl Write,
    t: &I18n,
    mut snapshot: impl FnMut() -> PsSnapshot,
    mut next: impl FnMut() -> WatchSignal,
    is_requested: impl Fn() -> bool,
) -> std::io::Result<()> {
    let entry = watch_entry(use_ansi);
    if !entry.is_empty() {
        out.write_all(entry.as_bytes())?;
    }
    loop {
        if is_requested() {
            break;
        }
        let lines = candyriya_command::render_ps(&snapshot(), full, Instant::now(), t);
        out.write_all(frame_block(&lines, use_ansi).as_bytes())?;
        out.flush()?;
        match next() {
            WatchSignal::Tick => {}
            WatchSignal::Line | WatchSignal::Closed => break,
        }
    }
    // one final, ordinary frame so the screen ends with the full table
    let lines = candyriya_command::render_ps(&snapshot(), full, Instant::now(), t);
    out.write_all(frame_block(&lines, use_ansi).as_bytes())?;
    out.flush()?;
    Ok(())
}

/// Enter watch mode until the user hits Enter or Ctrl-C.
///
/// Parks exactly one blocking reader on stdin (a rayon worker, like the
/// REPL's own stdin thread) and repaints while it waits. The reader owns
/// the line lock for the whole watch: keys typed into the terminal go
/// through the line discipline, and Enter submits the line that ends the
/// watch. If the watch exits via the shutdown flag instead, the reader
/// stays parked on read(2) — the same detached-by-design pattern as the
/// REPL's own blocking thread; the process is on its way out at that
/// point anyway.
fn watch_mode(
    use_ansi: bool,
    full: bool,
    service: &TaskService,
    directory: &Arc<dyn NetworkDirectory>,
    locales: &Locales,
    shutdown_handle: &ShutdownHandle,
) -> std::io::Result<()> {
    let (tx, rx) = mpsc::channel::<WatchSignal>();
    let _read_id = service.spawn_blocking("console.ps-watch-stdin", move || {
        let signal = match stdin().lock().lines().next() {
            Some(Ok(_)) | Some(Err(_)) => WatchSignal::Line,
            None => WatchSignal::Closed,
        };
        let _ = tx.send(signal);
    });
    let mut next = move || match rx.recv_timeout(WATCH_INTERVAL) {
        Ok(signal) => signal,
        Err(mpsc::RecvTimeoutError::Timeout) => WatchSignal::Tick,
        // the reader died without sending: leave watch rather than spin
        Err(mpsc::RecvTimeoutError::Disconnected) => WatchSignal::Closed,
    };
    let mut out = stdout();
    watch_run(
        full,
        use_ansi,
        &mut out,
        locales.console(),
        || directory.ps_snapshot(),
        &mut next,
        || shutdown_handle.is_requested(),
    )
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
    use super::*;

    use candyriya_command::{PsConn, PsUnit, PsUnitKind, PsUnitState};

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

    fn en() -> I18n {
        I18n::new("en").expect("embedded en locale")
    }

    fn unit(pid: u64, name: &str, kind: PsUnitKind) -> PsUnit {
        PsUnit {
            pid,
            ppid: 1,
            name: name.to_owned(),
            kind,
            state: PsUnitState::Running,
            parked: false,
            started_at: Instant::now() - Duration::from_secs(60),
            restart_count: 0,
            last_exit: None,
            exited_at: None,
        }
    }

    fn snapshot() -> PsSnapshot {
        let conns = vec![PsConn {
            pid: 5,
            bytes_rx: 1024,
            bytes_tx: 2048,
            connected_at: Instant::now() - Duration::from_secs(60),
            last_activity: Instant::now(),
        }];
        PsSnapshot {
            units: vec![
                PsUnit {
                    pid: 1,
                    ppid: 0,
                    name: "init".to_owned(),
                    kind: PsUnitKind::LongLived,
                    state: PsUnitState::Running,
                    parked: false,
                    started_at: Instant::now() - Duration::from_secs(3600),
                    restart_count: 0,
                    last_exit: None,
                    exited_at: None,
                },
                unit(5, "conn-1", PsUnitKind::Connection),
                unit(6, "conn-2", PsUnitKind::Connection),
            ],
            conns,
            zombies: 0,
            stuck_zombies: 0,
        }
    }

    /// Scripted `next` for [`watch_run`]: yields the given signals in
    /// order, then panics if the loop keeps asking (a watch that would
    /// never stop in production).
    fn scripted(signals: Vec<WatchSignal>) -> impl FnMut() -> WatchSignal {
        let mut signals = signals.into_iter();
        move || {
            signals
                .next()
                .expect("watch asked for another frame decision than scripted")
        }
    }

    fn run_watch(
        full: bool,
        use_ansi: bool,
        scripts: Vec<WatchSignal>,
        is_requested: impl Fn() -> bool,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        watch_run(
            full,
            use_ansi,
            &mut out,
            &en(),
            snapshot,
            scripted(scripts),
            is_requested,
        )
        .expect("writing to a Vec cannot fail");
        out
    }

    #[test]
    fn aggregate_view_collapses_connections() {
        let out = run_watch(false, false, vec![WatchSignal::Line], || false);
        let text = String::from_utf8(out).expect("plain output is utf-8");
        assert!(text.contains("conn"), "aggregate row in {text}");
        assert!(!text.contains("conn-1"), "aggregate hides rows in {text}");
        assert!(!text.contains("conn-2"), "aggregate hides rows in {text}");
    }

    #[test]
    fn full_view_expands_connections() {
        let out = run_watch(true, false, vec![WatchSignal::Line], || false);
        let text = String::from_utf8(out).expect("plain output is utf-8");
        assert!(text.contains("conn-1"), "full view lists rows in {text}");
        assert!(text.contains("conn-2"), "full view lists rows in {text}");
    }

    #[test]
    fn plain_mode_prints_newline_separated_frames_without_escapes() {
        let out = run_watch(
            false,
            false,
            vec![WatchSignal::Tick, WatchSignal::Line],
            || false,
        );
        assert!(!out.contains(&0x1b), "no escape byte may leak: {out:?}");
        let text = String::from_utf8(out).expect("plain output is utf-8");
        // three frames: one per Tick, plus the final one after Enter
        let frames = text.matches("LAST EXIT").count();
        assert_eq!(
            frames, 3,
            "one repaint per tick plus the final frame: {text}"
        );
        assert!(
            text.contains("\n\n"),
            "frames are separated by a blank line: {text}"
        );
    }

    #[test]
    fn ansi_mode_clears_once_then_homes_each_frame() {
        let out = run_watch(
            false,
            true,
            vec![WatchSignal::Tick, WatchSignal::Line],
            || false,
        );
        let text = String::from_utf8(out).expect("ansi output is utf-8");
        assert!(
            text.starts_with("\x1b[2J\x1b[H"),
            "entry clears then homes: {text:?}"
        );
        let homes = text.matches("\x1b[H").count();
        let clears = text.matches("\x1b[2J").count();
        assert_eq!(clears, 1, "clear happens exactly once, at entry: {text:?}");
        // home appears once per frame (entry home counted in the prefix) + one per repaint + final
        assert_eq!(homes, 1 + 2 + 1, "one home per frame: {text:?}");
        assert!(
            text.contains("\x1b[J"),
            "erase-below keeps repaints in place: {text:?}"
        );
    }

    #[test]
    fn enter_exits_and_prints_one_final_frame() {
        let out = run_watch(false, false, vec![WatchSignal::Line], || false);
        let text = String::from_utf8(out).expect("plain output is utf-8");
        let frames = text.matches("LAST EXIT").count();
        assert_eq!(frames, 2, "one repaint plus the final frame: {text}");
        assert!(
            text.trim_end()
                .ends_with("R=running F=frozen Z=zombie t=restarting !=failed-terminal")
        );
    }

    #[test]
    fn shutdown_flag_exits_before_the_first_repaint() {
        let out = run_watch(false, false, vec![], || true);
        let text = String::from_utf8(out).expect("plain output is utf-8");
        let frames = text.matches("LAST EXIT").count();
        assert_eq!(
            frames, 1,
            "only the final frame after an immediate stop: {text}"
        );
    }

    #[test]
    fn tick_then_shutdown_prints_repaint_and_final_frame() {
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let out = run_watch(false, false, vec![WatchSignal::Tick], || {
            // first check (top of the loop) says keep going, second says stop
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= 1
        });
        let text = String::from_utf8(out).expect("plain output is utf-8");
        let frames = text.matches("LAST EXIT").count();
        assert_eq!(frames, 2, "one repaint plus the final frame: {text}");
    }
}
