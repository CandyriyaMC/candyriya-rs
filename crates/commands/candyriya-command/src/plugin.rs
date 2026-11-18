//! The `/candyriya plugin` surface — the phase-2 operator commands for
//! native plugins, as a pure brigadier wire frame.
//!
//! What lives here: the [`PluginView`] trait the composition root
//! implements against the running plugin supervision tree
//! (`plugin-controller` and the `plugin-<id>` units under it), the typed
//! [`PluginViewResult`] envelope every call reports through, the
//! [`PluginViewPrefix`] handle the composition root wires in (the same
//! shape as the `/candyriya reload` hook), and the handlers plus the
//! pure line renderers.
//!
//! What lives elsewhere: the supervision integration itself (who calls
//! the controller, when, with which deadlines) lands with the plugin
//! controller integration; the tree unit that answers the calls does
//! not exist yet in this build. When the build did not wire a
//! [`PluginViewPrefix`], every subcommand answers with a localized
//! notice instead of pretending — the fail-soft mirror of a missing
//! reload hook.
//!
//! The wire machine renders lowercase, exactly as documented for the
//! phase-2 contract; there is deliberately no `failed` word. A failed
//! transition latches a reason on the record and the plugin travels the
//! `unloading -> unloaded` teardown; renderers show that reason through
//! [`PluginViewResult::Refused`] / [`PluginViewResult::Failed`].
//!
//! User-facing strings go through i18n (`candyriya.plugin.*`); the
//! machine state words and the `ok`/`err`/`timeout` outcome tokens are
//! contract spellings and are printed raw on purpose.

use std::fmt;
use std::sync::Arc;

use candyriya_i18n::I18n;

use crate::ps::{PsUnitState, exit_text};
use crate::source::CommandSource;
use crate::state::CommandState;
use crate::text::escape_minimessage;

/// The phase-2 wire machine's states, copied crate-locally on purpose:
/// this crate does not depend on the controller crate (the conversion
/// runs at the seam in the composition root, the same split as
/// [`crate::PsUnitKind`] versus the kernel's unit vocabulary).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PluginWireStateView {
    /// Discovered, no unit yet.
    Discovered,
    /// Loading the library.
    Loading,
    /// Library mapped, handshake passed.
    Loaded,
    /// Running the init callback.
    Initializing,
    /// Init done.
    Initialized,
    /// Running the enable callback.
    Enabling,
    /// Steady state.
    Running,
    /// Running the disable callback.
    Disabling,
    /// Cooperatively stopped, library still mapped.
    Disabled,
    /// Tearing down.
    Unloading,
    /// Terminal; the library mapping may still be gone or detached.
    Unloaded,
}

impl PluginWireStateView {
    /// The contract spelling: one lowercase word, no `failed`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Discovered => "discovered",
            Self::Loading => "loading",
            Self::Loaded => "loaded",
            Self::Initializing => "initializing",
            Self::Initialized => "initialized",
            Self::Enabling => "enabling",
            Self::Running => "running",
            Self::Disabling => "disabling",
            Self::Disabled => "disabled",
            Self::Unloading => "unloading",
            Self::Unloaded => "unloaded",
        }
    }
}

/// How the last lifecycle step ended. `-` when unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PluginOutcome {
    /// The call ran to completion.
    Ok,
    /// The call failed; the reason travels on the record, not here.
    Err,
    /// The call missed its deadline.
    Timeout,
}

impl PluginOutcome {
    /// The contract spelling: `ok` / `err` / `timeout`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Err => "err",
            Self::Timeout => "timeout",
        }
    }
}

/// One row of `/candyriya plugin list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginListEntry {
    /// The plugin id.
    pub id: String,
    /// The manifest version string, as written.
    pub version: String,
    /// The wire state, rendered with [`PluginWireStateView::as_str`].
    pub state: PluginWireStateView,
    /// The outcome of the last lifecycle step, `-` when unknown.
    pub last_outcome: Option<PluginOutcome>,
    /// `yes` while the unit waits out a restart backoff (`RestartPolicy::Never`
    /// in the recommended phase-2 policy, which never waits).
    pub restart_elected: bool,
    /// The discovered library path, as recorded at discovery.
    pub source: String,
}

/// A one-shot detail view for `/candyriya plugin info`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginInfo {
    /// The canonical (resolved) plugin id.
    pub id: String,
    /// The manifest version string, as written.
    pub version: String,
    /// The human-readable description, if the manifest carries one.
    pub description: Option<String>,
    /// The plugin author(s), if the manifest carries them.
    pub author: Option<String>,
    /// The entry-point symbol resolved at load; `None` before the first
    /// load (the contract keeps the resolution at load time).
    pub entry_point: Option<String>,
    /// The wire state, rendered with [`PluginWireStateView::as_str`].
    pub state: PluginWireStateView,
    /// Recent supervision-unit exit statuses, oldest first; a bounded
    /// ring whose size is a phase-2 constant.
    pub recent_exits: Vec<crate::PsExitStatus>,
    /// The failure reason latched on the record, if any. Rendered on
    /// its own line when set.
    pub reason: Option<String>,
}

/// What a mutating verb did to the plugin, echoed back from the callee.
///
/// Carries the canonical id the action applied to, so the success line
/// reports the spelling the controller knows — not the (maybe
/// case-insensitively matched) spelling the operator typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginActionReceipt {
    /// The canonical plugin id the action applied to.
    pub id: String,
}

/// What an unload ended up doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnloadReceipt {
    /// The canonical plugin id that was unloaded.
    pub id: String,
    /// `true` when the plugin OS thread missed the join bound and was
    /// detached; the mapping stays and the library file cannot be
    /// relied on to unmap. The response must say so.
    pub detached: bool,
}

/// The typed outcome of one plugin control call.
///
/// No variant is a stringly error: the CLI renders each from its
/// payload, so a missing reason or an unknown id is always rendered
/// honestly instead of being flattened into prose early.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginViewResult<T> {
    /// The call ran; here is the payload.
    Ok(T),
    /// The id is unknown to the controller. Carries the ids it does know
    /// (exact spellings), so the CLI can name them in the rejection line.
    /// Resolution order: exact match, then case-insensitive.
    UnknownId {
        /// The ids the controller knows, in discovery order.
        known: Vec<String>,
    },
    /// The action was refused before it ran (policy gate: running
    /// plugin, `disable_failed` latch, failure-latched record...). One
    /// line names the reason; the plugin stays where the machine left it.
    Refused {
        /// Plain text, bounded upstream; escaped at render time.
        reason: String,
    },
    /// A phase ran and failed. One line names the reason; nothing is
    /// retried.
    Failed {
        /// Plain text, bounded upstream; escaped at render time.
        reason: String,
    },
}

impl<T> PluginViewResult<T> {
    /// Whether the call ran to completion.
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Ok(_))
    }
}

/// How one row of `/candyriya plugin tree` prints its state word.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PluginTreeNodeState {
    /// The `plugins` group node or `plugin-controller`: a supervised unit.
    Unit(PsUnitState),
    /// A `plugin-<id>` unit: carries the machine's wire state.
    Wire(PluginWireStateView),
}

impl PluginTreeNodeState {
    /// The lowercase word printed inside the `[pid .., ppid .., state]` suffix.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Unit(PsUnitState::Running) => "running",
            Self::Unit(PsUnitState::Zombie) => "zombie",
            Self::Unit(PsUnitState::Restarting) => "restarting",
            Self::Unit(PsUnitState::FailedTerminal) => "failed-terminal",
            Self::Wire(state) => state.as_str(),
        }
    }
}

/// One line of `/candyriya plugin tree`: the `plugins` group node,
/// `plugin-controller`, or one `plugin-<id>` unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginTreeRow {
    /// The unit's pid.
    pub pid: u64,
    /// The parent's pid at snapshot time; `0` for the root.
    pub ppid: u64,
    /// The unit's name as the tree records it (`plugins`,
    /// `plugin-controller`, `plugin-<id>`).
    pub name: String,
    /// What prints for the state word.
    pub state: PluginTreeNodeState,
}

/// The phase-2 plugin control surface, as the command layer sees it.
///
/// Implemented by the composition root against the running plugin
/// supervision tree; this crate only parses, gates and renders. The id
/// resolution rule is the implementation's contract, restated there
/// once: exact match first, then case-insensitive; an unknown id is
/// [`PluginViewResult::UnknownId`], never a partial run.
///
/// Implementations must be total and bounded: the tree's own deadlines
/// bound every call (load/init/enable/disable/unload 10 s, join 5 s),
/// so no call may block the caller indefinitely on plugin code.
pub trait PluginView: Send + Sync {
    /// Every plugin known to the controller, in discovery order.
    fn list(&self) -> PluginViewResult<Vec<PluginListEntry>>;
    /// A one-shot detail view for one plugin.
    fn info(&self, id: &str) -> PluginViewResult<PluginInfo>;
    /// Drive the plugin to `running`. A running plugin is a no-op
    /// reporting success; a failure-latched one is refused.
    fn enable(&self, id: &str) -> PluginViewResult<PluginActionReceipt>;
    /// Cooperative stop. Disable never unmaps the library; a plugin
    /// that does not acknowledge is refused with the latch set.
    fn disable(&self, id: &str) -> PluginViewResult<PluginActionReceipt>;
    /// `disable -> unload -> rescan -> enable`, each step a no-op when
    /// its precondition already holds.
    fn reload(&self, id: &str) -> PluginViewResult<PluginActionReceipt>;
    /// Hard unload; requires `disabled` (a running plugin is refused
    /// with a reason naming the state).
    fn unload(&self, id: &str) -> PluginViewResult<UnloadReceipt>;
    /// The `plugins` subtree rows, parent before child: the group node
    /// first, then `plugin-controller`, then one row per plugin unit.
    fn tree(&self) -> PluginViewResult<Vec<PluginTreeRow>>;
}

/// The addressable handle for the [`PluginView`] service, wired by the
/// composition root.
///
/// Cheap to clone (shared `Arc`); one handle per process. The name
/// mirrors the phase-2 naming rule — the units this handle answers
/// for all live under the `plugin-` prefix (`plugin-controller`,
/// `plugin-<id>`). Without one, every `/candyriya plugin` subcommand
/// answers with a localized notice instead of pretending — the
/// fail-soft mirror of a missing `/candyriya reload` hook.
#[derive(Clone)]
pub struct PluginViewPrefix {
    inner: Arc<dyn PluginView>,
}

impl PluginViewPrefix {
    /// Wraps a view implementation as the shared handle.
    pub fn new(view: impl PluginView + 'static) -> Self {
        Self {
            inner: Arc::new(view),
        }
    }

    /// Wraps an already-shared view.
    pub fn from_arc(view: Arc<dyn PluginView>) -> Self {
        Self { inner: view }
    }
}

impl std::ops::Deref for PluginViewPrefix {
    type Target = dyn PluginView;

    fn deref(&self) -> &Self::Target {
        &*self.inner
    }
}

impl fmt::Debug for PluginViewPrefix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginViewPrefix")
            .field("wired", &true)
            .finish()
    }
}

// ---------- rendering ----------

/// Visible character count of a plain (tag-free) string. A local copy of
/// the ps helpers; kept here so ps.rs stays byte-identical.
fn chars(text: &str) -> usize {
    text.chars().count()
}

/// Pad a plain string to `width` chars, left-aligned.
fn pad(text: &str, width: usize) -> String {
    let padding = width.saturating_sub(chars(text));
    format!("{text}{}", " ".repeat(padding))
}

fn header_cell(text: String, width: usize) -> String {
    format!("<aqua><bold>{}<reset>", pad(&text, width))
}

fn dash(t: &I18n) -> String {
    t.t("candyriya.ps.none", &[])
}

fn escape_field(value: &str) -> String {
    escape_minimessage(value)
}

/// Render `/candyriya plugin list` as MiniMessage lines.
///
/// The table follows the ps convention: content-driven widths, all cells
/// left-aligned, two spaces between columns, bold aqua headers. The list
/// is small (one row per plugin), so nothing is truncated; the source
/// path prints as recorded at discovery.
pub(crate) fn render_list(t: &I18n, rows: &[PluginListEntry]) -> Vec<String> {
    if rows.is_empty() {
        return vec![t.t("candyriya.plugin.none_found", &[])];
    }
    let id_col = t.t("candyriya.plugin.col.id", &[]);
    let version_col = t.t("candyriya.plugin.col.version", &[]);
    let state_col = t.t("candyriya.plugin.col.state", &[]);
    let outcome_col = t.t("candyriya.plugin.col.outcome", &[]);
    let restart_col = t.t("candyriya.plugin.col.restart_elected", &[]);
    let source_col = t.t("candyriya.plugin.col.source", &[]);
    let yes = t.t("candyriya.plugin.restart.yes", &[]);
    let no = t.t("candyriya.plugin.restart.no", &[]);
    let dash = dash(t);

    let cells: Vec<[String; 6]> = rows
        .iter()
        .map(|row| {
            [
                escape_field(&row.id),
                escape_field(&row.version),
                row.state.as_str().to_owned(),
                row.last_outcome
                    .map_or_else(|| dash.clone(), |outcome| outcome.as_str().to_owned()),
                if row.restart_elected {
                    yes.clone()
                } else {
                    no.clone()
                },
                escape_field(&row.source),
            ]
        })
        .collect();
    let mut widths = [
        chars(&id_col),
        chars(&version_col),
        chars(&state_col),
        chars(&outcome_col),
        chars(&restart_col),
        0,
    ];
    for cell in &cells {
        for (index, cell) in cell.iter().enumerate().take(5) {
            widths[index] = widths[index].max(chars(cell));
        }
    }

    let header = format!(
        "{}  {}  {}  {}  {}  {}",
        header_cell(id_col, widths[0]),
        header_cell(version_col, widths[1]),
        header_cell(state_col, widths[2]),
        header_cell(outcome_col, widths[3]),
        header_cell(restart_col, widths[4]),
        source_col,
    );
    let mut lines = Vec::with_capacity(cells.len() + 1);
    lines.push(header);
    for cell in &cells {
        lines.push(format!(
            "{}  {}  {}  {}  {}  {}",
            pad(&cell[0], widths[0]),
            pad(&cell[1], widths[1]),
            pad(&cell[2], widths[2]),
            pad(&cell[3], widths[3]),
            pad(&cell[4], widths[4]),
            cell[5],
        ));
    }
    lines
}

/// Render `/candyriya plugin info` as MiniMessage lines.
///
/// The `Reason:` line prints only when the record is failure-latched;
/// `-` fills the unset slots, the same convention `ps` uses for empty
/// cells.
pub(crate) fn render_info(t: &I18n, info: &PluginInfo) -> Vec<String> {
    let dash = dash(t);
    let optional = |value: &Option<String>| -> String {
        value.as_deref().map_or_else(|| dash.clone(), escape_field)
    };
    let exits = if info.recent_exits.is_empty() {
        dash.clone()
    } else {
        info.recent_exits
            .iter()
            .map(|status| exit_text(t, status))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut lines = vec![
        t.t(
            "candyriya.plugin.info.plugin",
            &[("id", escape_field(&info.id).as_str())],
        ),
        t.t(
            "candyriya.plugin.info.version",
            &[("version", escape_field(&info.version).as_str())],
        ),
        t.t(
            "candyriya.plugin.info.description",
            &[("description", optional(&info.description).as_str())],
        ),
        t.t(
            "candyriya.plugin.info.author",
            &[("author", optional(&info.author).as_str())],
        ),
        t.t(
            "candyriya.plugin.info.entry_point",
            &[("entry_point", optional(&info.entry_point).as_str())],
        ),
        t.t(
            "candyriya.plugin.info.state",
            &[("state", info.state.as_str())],
        ),
        t.t(
            "candyriya.plugin.info.recent_exits",
            &[("exits", exits.as_str())],
        ),
    ];
    if let Some(reason) = &info.reason {
        lines.push(t.t(
            "candyriya.plugin.info.reason",
            &[("reason", escape_field(reason).as_str())],
        ));
    }
    lines
}

/// Render `/candyriya plugin tree` as MiniMessage lines.
///
/// One line per row, `plugins [pid 5, ppid 1, running]` style; box-drawing
/// guides identical to `/candyriya tree`, but with no captions, no kind
/// markers, no summary line. Rows whose parent is outside the snapshot are
/// promoted to top level, never dropped.
pub(crate) fn render_tree(t: &I18n, rows: &[PluginTreeRow]) -> Vec<String> {
    if rows.is_empty() {
        return vec![t.t("candyriya.plugin.none_found", &[])];
    }
    let known: std::collections::HashSet<u64> = rows.iter().map(|row| row.pid).collect();
    let mut children: std::collections::BTreeMap<u64, Vec<&PluginTreeRow>> =
        std::collections::BTreeMap::new();
    for row in rows {
        children.entry(row.ppid).or_default().push(row);
    }
    for kids in children.values_mut() {
        kids.sort_by_key(|row| row.pid);
    }
    let mut roots: Vec<&PluginTreeRow> = rows
        .iter()
        .filter(|row| row.ppid == 0 || !known.contains(&row.ppid))
        .collect();
    roots.sort_by_key(|row| row.pid);

    let mut lines = Vec::with_capacity(rows.len());
    let mut seen = std::collections::HashSet::new();
    let mut ctx = TreeCtx {
        children: &children,
        seen: &mut seen,
        t,
    };
    for root in roots {
        tree_node(root, "", true, true, &mut ctx, &mut lines);
    }
    // The kernel never emits a pid cycle, but the DTO promises nothing:
    // anything not reached from a root is promoted, by pid.
    let mut promoted: Vec<&PluginTreeRow> = rows
        .iter()
        .filter(|row| !ctx.seen.contains(&row.pid))
        .collect();
    promoted.sort_by_key(|row| row.pid);
    for row in promoted {
        if ctx.seen.contains(&row.pid) {
            continue;
        }
        tree_node(row, "", true, true, &mut ctx, &mut lines);
    }
    lines
}

struct TreeCtx<'a> {
    children: &'a std::collections::BTreeMap<u64, Vec<&'a PluginTreeRow>>,
    seen: &'a mut std::collections::HashSet<u64>,
    t: &'a I18n,
}

fn tree_node(
    row: &PluginTreeRow,
    prefix: &str,
    is_tail: bool,
    is_top: bool,
    ctx: &mut TreeCtx<'_>,
    lines: &mut Vec<String>,
) {
    ctx.seen.insert(row.pid);
    let guide = if is_top {
        ""
    } else if is_tail {
        "└── "
    } else {
        "├── "
    };
    lines.push(tree_line(ctx.t, row, prefix, guide));

    let child_prefix = if is_top {
        prefix.to_owned()
    } else if is_tail {
        format!("{prefix}    ")
    } else {
        format!("{prefix}│   ")
    };

    let kids: Vec<&PluginTreeRow> = ctx
        .children
        .get(&row.pid)
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .copied()
        .filter(|child| !ctx.seen.contains(&child.pid))
        .collect();
    let last = kids.len().saturating_sub(1);
    for (index, child) in kids.iter().enumerate() {
        tree_node(child, &child_prefix, index == last, false, ctx, lines);
    }
}

fn tree_line(t: &I18n, row: &PluginTreeRow, prefix: &str, guide: &str) -> String {
    let ppid = if row.ppid == 0 {
        dash(t)
    } else {
        row.ppid.to_string()
    };
    format!(
        "{prefix}{guide}{} <dark_gray>[pid {}, ppid {}, {}]<reset>",
        escape_field(&row.name),
        row.pid,
        ppid,
        row.state.as_str(),
    )
}

// ---------- handlers ----------

/// Emit one line to `source`. Local mirror of the `commands::common::out`
/// wrapper — kept here so this module stays self-contained.
fn out(state: &CommandState, source: &CommandSource, line: String) {
    state.effects.send(source, &line);
}

/// The localized fail-soft line when the build did not wire a
/// [`PluginViewPrefix`]. Returns 0 (the command ran, the report is the
/// notice), mirroring the missing reload hook.
fn handle_unwired(state: &CommandState, source: &CommandSource) -> i32 {
    let t = state.locales.pick(source);
    out(state, source, t.t("candyriya.plugin.unavailable", &[]));
    0
}

/// Render a failed [`PluginViewResult`] arm to one line and return 0.
///
/// `name` is the raw id argument for id-taking subcommands; `None` for
/// `list`/`tree`, where an `UnknownId` arm renders as a reason line with
/// the known-list spelled out (the service never returns it there).
fn plugin_error<T>(
    state: &CommandState,
    source: &CommandSource,
    t: &I18n,
    name: Option<&str>,
    err: PluginViewResult<T>,
) -> i32 {
    let line = match err {
        PluginViewResult::UnknownId { known } => {
            let mut names = known;
            names.sort();
            let known = names
                .iter()
                .map(|name| escape_field(name))
                .collect::<Vec<_>>()
                .join(", ");
            match name {
                Some(name) => t.t(
                    "candyriya.plugin.unknown",
                    &[
                        ("name", escape_field(name).as_str()),
                        ("known", known.as_str()),
                    ],
                ),
                None => t.t(
                    "candyriya.plugin.failed",
                    &[(
                        "reason",
                        format!("unknown plugin id; known plugins: {known}").as_str(),
                    )],
                ),
            }
        }
        PluginViewResult::Refused { reason } | PluginViewResult::Failed { reason } => t.t(
            "candyriya.plugin.failed",
            &[("reason", escape_field(&reason).as_str())],
        ),
        PluginViewResult::Ok(_) => {
            // plugin_error is only called on the failure arms; an Ok
            // landing here is a bug in the caller, kept non-fatal
            tracing::error!("plugin_error called on an Ok result; reporting success anyway");
            return 1;
        }
    };
    out(state, source, line);
    0
}

/// Which receipt-producing verb a handler invocation is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReceiptVerb {
    /// `/candyriya plugin enable <id>`
    Enable,
    /// `/candyriya plugin disable <id>`
    Disable,
    /// `/candyriya plugin reload <id>`
    Reload,
}

impl ReceiptVerb {
    const fn ok_key(self) -> &'static str {
        match self {
            Self::Enable => "candyriya.plugin.enable_ok",
            Self::Disable => "candyriya.plugin.disable_ok",
            Self::Reload => "candyriya.plugin.reload_ok",
        }
    }
}

/// `/candyriya plugin list` — one line per plugin known to the controller.
pub(crate) fn handle_plugin_list(
    state: &CommandState,
    source: &CommandSource,
    plugin_view: &Option<PluginViewPrefix>,
) -> i32 {
    let t = state.locales.pick(source);
    let Some(view) = plugin_view else {
        return handle_unwired(state, source);
    };
    match view.list() {
        PluginViewResult::Ok(rows) => {
            for line in render_list(t, &rows) {
                out(state, source, line);
            }
            1
        }
        err => plugin_error(state, source, t, None, err),
    }
}

/// `/candyriya plugin info <id>` — the one-shot detail sheet.
pub(crate) fn handle_plugin_info(
    state: &CommandState,
    source: &CommandSource,
    plugin_view: &Option<PluginViewPrefix>,
    id: &str,
) -> i32 {
    let t = state.locales.pick(source);
    let Some(view) = plugin_view else {
        return handle_unwired(state, source);
    };
    match view.info(id) {
        PluginViewResult::Ok(info) => {
            for line in render_info(t, &info) {
                out(state, source, line);
            }
            1
        }
        err => plugin_error(state, source, t, Some(id), err),
    }
}

/// `/candyriya plugin enable|disable|reload <id>` — the receipt verbs.
pub(crate) fn handle_plugin_receipt(
    state: &CommandState,
    source: &CommandSource,
    plugin_view: &Option<PluginViewPrefix>,
    id: &str,
    verb: ReceiptVerb,
) -> i32 {
    let t = state.locales.pick(source);
    let Some(view) = plugin_view else {
        return handle_unwired(state, source);
    };
    let result = match verb {
        ReceiptVerb::Enable => view.enable(id),
        ReceiptVerb::Disable => view.disable(id),
        ReceiptVerb::Reload => view.reload(id),
    };
    match result {
        PluginViewResult::Ok(receipt) => {
            out(
                state,
                source,
                t.t(verb.ok_key(), &[("id", escape_field(&receipt.id).as_str())]),
            );
            1
        }
        err => plugin_error(state, source, t, Some(id), err),
    }
}

/// `/candyriya plugin unload <id>` — hard unload without reload.
pub(crate) fn handle_plugin_unload(
    state: &CommandState,
    source: &CommandSource,
    plugin_view: &Option<PluginViewPrefix>,
    id: &str,
) -> i32 {
    let t = state.locales.pick(source);
    let Some(view) = plugin_view else {
        return handle_unwired(state, source);
    };
    match view.unload(id) {
        PluginViewResult::Ok(receipt) => {
            out(
                state,
                source,
                t.t(
                    "candyriya.plugin.unload_ok",
                    &[("id", escape_field(&receipt.id).as_str())],
                ),
            );
            if receipt.detached {
                out(
                    state,
                    source,
                    t.t(
                        "candyriya.plugin.unload_detached",
                        &[("id", escape_field(&receipt.id).as_str())],
                    ),
                );
            }
            1
        }
        err => plugin_error(state, source, t, Some(id), err),
    }
}

/// `/candyriya plugin tree` — the `plugins` subtree, box-drawing.
pub(crate) fn handle_plugin_tree(
    state: &CommandState,
    source: &CommandSource,
    plugin_view: &Option<PluginViewPrefix>,
) -> i32 {
    let t = state.locales.pick(source);
    let Some(view) = plugin_view else {
        return handle_unwired(state, source);
    };
    match view.tree() {
        PluginViewResult::Ok(rows) => {
            for line in render_tree(t, &rows) {
                out(state, source, line);
            }
            1
        }
        err => plugin_error(state, source, t, None, err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ps::PsExitStatus;

    fn en() -> I18n {
        I18n::new("en").expect("embedded en locale")
    }

    fn ex_girl() -> I18n {
        I18n::new("ex_girl").expect("embedded ex_girl locale")
    }

    fn entry(id: &str, state: PluginWireStateView) -> PluginListEntry {
        PluginListEntry {
            id: id.to_owned(),
            version: "0.1.0".to_owned(),
            state,
            last_outcome: Some(PluginOutcome::Ok),
            restart_elected: false,
            source: format!("plugins/{id}/{id}.dll"),
        }
    }

    fn row(pid: u64, ppid: u64, name: &str, state: PluginTreeNodeState) -> PluginTreeRow {
        PluginTreeRow {
            pid,
            ppid,
            name: name.to_owned(),
            state,
        }
    }

    /// A canned view for the prefix wiring tests; not for dispatch tests
    /// (those live in `tests/commands.rs`).
    struct CannedView;

    impl PluginView for CannedView {
        fn list(&self) -> PluginViewResult<Vec<PluginListEntry>> {
            PluginViewResult::Ok(vec![])
        }
        fn info(&self, id: &str) -> PluginViewResult<PluginInfo> {
            PluginViewResult::UnknownId {
                known: vec![id.to_owned()],
            }
        }
        fn enable(&self, id: &str) -> PluginViewResult<PluginActionReceipt> {
            PluginViewResult::Ok(PluginActionReceipt { id: id.to_owned() })
        }
        fn disable(&self, id: &str) -> PluginViewResult<PluginActionReceipt> {
            PluginViewResult::Ok(PluginActionReceipt { id: id.to_owned() })
        }
        fn reload(&self, id: &str) -> PluginViewResult<PluginActionReceipt> {
            PluginViewResult::Ok(PluginActionReceipt { id: id.to_owned() })
        }
        fn unload(&self, id: &str) -> PluginViewResult<UnloadReceipt> {
            PluginViewResult::Ok(UnloadReceipt {
                id: id.to_owned(),
                detached: false,
            })
        }
        fn tree(&self) -> PluginViewResult<Vec<PluginTreeRow>> {
            PluginViewResult::Ok(vec![])
        }
    }

    #[test]
    fn wire_state_spelling_is_the_contract_eleven() {
        let states = [
            PluginWireStateView::Discovered,
            PluginWireStateView::Loading,
            PluginWireStateView::Loaded,
            PluginWireStateView::Initializing,
            PluginWireStateView::Initialized,
            PluginWireStateView::Enabling,
            PluginWireStateView::Running,
            PluginWireStateView::Disabling,
            PluginWireStateView::Disabled,
            PluginWireStateView::Unloading,
            PluginWireStateView::Unloaded,
        ];
        let spellings: Vec<&'static str> = states.iter().map(|state| state.as_str()).collect();
        assert_eq!(
            spellings,
            [
                "discovered",
                "loading",
                "loaded",
                "initializing",
                "initialized",
                "enabling",
                "running",
                "disabling",
                "disabled",
                "unloading",
                "unloaded",
            ]
        );
    }

    #[test]
    fn outcome_spelling_is_ok_err_timeout() {
        assert_eq!(PluginOutcome::Ok.as_str(), "ok");
        assert_eq!(PluginOutcome::Err.as_str(), "err");
        assert_eq!(PluginOutcome::Timeout.as_str(), "timeout");
    }

    #[test]
    fn tree_node_state_spelling() {
        assert_eq!(
            PluginTreeNodeState::Unit(PsUnitState::Running).as_str(),
            "running"
        );
        assert_eq!(
            PluginTreeNodeState::Unit(PsUnitState::Zombie).as_str(),
            "zombie"
        );
        assert_eq!(
            PluginTreeNodeState::Unit(PsUnitState::Restarting).as_str(),
            "restarting"
        );
        assert_eq!(
            PluginTreeNodeState::Unit(PsUnitState::FailedTerminal).as_str(),
            "failed-terminal"
        );
        assert_eq!(
            PluginTreeNodeState::Wire(PluginWireStateView::Disabled).as_str(),
            "disabled"
        );
        assert_eq!(
            PluginTreeNodeState::Wire(PluginWireStateView::Unloaded).as_str(),
            "unloaded"
        );
    }

    #[test]
    fn prefix_is_cheap_to_clone_and_debug() {
        let prefix = PluginViewPrefix::new(CannedView);
        let clone = prefix.clone();
        drop(clone);
        let debug = format!("{prefix:?}");
        assert!(debug.contains("wired"), "{debug}");
        let shared = PluginViewPrefix::from_arc(Arc::new(CannedView));
        assert_eq!(format!("{shared:?}"), format!("{prefix:?}"));
    }

    #[test]
    fn list_renders_header_and_aligned_rows() {
        let rows = vec![
            entry("greeting", PluginWireStateView::Running),
            PluginListEntry {
                last_outcome: Some(PluginOutcome::Err),
                source: "plugins/quests/quests.dll".to_owned(),
                ..entry("quests", PluginWireStateView::Unloaded)
            },
        ];
        let lines = render_list(&en(), &rows);
        assert_eq!(lines.len(), 3, "{lines:?}");
        for label in [
            "ID",
            "VERSION",
            "STATE",
            "LAST OUTCOME",
            "RESTART ELECTED",
            "SOURCE",
        ] {
            assert!(lines[0].contains(label), "header {label} in {lines:?}");
        }
        assert!(lines[1].contains("greeting"), "{lines:?}");
        assert!(
            lines[1].contains("plugins/greeting/greeting.dll"),
            "{lines:?}"
        );
        assert!(lines[2].contains("quests"), "{lines:?}");
        assert!(lines[2].contains("err"), "{lines:?}");
        let version_column = |line: &str| line.find("0.1.0").expect("version cell");
        assert_eq!(version_column(&lines[1]), version_column(&lines[2]));
    }

    #[test]
    fn list_with_nothing_reports_none_found() {
        let lines = render_list(&en(), &[]);
        assert_eq!(lines, vec!["No plugins found.".to_string()]);
    }

    #[test]
    fn info_renders_reason_only_when_latched() {
        let base = PluginInfo {
            id: "greeting".to_owned(),
            version: "0.1.0".to_owned(),
            description: Some("says hello on startup".to_owned()),
            author: Some("Jane Doe".to_owned()),
            entry_point: Some("candyriya_plugin_init".to_owned()),
            state: PluginWireStateView::Running,
            recent_exits: vec![],
            reason: None,
        };
        let lines = render_info(&en(), &base);
        let joined = lines.join("\n");
        assert!(joined.contains("Plugin: <gold>greeting<reset>"), "{joined}");
        assert!(
            joined.contains("Description: says hello on startup"),
            "{joined}"
        );
        assert!(joined.contains("Author: Jane Doe"), "{joined}");
        assert!(
            joined.contains("Entry point: candyriya_plugin_init"),
            "{joined}"
        );
        assert!(joined.contains("State: running"), "{joined}");
        assert!(joined.contains("Recent exits: -"), "{joined}");
        assert!(!joined.contains("Reason:"), "{joined}");

        let latched = PluginInfo {
            state: PluginWireStateView::Unloaded,
            reason: Some("ABI handshake failed".to_owned()),
            recent_exits: vec![PsExitStatus::Failure("boom".to_owned())],
            ..base
        };
        let lines = render_info(&en(), &latched);
        let joined = lines.join("\n");
        assert!(joined.contains("Reason: ABI handshake failed"), "{joined}");
        assert!(joined.contains("failure(boom)"), "{joined}");
    }

    #[test]
    fn tree_renders_the_contract_shape() {
        let rows = vec![
            row(
                7,
                1,
                "plugins",
                PluginTreeNodeState::Unit(PsUnitState::Running),
            ),
            row(
                8,
                7,
                "plugin-controller",
                PluginTreeNodeState::Unit(PsUnitState::Running),
            ),
            row(
                9,
                8,
                "plugin-greeting",
                PluginTreeNodeState::Wire(PluginWireStateView::Running),
            ),
            row(
                10,
                8,
                "plugin-quests",
                PluginTreeNodeState::Wire(PluginWireStateView::Disabled),
            ),
        ];
        let lines = render_tree(&en(), &rows);
        assert_eq!(
            lines,
            vec![
                "plugins <dark_gray>[pid 7, ppid 1, running]<reset>".to_string(),
                "└── plugin-controller <dark_gray>[pid 8, ppid 7, running]<reset>".to_string(),
                "    ├── plugin-greeting <dark_gray>[pid 9, ppid 8, running]<reset>".to_string(),
                "    └── plugin-quests <dark_gray>[pid 10, ppid 8, disabled]<reset>".to_string(),
            ]
        );
    }

    #[test]
    fn tree_with_only_the_group_prints_one_bare_line() {
        let rows = vec![row(
            7,
            1,
            "plugins",
            PluginTreeNodeState::Unit(PsUnitState::Running),
        )];
        let lines = render_tree(&en(), &rows);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(!lines[0].contains("──"), "no guides: {lines:?}");
        assert!(lines[0].contains("plugins"), "{lines:?}");
    }

    #[test]
    fn tree_with_zero_rows_reports_none_found() {
        let lines = render_tree(&en(), &[]);
        assert_eq!(lines, vec!["No plugins found.".to_string()]);
    }

    #[test]
    fn tree_promotes_orphaned_rows_instead_of_dropping_them() {
        let rows = vec![row(
            42,
            999,
            "plugin-ghost",
            PluginTreeNodeState::Wire(PluginWireStateView::Discovered),
        )];
        let lines = render_tree(&en(), &rows);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("plugin-ghost"), "{lines:?}");
    }

    #[test]
    fn plugin_view_prefix_wraps_a_dyn_view() {
        let view: Arc<dyn PluginView> = Arc::new(CannedView);
        let prefix = PluginViewPrefix::from_arc(view);
        assert!(prefix.list().is_success());
        assert!(prefix.enable("greeting").is_success());
    }

    #[test]
    fn every_new_plugin_key_resolves_in_both_locales() {
        const KEYS: &[&str] = &[
            "candyriya.plugin.col.id",
            "candyriya.plugin.col.outcome",
            "candyriya.plugin.col.restart_elected",
            "candyriya.plugin.col.source",
            "candyriya.plugin.col.state",
            "candyriya.plugin.col.version",
            "candyriya.plugin.disable_ok",
            "candyriya.plugin.enable_ok",
            "candyriya.plugin.failed",
            "candyriya.plugin.info.author",
            "candyriya.plugin.info.description",
            "candyriya.plugin.info.entry_point",
            "candyriya.plugin.info.plugin",
            "candyriya.plugin.info.reason",
            "candyriya.plugin.info.recent_exits",
            "candyriya.plugin.info.state",
            "candyriya.plugin.info.version",
            "candyriya.plugin.none_found",
            "candyriya.plugin.reload_ok",
            "candyriya.plugin.restart.no",
            "candyriya.plugin.restart.yes",
            "candyriya.plugin.unavailable",
            "candyriya.plugin.unload_detached",
            "candyriya.plugin.unload_ok",
            "candyriya.plugin.unknown",
        ];
        for locale in [en(), ex_girl()] {
            for key in KEYS {
                let value = locale.t(key, &[]);
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

    #[test]
    fn minimessage_is_escaped_in_rendered_cells() {
        let mut hostile = entry("g<r>eeding", PluginWireStateView::Running);
        hostile.source = "plugins/g<r>eeding/x<reset>.dll".to_owned();
        let lines = render_list(&en(), &[hostile]);
        let joined = lines.join("\n");
        assert!(joined.contains("g‹r›eeding"), "{joined}");
        assert!(!joined.contains("<r>"), "{joined}");
    }
}
