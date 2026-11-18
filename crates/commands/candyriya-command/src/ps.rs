//! The `/candyriya ps` and `/candyriya tree` surface: a process-table view
//! and a box-drawing tree of the supervision tree.
//!
//! Everything here is a function of a [`PsSnapshot`] taken at invocation time —
//! no tree handle, no lock, no I/O. The composition root fills the snapshot
//! from `Init::units()` / `Init::connections()` / `Init::snapshot()`; tests
//! copy plain data into the DTOs. That split is deliberate: the renderer is
//! pure data→lines, so every table, merge and truncation rule is testable
//! without a running proxy.
//!
//! Honesty rules the table follows:
//!
//! - Columns come from what the tree actually tracks: pid, ppid, name, kind,
//!   state (+ park gate), restarts, uptime and last exit. Per-connection byte
//!   counters are shown only for connection units and only because the kernel
//!   keeps them; there is no CPU or RSS column and never will be — the tree
//!   does not measure it.
//! - By default, connection units collapse to a single `conn × N` row (the
//!   htop/systemd convention) with their aggregates: count, byte sums, the
//!   worst state present, the restart sum and the oldest uptime. `--full`
//!   expands every unit to its own row.
//! - One-shot bootstrap markers are not special-cased: whatever is in the
//!   registry is a unit and gets a row. They are reaped at boot, so they
//!   never flood the table; anything visible is worth showing.
//! - The output always ends with a summary line built from the tree's own
//!   counters (`units: N (conn: M, plugins: K) · zombies: A (stuck: B)`),
//!   so the numbers on screen are the numbers the tree tracks.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::time::Instant;

use candyriya_i18n::I18n;
use candyriya_utils::{format_uptime, human_bytes, truncate_to_char_boundary};

use crate::text::escape_minimessage;

/// The kind of unit, as the ps table renders it.
///
/// A copy of the kernel read model's vocabulary kept crate-local so this
/// crate does not depend on `candyriya-init`; the conversion runs at the
/// seam in the composition root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PsUnitKind {
    /// Runs once and exits.
    Oneshot,
    /// A long-lived service.
    LongLived,
    /// A periodic job.
    Every,
    /// One network connection.
    Connection,
    /// A plugin (host) instance.
    Plugin,
}

/// Lifecycle state of a unit, as ps renders it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PsUnitState {
    /// The body is running.
    Running,
    /// Exited, not yet reaped; the exit status is on the node.
    Zombie,
    /// Waiting out a restart backoff.
    Restarting,
    /// Start limit breached; terminal and visible forever.
    FailedTerminal,
}

/// How a unit finished; mirrors the kernel exit vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PsExitStatus {
    /// Ran to completion.
    Success,
    /// Reported failure; carries the (bounded) message.
    Failure(String),
    /// The panic was contained by the scheduler.
    Panicked,
    /// Stopped through the tree.
    Cancelled,
    /// Exceeded its per-unit timeout.
    TimedOut,
}

/// One unit row for the ps table; a point-in-time snapshot, owns nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PsUnit {
    /// The unit's pid.
    pub pid: u64,
    /// The parent's pid at snapshot time; `0` for the root.
    pub ppid: u64,
    /// The unit's name.
    pub name: String,
    /// The kind of unit.
    pub kind: PsUnitKind,
    /// The lifecycle state.
    pub state: PsUnitState,
    /// Whether the park gate (freeze) is closed.
    pub parked: bool,
    /// When this run of the unit started.
    pub started_at: Instant,
    /// How many restarts this unit has performed.
    pub restart_count: u32,
    /// The status of the last exit, if any.
    pub last_exit: Option<PsExitStatus>,
    /// When the current run exited, if it has.
    pub exited_at: Option<Instant>,
}

/// Per-connection counters bound to a [`PsUnitKind::Connection`] unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PsConn {
    /// The connection unit's pid.
    pub pid: u64,
    /// Bytes pumped client → backend.
    pub bytes_rx: u64,
    /// Bytes pumped backend → client.
    pub bytes_tx: u64,
    /// When the connection was accepted.
    pub connected_at: Instant,
    /// When either direction last moved a byte.
    pub last_activity: Instant,
}

/// Everything the ps/tree surface renders, one consistent view per
/// invocation. Units come ascending by pid; connections likewise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PsSnapshot {
    /// Every unit in the registry, ascending pid order.
    pub units: Vec<PsUnit>,
    /// Counters of the connection units, ascending pid order.
    pub conns: Vec<PsConn>,
    /// Zombies currently in the registry (the tree's own gauge).
    pub zombies: u64,
    /// Zombies older than the tree's stuck threshold (the tree's own gauge).
    pub stuck_zombies: u64,
}

/// Names longer than this many bytes are truncated with an ellipsis.
const NAME_WIDTH: usize = 44;
/// Failure payloads in the LAST EXIT column are cut at this many bytes.
const EXIT_REASON_WIDTH: usize = 48;

/// The single-letter state shown by ps, htop-style.
///
/// The park gate wins over the bare `Running` state, because a frozen unit
/// being shown as plain `R` would be a lie about what the operator sees on
/// the wire; a zombie's freeze request is stale (the body is gone), so the
/// exit states keep their plain letters regardless of the flag.
pub(crate) fn state_char(state: PsUnitState, parked: bool) -> char {
    match state {
        PsUnitState::Running if parked => 'F',
        PsUnitState::Running => 'R',
        PsUnitState::Zombie => 'Z',
        PsUnitState::Restarting => 't',
        PsUnitState::FailedTerminal => '!',
    }
}

/// How bad a state is, for the aggregate row: higher wins.
fn state_severity(state: PsUnitState, parked: bool) -> u8 {
    match state_char(state, parked) {
        '!' => 5,
        'Z' => 4,
        't' => 3,
        'F' => 2,
        _ => 1,
    }
}

/// The color wrapper for a state letter, per the console palette:
/// R green, F yellow, Z dim yellow (gold), t cyan, ! bold red.
pub(crate) fn state_open(state: PsUnitState, parked: bool) -> &'static str {
    match state_char(state, parked) {
        'R' => "<green>",
        'F' => "<yellow>",
        'Z' => "<gold>",
        't' => "<aqua>",
        _ => "<red><bold>",
    }
}

/// Map an exit status to its i18n'd, styled LAST EXIT text.
pub(crate) fn exit_text(t: &I18n, status: &PsExitStatus) -> String {
    match status {
        PsExitStatus::Success => t.t("candyriya.ps.exit.success", &[]),
        PsExitStatus::Failure(message) => {
            let truncated = truncate_to_char_boundary(message, EXIT_REASON_WIDTH);
            let reason = if truncated.len() < message.len() {
                format!("{truncated}…")
            } else {
                truncated.to_owned()
            };
            t.t(
                "candyriya.ps.exit.failure",
                &[("reason", escape_minimessage(&reason).as_str())],
            )
        }
        PsExitStatus::Panicked => t.t("candyriya.ps.exit.panicked", &[]),
        PsExitStatus::Cancelled => t.t("candyriya.ps.exit.cancelled", &[]),
        PsExitStatus::TimedOut => t.t("candyriya.ps.exit.timed_out", &[]),
    }
}

fn dash(t: &I18n) -> String {
    t.t("candyriya.ps.none", &[])
}

/// Visible character count of a plain (tag-free) string.
fn chars(text: &str) -> usize {
    text.chars().count()
}

/// Pad a plain string to `width` chars, left-aligned (numbers pass `true`).
fn pad(text: &str, width: usize, right: bool) -> String {
    let padding = width.saturating_sub(chars(text));
    let fill = " ".repeat(padding);
    if right {
        format!("{fill}{text}")
    } else {
        format!("{text}{fill}")
    }
}

/// Truncate a plain string to `width` chars, ending with `…` when cut.
fn truncate_chars(text: &str, width: usize) -> String {
    if chars(text) <= width {
        return text.to_owned();
    }
    let mut out = text
        .chars()
        .take(width.saturating_sub(1))
        .collect::<String>();
    out.push('…');
    out
}

/// One assembled table row: plain cell contents plus the tag wrappers.
struct Row {
    /// Sort by this pid (the aggregate row uses the oldest conn pid).
    sort_key: u64,
    pid: String,
    ppid: String,
    /// Escaped, truncated name without tags.
    name_plain: String,
    /// Tag wrapper around the name, if any (`<aqua>…<reset>` for conns).
    name_open: &'static str,
    /// The state letter.
    state: char,
    /// The state's color wrapper.
    state_open: &'static str,
    restarts: String,
    uptime: String,
    rx: String,
    tx: String,
    /// Fully assembled LAST EXIT cell (already i18n'd and tagged).
    exit: String,
}

impl Row {
    /// The plain name with the wrapper applied — what `chars` measures.
    fn name_cell(&self, width: usize) -> String {
        let padded = pad(&self.name_plain, width, false);
        if self.name_open.is_empty() {
            padded
        } else {
            format!("{}{}<reset>", self.name_open, padded)
        }
    }

    fn state_cell(&self, width: usize) -> String {
        format!(
            "{}{}<reset>",
            self.state_open,
            pad(&self.state.to_string(), width, false)
        )
    }
}

fn conn_map(conns: &[PsConn]) -> HashMap<u64, &PsConn> {
    conns.iter().map(|conn| (conn.pid, conn)).collect()
}

fn uptime_since(now: Instant, started: Instant) -> String {
    format_uptime(now.saturating_duration_since(started))
}

fn name_cell(kind: PsUnitKind, name: &str) -> (String, &'static str) {
    let plain = escape_minimessage(&truncate_chars(name, NAME_WIDTH));
    match kind {
        PsUnitKind::Connection => (plain, "<aqua>"),
        _ => (plain, ""),
    }
}

fn unit_row(now: Instant, conns: &HashMap<u64, &PsConn>, unit: &PsUnit, t: &I18n) -> Row {
    let (rx, tx) = match conns.get(&unit.pid) {
        Some(conn) => (human_bytes(conn.bytes_rx), human_bytes(conn.bytes_tx)),
        None => (dash(t), dash(t)),
    };
    let (name_plain, name_open) = name_cell(unit.kind, &unit.name);
    Row {
        sort_key: unit.pid,
        pid: unit.pid.to_string(),
        ppid: if unit.ppid == 0 {
            dash(t)
        } else {
            unit.ppid.to_string()
        },
        name_plain,
        name_open,
        state: state_char(unit.state, unit.parked),
        state_open: state_open(unit.state, unit.parked),
        restarts: unit.restart_count.to_string(),
        uptime: uptime_since(now, unit.started_at),
        rx,
        tx,
        exit: unit
            .last_exit
            .as_ref()
            .map_or_else(|| dash(t), |status| exit_text(t, status)),
    }
}

/// Rows for the connection units: one per unit in `--full`, otherwise a
/// single `conn × N` aggregate at the oldest pid.
fn conn_rows(
    now: Instant,
    conns: &HashMap<u64, &PsConn>,
    units: &[&PsUnit],
    full: bool,
    t: &I18n,
) -> Vec<Row> {
    if full {
        return units
            .iter()
            .map(|unit| unit_row(now, conns, unit, t))
            .collect();
    }
    if units.is_empty() {
        return Vec::new();
    }
    let mut rx_total = 0u64;
    let mut tx_total = 0u64;
    let mut restart_total = 0u32;
    let mut oldest = units[0].started_at;
    let mut worst = (PsUnitState::Running, false);
    for unit in units {
        if let Some(conn) = conns.get(&unit.pid) {
            rx_total = rx_total.saturating_add(conn.bytes_rx);
            tx_total = tx_total.saturating_add(conn.bytes_tx);
        }
        restart_total = restart_total.saturating_add(unit.restart_count);
        if unit.started_at < oldest {
            oldest = unit.started_at;
        }
        if state_severity(unit.state, unit.parked) > state_severity(worst.0, worst.1) {
            worst = (unit.state, unit.parked);
        }
    }
    let sort_key = units.iter().map(|unit| unit.pid).min().unwrap_or(0);
    let name = t.t(
        "candyriya.ps.conn_aggregate",
        &[("count", units.len().to_string().as_str())],
    );
    vec![Row {
        sort_key,
        pid: dash(t),
        ppid: dash(t),
        name_plain: escape_minimessage(&name),
        name_open: "<aqua>",
        state: state_char(worst.0, worst.1),
        state_open: state_open(worst.0, worst.1),
        restarts: restart_total.to_string(),
        uptime: uptime_since(now, oldest),
        rx: human_bytes(rx_total),
        tx: human_bytes(tx_total),
        exit: dash(t),
    }]
}

/// Column widths, from content and the (localized) header labels.
struct Widths {
    pid: usize,
    ppid: usize,
    name: usize,
    state: usize,
    restarts: usize,
    uptime: usize,
    rx: usize,
    tx: usize,
}

fn widths_of(t: &I18n, rows: &[Row]) -> Widths {
    let header = |key: &str| chars(&t.t(key, &[]));
    let mut widths = Widths {
        pid: header("candyriya.ps.col.pid").max(3),
        ppid: header("candyriya.ps.col.ppid").max(3),
        name: header("candyriya.ps.col.name").clamp(4, NAME_WIDTH),
        state: header("candyriya.ps.col.state").max(1),
        restarts: header("candyriya.ps.col.restarts").max(8),
        uptime: header("candyriya.ps.col.uptime").max(6),
        rx: header("candyriya.ps.col.rx").max(2),
        tx: header("candyriya.ps.col.tx").max(2),
    };
    for row in rows {
        widths.pid = widths.pid.max(chars(&row.pid));
        widths.ppid = widths.ppid.max(chars(&row.ppid));
        widths.name = widths.name.max(chars(&row.name_plain)).min(NAME_WIDTH);
        widths.restarts = widths.restarts.max(chars(&row.restarts));
        widths.uptime = widths.uptime.max(chars(&row.uptime));
        widths.rx = widths.rx.max(chars(&row.rx));
        widths.tx = widths.tx.max(chars(&row.tx));
    }
    widths
}

fn header_cell(text: String, width: usize, right: bool) -> String {
    format!("<aqua><bold>{}<reset>", pad(&text, width, right))
}

fn header_line(t: &I18n, widths: &Widths) -> String {
    let exit = format!("<aqua><bold>{}<reset>", t.t("candyriya.ps.col.exit", &[]));
    format!(
        "{}  {}  {}  {}  {}  {}  {}  {}  {}",
        header_cell(t.t("candyriya.ps.col.pid", &[]), widths.pid, true),
        header_cell(t.t("candyriya.ps.col.ppid", &[]), widths.ppid, true),
        header_cell(t.t("candyriya.ps.col.name", &[]), widths.name, false),
        header_cell(t.t("candyriya.ps.col.state", &[]), widths.state, false),
        header_cell(t.t("candyriya.ps.col.restarts", &[]), widths.restarts, true),
        header_cell(t.t("candyriya.ps.col.uptime", &[]), widths.uptime, true),
        header_cell(t.t("candyriya.ps.col.rx", &[]), widths.rx, true),
        header_cell(t.t("candyriya.ps.col.tx", &[]), widths.tx, true),
        exit,
    )
}

fn row_line(row: &Row, w: &Widths) -> String {
    let mut line = String::with_capacity(160);
    let _ = write!(
        line,
        "{}  {}  {}  {}  {}  {}  {}  {}  {}",
        pad(&row.pid, w.pid, true),
        pad(&row.ppid, w.ppid, true),
        row.name_cell(w.name),
        row.state_cell(w.state),
        pad(&row.restarts, w.restarts, true),
        pad(&row.uptime, w.uptime, true),
        pad(&row.rx, w.rx, true),
        pad(&row.tx, w.tx, true),
        row.exit,
    );
    line
}

fn summary_line(t: &I18n, snapshot: &PsSnapshot) -> String {
    let units = snapshot.units.len();
    let conn = snapshot
        .units
        .iter()
        .filter(|unit| unit.kind == PsUnitKind::Connection)
        .count();
    let plugins = snapshot
        .units
        .iter()
        .filter(|unit| unit.kind == PsUnitKind::Plugin)
        .count();
    t.t(
        "candyriya.ps.summary",
        &[
            ("units", units.to_string().as_str()),
            ("conn", conn.to_string().as_str()),
            ("plugins", plugins.to_string().as_str()),
            ("zombies", snapshot.zombies.to_string().as_str()),
            ("stuck", snapshot.stuck_zombies.to_string().as_str()),
        ],
    )
}

/// Render `ps` (or `ps --full`) to MiniMessage lines.
pub(crate) fn render_ps(snapshot: &PsSnapshot, full: bool, now: Instant, t: &I18n) -> Vec<String> {
    let conns = conn_map(&snapshot.conns);

    let mut rows: Vec<Row> = Vec::with_capacity(snapshot.units.len().saturating_add(1));
    let conn_units: Vec<&PsUnit> = snapshot
        .units
        .iter()
        .filter(|unit| unit.kind == PsUnitKind::Connection)
        .collect();
    for unit in snapshot
        .units
        .iter()
        .filter(|u| u.kind != PsUnitKind::Connection)
    {
        rows.push(unit_row(now, &conns, unit, t));
    }
    rows.extend(conn_rows(now, &conns, &conn_units, full, t));
    rows.sort_by_key(|row| row.sort_key);

    let mut lines = Vec::with_capacity(rows.len() + 3);
    if rows.is_empty() {
        lines.push(t.t("candyriya.ps.empty", &[]));
    } else {
        let widths = widths_of(t, &rows);
        lines.push(header_line(t, &widths));
        for row in &rows {
            lines.push(row_line(row, &widths));
        }
    }
    lines.push(summary_line(t, snapshot));
    lines.push(t.t("candyriya.ps.legend", &[]));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn en() -> I18n {
        I18n::new("en").expect("embedded en locale")
    }

    fn ex_girl() -> I18n {
        I18n::new("ex_girl").expect("embedded ex_girl locale")
    }

    fn unit(pid: u64, ppid: u64, name: &str, kind: PsUnitKind) -> PsUnit {
        PsUnit {
            pid,
            ppid,
            name: name.to_owned(),
            kind,
            state: PsUnitState::Running,
            parked: false,
            started_at: Instant::now(),
            restart_count: 0,
            last_exit: None,
            exited_at: None,
        }
    }

    fn conn(pid: u64) -> PsConn {
        PsConn {
            pid,
            bytes_rx: 100,
            bytes_tx: 200,
            connected_at: Instant::now(),
            last_activity: Instant::now(),
        }
    }

    fn snapshot(units: Vec<PsUnit>, conns: Vec<PsConn>) -> PsSnapshot {
        PsSnapshot {
            units,
            conns,
            zombies: 0,
            stuck_zombies: 0,
        }
    }

    #[test]
    fn state_char_covers_every_state_and_parked_gate() {
        assert_eq!(state_char(PsUnitState::Running, false), 'R');
        assert_eq!(state_char(PsUnitState::Running, true), 'F');
        assert_eq!(state_char(PsUnitState::Zombie, false), 'Z');
        assert_eq!(state_char(PsUnitState::Zombie, true), 'Z');
        assert_eq!(state_char(PsUnitState::Restarting, false), 't');
        assert_eq!(state_char(PsUnitState::FailedTerminal, false), '!');
    }

    #[test]
    fn severity_orders_worst_first() {
        assert!(
            state_severity(PsUnitState::FailedTerminal, false)
                > state_severity(PsUnitState::Zombie, false)
        );
        assert!(
            state_severity(PsUnitState::Zombie, false)
                > state_severity(PsUnitState::Restarting, false)
        );
        assert!(
            state_severity(PsUnitState::Restarting, false)
                > state_severity(PsUnitState::Running, true)
        );
        assert!(
            state_severity(PsUnitState::Running, true)
                > state_severity(PsUnitState::Running, false)
        );
    }

    #[test]
    fn aggregate_row_sums_and_keeps_worst_state() {
        let now = Instant::now();
        let mut zombie = unit(6, 4, "conn-2", PsUnitKind::Connection);
        zombie.state = PsUnitState::Zombie;
        zombie.started_at = now - std::time::Duration::from_secs(600);
        let mut fresh = unit(5, 4, "conn-1", PsUnitKind::Connection);
        fresh.started_at = now - std::time::Duration::from_secs(60);
        let snap = snapshot(
            vec![unit(1, 0, "init", PsUnitKind::LongLived), zombie, fresh],
            vec![
                PsConn {
                    pid: 5,
                    bytes_rx: 1024,
                    bytes_tx: 2048,
                    ..conn(5)
                },
                PsConn {
                    pid: 6,
                    bytes_rx: 4096,
                    bytes_tx: 8192,
                    ..conn(6)
                },
            ],
        );
        let lines = render_ps(&snap, false, now, &en());
        let joined = lines.join("\n");
        assert!(joined.contains("conn × 2"), "{joined}");
        assert!(joined.contains("5 KiB"), "rx summed: {joined}");
        assert!(joined.contains("10 KiB"), "tx summed: {joined}");
        assert!(joined.contains("<gold>Z"), "worst state wins: {joined}");
        assert!(joined.contains("10m"), "oldest uptime: {joined}");
    }

    #[test]
    fn aggregate_placed_at_oldest_conn_pid() {
        let now = Instant::now();
        let mut first = unit(50, 4, "conn-a", PsUnitKind::Connection);
        first.started_at = now - std::time::Duration::from_secs(500);
        let mut second = unit(52, 4, "conn-b", PsUnitKind::Connection);
        second.started_at = now - std::time::Duration::from_secs(50);
        let snap = snapshot(
            vec![
                unit(1, 0, "init", PsUnitKind::LongLived),
                second,
                first,
                unit(40, 1, "config", PsUnitKind::LongLived),
            ],
            vec![conn(50), conn(52)],
        );
        let lines = render_ps(&snap, false, now, &en());
        let aggregate = lines
            .iter()
            .position(|line| line.contains("conn × 2"))
            .expect("aggregate row");
        let init = lines
            .iter()
            .position(|line| line.contains("init"))
            .expect("init row");
        let config = lines
            .iter()
            .position(|line| line.contains("config"))
            .expect("config row");
        assert!(init < aggregate, "aggregate after pid-1 rows: {lines:?}");
        assert!(
            config < aggregate,
            "aggregate sits at the oldest conn pid: {lines:?}"
        );
    }

    #[test]
    fn full_mode_expands_every_connection() {
        let now = Instant::now();
        let units = (0..3)
            .map(|i| unit(10 + i, 4, &format!("conn-{i}"), PsUnitKind::Connection))
            .collect::<Vec<_>>();
        let snap = snapshot(units, vec![conn(10), conn(11), conn(12)]);
        let lines = render_ps(&snap, true, now, &en());
        let joined = lines.join("\n");
        for i in 0..3 {
            assert!(joined.contains(&format!("conn-{i}")), "{joined}");
        }
        assert!(!joined.contains("conn × 3"), "{joined}");
    }

    #[test]
    fn rows_render_ascending_pid_even_from_scrambled_input() {
        let now = Instant::now();
        let snap = snapshot(
            vec![
                unit(30, 1, "gamma", PsUnitKind::LongLived),
                unit(10, 0, "alpha", PsUnitKind::LongLived),
                unit(20, 1, "beta", PsUnitKind::LongLived),
            ],
            vec![],
        );
        let lines = render_ps(&snap, false, now, &en());
        let a = lines
            .iter()
            .position(|line| line.contains("alpha"))
            .unwrap();
        let b = lines.iter().position(|line| line.contains("beta")).unwrap();
        let c = lines
            .iter()
            .position(|line| line.contains("gamma"))
            .unwrap();
        assert!(a < b && b < c, "{lines:?}");
    }

    #[test]
    fn thousand_connections_render_bounded_and_complete() {
        let now = Instant::now();
        let mut units: Vec<PsUnit> = (0..1000)
            .map(|i| unit(100 + i, 4, &format!("conn-{i}"), PsUnitKind::Connection))
            .collect();
        units.insert(0, unit(1, 0, "init", PsUnitKind::LongLived));
        let conns = (100..1100).map(conn).collect();
        let snap = snapshot(units, conns);
        let lines = render_ps(&snap, true, now, &en());
        assert_eq!(lines.len(), 1004); // header + 1001 rows + summary + legend
        // measure the visible width, not the MiniMessage tag overhead
        fn visible(line: &str) -> usize {
            let mut depth: u32 = 0;
            let mut out = 0usize;
            for ch in line.chars() {
                match ch {
                    '<' => depth += 1,
                    '>' => depth = depth.saturating_sub(1),
                    _ => {
                        if depth == 0 {
                            out += 1;
                        }
                    }
                }
            }
            out
        }
        for line in &lines {
            assert!(
                visible(line) <= 160,
                "line too wide ({} cols): {line}",
                visible(line)
            );
        }
    }

    #[test]
    fn overlong_names_are_truncated_with_ellipsis() {
        let now = Instant::now();
        let long = "x".repeat(120);
        let snap = snapshot(vec![unit(1, 0, &long, PsUnitKind::LongLived)], vec![]);
        let lines = render_ps(&snap, false, now, &en());
        let joined = lines.join("\n");
        assert!(joined.contains('…'), "{joined}");
        assert!(
            !joined.contains(&"x".repeat(120)),
            "full name must not appear: {joined}"
        );
    }

    #[test]
    fn failure_reason_is_cut_with_ellipsis() {
        let now = Instant::now();
        let mut unit = unit(1, 0, "plugin-dead", PsUnitKind::Plugin);
        unit.state = PsUnitState::FailedTerminal;
        unit.restart_count = 5;
        unit.last_exit = Some(PsExitStatus::Failure("e".repeat(200)));
        let snap = snapshot(vec![unit], vec![]);
        let lines = render_ps(&snap, false, now, &en());
        let joined = lines.join("\n");
        assert!(joined.contains('…'), "{joined}");
        assert!(!joined.contains(&"e".repeat(120)), "{joined}");
    }

    #[test]
    fn empty_tree_renders_honest_empty_state() {
        let snap = snapshot(vec![], vec![]);
        let lines = render_ps(&snap, false, Instant::now(), &en());
        assert!(lines[0].contains("No units"), "{lines:?}");
        let joined = lines.join("\n");
        assert!(joined.contains("units: <gold>0<reset>"), "{joined}");
        assert!(joined.contains("zombies: <gold>0<reset>"), "{joined}");
    }

    #[test]
    fn zombies_in_summary_come_from_the_tree_not_the_rows() {
        let now = Instant::now();
        let snap = PsSnapshot {
            units: vec![unit(1, 0, "init", PsUnitKind::LongLived)],
            conns: vec![],
            zombies: 7,
            stuck_zombies: 2,
        };
        let lines = render_ps(&snap, false, now, &en());
        let joined = lines.join("\n");
        assert!(joined.contains("zombies: <gold>7<reset>"), "{joined}");
        assert!(joined.contains("stuck: <gold>2<reset>"), "{joined}");
    }

    #[test]
    fn every_new_locale_key_resolves_in_both_locales() {
        const KEYS: &[&str] = &[
            "candyriya.kind.connection",
            "candyriya.kind.every",
            "candyriya.kind.long_lived",
            "candyriya.kind.oneshot",
            "candyriya.kind.plugin",
            "candyriya.ps.col.exit",
            "candyriya.ps.col.name",
            "candyriya.ps.col.pid",
            "candyriya.ps.col.ppid",
            "candyriya.ps.col.restarts",
            "candyriya.ps.col.rx",
            "candyriya.ps.col.state",
            "candyriya.ps.col.tx",
            "candyriya.ps.col.uptime",
            "candyriya.ps.conn_aggregate",
            "candyriya.ps.empty",
            "candyriya.ps.exit.cancelled",
            "candyriya.ps.exit.failure",
            "candyriya.ps.exit.panicked",
            "candyriya.ps.exit.success",
            "candyriya.ps.exit.timed_out",
            "candyriya.ps.legend",
            "candyriya.ps.none",
            "candyriya.ps.summary",
            "candyriya.tree.exit",
            "candyriya.tree.frozen",
        ];
        for locale in [en(), ex_girl()] {
            for key in KEYS {
                let value = locale.t(key, &[]);
                // t() falls back to the key itself when the key is missing
                assert_ne!(
                    value,
                    key.to_string(),
                    "{key} must resolve in {}",
                    locale.locale()
                );
                assert!(
                    !value.is_empty(),
                    "{key} must not be empty in {}",
                    locale.locale()
                );
            }
        }
    }
}
