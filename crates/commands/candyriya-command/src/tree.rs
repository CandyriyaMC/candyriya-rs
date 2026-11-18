//! `/candyriya tree`: the supervision tree as pure box-drawing lines.
//!
//! No table, no columns — one line per unit, indented by depth with
//! `├──` / `└──` guides and `│` continuation stems. The line keeps the
//! `──` branch format at any width; only the trailing caption is allowed
//! to flow. Parenthetical captions carry exactly two facts the operator
//! cannot see otherwise — the unit's kind and (when present) its last
//! exit — plus a `frozen` marker for a parked body. Names are printed as
//! recorded by the tree, escaped for MiniMessage.
//!
//! Units whose parent is not in the snapshot are promoted to top level
//! instead of being dropped: real units must never vanish from an
//! operator view.

use std::collections::BTreeMap;

use candyriya_i18n::I18n;

use crate::ps::{PsSnapshot, PsUnit, PsUnitKind, exit_text, state_open};
use crate::text::escape_minimessage;

/// Render the tree to MiniMessage lines.
pub(crate) fn render_tree(snapshot: &PsSnapshot, t: &I18n) -> Vec<String> {
    if snapshot.units.is_empty() {
        return vec![t.t("candyriya.ps.empty", &[])];
    }

    let known: std::collections::HashSet<u64> =
        snapshot.units.iter().map(|unit| unit.pid).collect();
    let mut children: BTreeMap<u64, Vec<&PsUnit>> = BTreeMap::new();
    for unit in &snapshot.units {
        children.entry(unit.ppid).or_default().push(unit);
    }
    for kids in children.values_mut() {
        kids.sort_by_key(|unit| unit.pid);
    }

    // the pid-1 root first, then any promoted orphans (parent not in the
    // snapshot), each by pid
    let mut roots: Vec<&PsUnit> = snapshot
        .units
        .iter()
        .filter(|unit| unit.ppid == 0 || !known.contains(&unit.ppid))
        .collect();
    roots.sort_by_key(|unit| (unit.pid != 1, unit.pid));

    let mut lines = Vec::with_capacity(snapshot.units.len());
    let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let mut ctx = Ctx {
        children: &children,
        seen: &mut seen,
        t,
    };
    for root in roots {
        tree_node(root, "", true, true, &mut ctx, &mut lines);
    }
    // The kernel never emits a ppid cycle (pids strictly increase downward),
    // but the DTO makes no such promise — and real units must not vanish.
    // Anything not reached from a root is promoted to top level, by pid.
    let mut promoted: Vec<&PsUnit> = snapshot
        .units
        .iter()
        .filter(|unit| !ctx.seen.contains(&unit.pid))
        .collect();
    promoted.sort_by_key(|unit| unit.pid);
    for unit in promoted {
        if ctx.seen.contains(&unit.pid) {
            // it was reached from another promoted subtree after the list
            // was built
            continue;
        }
        tree_node(unit, "", true, true, &mut ctx, &mut lines);
    }
    lines
}

/// Shared walker state, so the recursion stays small.
struct Ctx<'a> {
    children: &'a BTreeMap<u64, Vec<&'a PsUnit>>,
    seen: &'a mut std::collections::HashSet<u64>,
    t: &'a I18n,
}

fn tree_node(
    unit: &PsUnit,
    prefix: &str,
    is_tail: bool,
    is_top: bool,
    ctx: &mut Ctx<'_>,
    lines: &mut Vec<String>,
) {
    ctx.seen.insert(unit.pid);
    let guide = if is_top {
        ""
    } else if is_tail {
        "└── "
    } else {
        "├── "
    };
    lines.push(format!("{prefix}{guide}{}", caption(unit, ctx.t)));

    let child_prefix = if is_top {
        prefix.to_string()
    } else if is_tail {
        format!("{prefix}    ")
    } else {
        format!("{prefix}│   ")
    };

    // kids already claimed by an earlier DFS branch are skipped; the guide
    // for the remaining ones is computed on the surviving set
    let kids: Vec<&PsUnit> = ctx
        .children
        .get(&unit.pid)
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

/// The `name (kind[, frozen][, exit])` caption for one line.
fn caption(unit: &PsUnit, t: &I18n) -> String {
    let name = format!(
        "{}{}<reset>",
        state_open(unit.state, unit.parked),
        escape_minimessage(&unit.name)
    );
    let mut parts: Vec<String> = Vec::with_capacity(3);
    parts.push(kind_text(t, unit.kind));
    if unit.parked {
        parts.push(t.t("candyriya.tree.frozen", &[]));
    }
    if let Some(status) = &unit.last_exit {
        parts.push(t.t(
            "candyriya.tree.exit",
            &[("status", exit_text(t, status).as_str())],
        ));
    }
    format!("{name} <dark_gray>({})<reset>", parts.join(", "))
}

fn kind_text(t: &I18n, kind: PsUnitKind) -> String {
    let key = match kind {
        PsUnitKind::Oneshot => "candyriya.kind.oneshot",
        PsUnitKind::LongLived => "candyriya.kind.long_lived",
        PsUnitKind::Every => "candyriya.kind.every",
        PsUnitKind::Connection => "candyriya.kind.connection",
        PsUnitKind::Plugin => "candyriya.kind.plugin",
    };
    t.t(key, &[])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ps::PsUnitState;
    use std::time::Instant;

    fn en() -> I18n {
        I18n::new("en").expect("embedded en locale")
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

    fn snapshot(units: Vec<PsUnit>) -> PsSnapshot {
        PsSnapshot {
            units,
            conns: vec![],
            zombies: 0,
            stuck_zombies: 0,
        }
    }

    #[test]
    fn nested_tree_uses_box_drawing_guides() {
        let snap = snapshot(vec![
            unit(1, 0, "init", PsUnitKind::LongLived),
            unit(2, 1, "a", PsUnitKind::LongLived),
            unit(3, 2, "b", PsUnitKind::Connection),
            unit(4, 2, "c", PsUnitKind::Connection),
            unit(5, 1, "d", PsUnitKind::Plugin),
        ]);
        let lines = render_tree(&snap, &en());
        assert_eq!(lines.len(), 5, "{lines:?}");
        assert!(
            lines[0].contains("init") && !lines[0].contains("──"),
            "root carries no guide: {lines:?}"
        );
        assert!(
            lines[1].starts_with("├── ") && lines[1].contains("a"),
            "{lines:?}"
        );
        assert!(
            lines[2].starts_with("│   ├── ") && lines[2].contains("b"),
            "{lines:?}"
        );
        assert!(
            lines[3].starts_with("│   └── ") && lines[3].contains("c"),
            "{lines:?}"
        );
        assert!(
            lines[4].starts_with("└── ") && lines[4].contains("d"),
            "{lines:?}"
        );
    }

    #[test]
    fn unit_with_missing_parent_is_promoted_to_top_level() {
        let snap = snapshot(vec![
            unit(1, 0, "init", PsUnitKind::LongLived),
            unit(7, 99, "ghost", PsUnitKind::Oneshot),
        ]);
        let lines = render_tree(&snap, &en());
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(
            lines[1].contains("ghost"),
            "promoted, not dropped: {lines:?}"
        );
        assert!(
            !lines[1].contains("──"),
            "top-level carries no guide: {lines:?}"
        );
    }

    #[test]
    fn parked_units_are_marked_frozen() {
        let mut frozen = unit(3, 1, "conn-frozen", PsUnitKind::Connection);
        frozen.parked = true;
        let snap = snapshot(vec![unit(1, 0, "init", PsUnitKind::LongLived), frozen]);
        let lines = render_tree(&snap, &en());
        assert!(lines[1].contains("frozen"), "{lines:?}");
    }

    #[test]
    fn failed_terminal_units_show_their_exit() {
        let mut dead = unit(3, 1, "plugin-dead", PsUnitKind::Plugin);
        dead.state = PsUnitState::FailedTerminal;
        dead.last_exit = Some(crate::ps::PsExitStatus::Failure("boom".to_owned()));
        let snap = snapshot(vec![unit(1, 0, "init", PsUnitKind::LongLived), dead]);
        let lines = render_tree(&snap, &en());
        assert!(lines[1].contains("exit:"), "{lines:?}");
        assert!(lines[1].contains("failure(boom)"), "{lines:?}");
    }

    #[test]
    fn empty_tree_renders_the_empty_state() {
        let snap = snapshot(vec![]);
        let lines = render_tree(&snap, &en());
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("No units"), "{lines:?}");
    }

    #[test]
    fn ppid_cycle_units_are_still_printed() {
        // the kernel can never produce this, but the DTO makes no promises:
        // even a cyclic snapshot must not silently swallow units
        let snap = snapshot(vec![
            unit(1, 0, "init", PsUnitKind::LongLived),
            unit(10, 11, "cycle-a", PsUnitKind::Oneshot),
            unit(11, 10, "cycle-b", PsUnitKind::Oneshot),
        ]);
        let lines = render_tree(&snap, &en());
        let joined = lines.join("\n");
        assert!(joined.contains("cycle-a"), "{lines:?}");
        assert!(joined.contains("cycle-b"), "{lines:?}");
        assert_eq!(lines.len(), 3, "{lines:?}");
    }

    #[test]
    fn children_render_ascending_pid() {
        let snap = snapshot(vec![
            unit(1, 0, "init", PsUnitKind::LongLived),
            unit(9, 1, "i-unit", PsUnitKind::LongLived),
            unit(3, 1, "c-unit", PsUnitKind::LongLived),
            unit(7, 1, "g-unit", PsUnitKind::LongLived),
        ]);
        let lines = render_tree(&snap, &en());
        let order: Vec<&str> = lines
            .iter()
            .map(|line| {
                if line.contains("c-unit") {
                    "c"
                } else if line.contains("g-unit") {
                    "g"
                } else if line.contains("i-unit") {
                    "i"
                } else {
                    "root"
                }
            })
            .collect();
        assert_eq!(order, vec!["root", "c", "g", "i"], "{lines:?}");
    }
}
