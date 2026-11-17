//! The dependency resolver: dependency-order loading for the discovered
//! plugin set.
//!
//! The loader collects every plugin it finds ([`LoadReport::plugins`]) in a
//! stable discovery order — libraries first, then plugins, alphabetical
//! within each root. This module turns that set into an actual *load order*:
//!
//! - Hard dependencies (`depend` in `plugin.toml`) must load first. A missing
//!   hard dependency drops the *requesting* plugin; everything that
//!   hard-depends on a dropped plugin is dropped too, recursively.
//! - Soft dependencies (`softdepend`) load first as well, but only when the
//!   companion is present. A missing companion never blocks the consumer —
//!   it just means the optional feature stays off.
//! - Duplicated ids are a loader-level bug detector: the loader already
//!   drops cross-root duplicates, but the provided set is checked again
//!   before resolution.
//! - Cycles are reported as a [`PluginError::DependencyCycle`] whose
//!   `diagnosis` lines name the cycle members, e.g.
//!   `` `a` depends on `b`; `b` depends on `a` ``.
//!
//! # Libraries before plugins
//!
//! Among plugins with no dependency edges between them, libraries sort
//! before plugins and alphabetical order wins inside each group. The
//! topological sort uses exactly this as its tie-break whenever several
//! nodes are ready to load.
//!
//! # Entry points
//!
//! - [`resolve_plugins`] is the full view: survivors in load order, every
//!   issue encountered, per-plugin soft dependency outcomes and the
//!   `[api].exports` table per plugin. Drops for missing hard dependencies
//!   are *issues*, not an error.
//! - [`resolve_plugin_order`] is the minimal view: just the ordered set. It
//!   fails the whole call when any hard dependency cannot be satisfied
//!   (the error carries every failure plus the loadable plugins), and on
//!   duplicates or cycles. Soft dependency misses are not fatal here.
//!
//! Plugins in [`crate::PluginState::Failed`] state take part in ordering
//! like any other record: resolution is about *presence* of the companion,
//! not its health. Callers decide what to invoke on the records they get.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::error::PluginError;
use crate::library_plugin::{LibraryPlugin, PluginOrigin};
use crate::manifest::validate_unique;
use crate::plugin_loader::{LoadIssue, LoadReport};

/// One plugin dropped from the load order because a hard dependency cannot
/// be satisfied.
///
/// Failures are collected, not reported one at a time: a single resolution
/// pass lists every dropped plugin, so operators fix everything at once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyFailure {
    /// Id of the plugin that cannot load.
    pub plugin: String,
    /// Id of the dependency that is missing (or that was dropped earlier in
    /// this pass).
    pub missing: String,
    /// Where the dropped plugin lives (its `<dir>/<id>/` directory).
    pub path: PathBuf,
}

impl std::fmt::Display for DependencyFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "plugin `{}` cannot load: missing hard dependency `{}`",
            self.plugin, self.missing
        )
    }
}

/// A declared soft dependency and whether the companion was found.
///
/// `present == false` means the consumer loaded anyway and must keep the
/// optional feature off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoftDependency {
    /// The companion id as written in `softdepend`.
    pub companion: String,
    /// Whether the companion is in the discovered set (and was not dropped).
    pub present: bool,
}

/// The outcome of resolving the dependency order of a discovered plugin set.
///
/// Nothing here is fatal: drops and missing optional companions are reported
/// through [`LoadReport::issues`], as `(where, why)` tuples, the same
/// channel the loader uses.
pub struct DependencyResolution {
    /// Survivors in load order + every issue found along the way.
    pub report: LoadReport,
    /// Per-survivor soft dependency outcomes, keyed by plugin id. One entry
    /// per declared `softdepend`, in declaration order. Used by the proxy to
    /// gate optional features.
    pub soft_deps: BTreeMap<String, Vec<SoftDependency>>,
    /// Per-survivor `[api].exports` tables, keyed by plugin id. Maps a
    /// plugin id to the export names other plugins may call through
    /// `call_api`. The names themselves were already validated by
    /// [`candyriya_plugin_api::PluginMetadata::validate`]; this is the index
    /// the call_api handler consults.
    pub exports: BTreeMap<String, Vec<String>>,
}

/// Check the provided set for duplicate ids.
///
/// The loader already resolves cross-root collisions (libraries win, plugins
/// with a colliding id are dropped and reported), so by the time a set
/// reaches this module duplicates mean someone built the input by hand.
/// Every duplicate group is reported, not just the first.
pub fn validate_set(plugins: &[LibraryPlugin]) -> Result<(), PluginError> {
    let set: Vec<(&str, &Path)> = plugins.iter().map(|p| (p.id(), p.dir())).collect();
    validate_unique(set).map_err(PluginError::DuplicatePluginIds)
}

/// Resolve the load order of the discovered set, collecting issues.
///
/// Never fails on missing dependencies — affected plugins are dropped and
/// listed in [`DependencyResolution::report`]'s issues. Fails as a whole
/// only when the set itself cannot be ordered: duplicate ids or dependency
/// cycles.
pub fn resolve_plugins(plugins: Vec<LibraryPlugin>) -> Result<DependencyResolution, PluginError> {
    let Resolution {
        ordered,
        issues,
        soft_deps,
        exports,
        ..
    } = resolve_inner(plugins)?;
    Ok(DependencyResolution {
        report: LoadReport {
            plugins: ordered,
            issues,
        },
        soft_deps,
        exports,
    })
}

/// Resolve the load order of the discovered set.
///
/// # Errors
///
/// - [`PluginError::DuplicatePluginIds`] — the set contains a duplicate id.
/// - [`PluginError::DependencyCycle`] — a dependency cycle, with a
///   `diagnosis` naming the cycle members.
/// - [`PluginError::UnresolvedDependencies`] — at least one plugin cannot
///   load because a hard dependency is missing. The error carries *every*
///   such failure plus the plugins that do load, in load order.
///
/// Missing *soft* dependencies are not errors: the consumer is part of the
/// returned set either way. Use [`resolve_plugins`] when you need the full
/// outcome (issues, soft dependency flags, `[api].exports` index).
pub fn resolve_plugin_order(
    plugins: Vec<LibraryPlugin>,
) -> Result<Vec<LibraryPlugin>, PluginError> {
    let Resolution {
        ordered,
        issues,
        failures,
        ..
    } = resolve_inner(plugins)?;
    if failures.is_empty() {
        Ok(ordered)
    } else {
        Err(PluginError::UnresolvedDependencies {
            failures,
            issues,
            loadable: ordered,
        })
    }
}

/// Internal outcome of one resolution pass.
struct Resolution {
    ordered: Vec<LibraryPlugin>,
    issues: Vec<LoadIssue>,
    soft_deps: BTreeMap<String, Vec<SoftDependency>>,
    exports: BTreeMap<String, Vec<String>>,
    failures: Vec<DependencyFailure>,
}

/// Libraries sort before plugins; alphabetical within a group. Used as the
/// topological tie-break so unrelated plugins keep the loader's documented
/// order.
fn origin_rank(origin: PluginOrigin) -> u8 {
    match origin {
        PluginOrigin::Library => 0,
        PluginOrigin::Plugin => 1,
    }
}

struct DepEdge {
    /// The dependency id as written in the manifest.
    id: String,
    /// Index of the target in the set, when the target was discovered.
    target: Option<usize>,
    /// `true` for `softdepend`, `false` for `depend`.
    soft: bool,
}

fn collect_deps(plugins: &[LibraryPlugin]) -> (Vec<Vec<DepEdge>>, HashMap<&str, usize>) {
    let mut index: HashMap<&str, usize> = HashMap::with_capacity(plugins.len());
    for (i, plugin) in plugins.iter().enumerate() {
        index.insert(plugin.id(), i);
    }

    let mut deps = Vec::with_capacity(plugins.len());
    for plugin in plugins {
        let metadata = plugin.manifest().metadata();
        let mut edges = Vec::with_capacity(metadata.depend.len() + metadata.softdepend.len());
        for dep in &metadata.depend {
            edges.push(DepEdge {
                id: dep.clone(),
                target: index.get(dep.as_str()).copied(),
                soft: false,
            });
        }
        for dep in &metadata.softdepend {
            edges.push(DepEdge {
                id: dep.clone(),
                target: index.get(dep.as_str()).copied(),
                soft: true,
            });
        }
        deps.push(edges);
    }
    (deps, index)
}

/// Mark every plugin whose hard dependency is unsatisfiable as dropped.
///
/// A hard dependency is unsatisfiable when the target is not in the set at
/// all, or when the target was itself dropped in this pass. Iterates to a
/// fixpoint so chains (`c` gone -> `b` dropped -> `a` dropped) all surface.
/// Returns the failures, in drop order.
fn drop_unsatisfiable(
    plugins: &[LibraryPlugin],
    deps: &[Vec<DepEdge>],
) -> (Vec<bool>, Vec<DependencyFailure>, Vec<LoadIssue>) {
    let mut dropped = vec![false; plugins.len()];
    let mut failures = Vec::new();
    let mut issues = Vec::new();

    loop {
        let mut changed = false;
        for i in 0..plugins.len() {
            if dropped[i] {
                continue;
            }
            for edge in &deps[i] {
                if edge.soft {
                    continue;
                }
                let missing = match edge.target {
                    None => Some(edge.id.clone()),
                    Some(target) if dropped[target] => Some(plugins[target].id().to_owned()),
                    Some(_) => None,
                };
                if let Some(missing) = missing {
                    dropped[i] = true;
                    failures.push(DependencyFailure {
                        plugin: plugins[i].id().to_owned(),
                        missing: missing.clone(),
                        path: plugins[i].dir().to_path_buf(),
                    });
                    issues.push((
                        plugins[i].dir().to_path_buf(),
                        format!(
                            "plugin `{}` cannot load: missing hard dependency `{missing}`",
                            plugins[i].id()
                        ),
                    ));
                    changed = true;
                    break;
                }
            }
        }
        if !changed {
            return (dropped, failures, issues);
        }
    }
}

/// Kahn's topological sort over the survivors. Returns the ordered
/// survivor indices, or the diagnosis when a cycle is found.
///
/// All edges (hard and soft) count for ordering: a present soft companion
/// must load before its consumer. Edges into dropped targets are ignored —
/// for soft deps that means "companion absent", for hard deps the dependent
/// is already dropped.
fn topo_sort(
    plugins: &[LibraryPlugin],
    deps: &[Vec<DepEdge>],
    dropped: &[bool],
) -> Result<Vec<usize>, Vec<String>> {
    let n = plugins.len();
    let mut indegree = vec![0usize; n];
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); n];

    for i in 0..n {
        if dropped[i] {
            continue;
        }
        for edge in &deps[i] {
            let Some(target) = edge.target else {
                continue;
            };
            if dropped[target] {
                continue;
            }
            indegree[i] += 1;
            dependents[target].push(i);
        }
    }

    let mut order = Vec::with_capacity(n);
    let mut remaining: Vec<usize> = (0..n).filter(|&i| !dropped[i]).collect();

    let key = |i: usize| (origin_rank(plugins[i].origin()), plugins[i].id());
    loop {
        // pick the ready node with the smallest (origin, id) tie-break
        let mut best: Option<usize> = None;
        for &i in &remaining {
            if indegree[i] != 0 {
                continue;
            }
            let better = match best {
                None => true,
                Some(b) => key(i) < key(b),
            };
            if better {
                best = Some(i);
            }
        }
        let Some(node) = best else { break };
        let pos = remaining
            .iter()
            .position(|&n| n == node)
            .expect("a ready node is in `remaining`");
        remaining.remove(pos);
        order.push(node);
        for &dependent in &dependents[node] {
            indegree[dependent] -= 1;
        }
    }

    if remaining.is_empty() {
        return Ok(order);
    }
    Err(diagnose_cycles(plugins, deps, &remaining))
}

/// Extract a human-readable diagnosis for the nodes Kahn's algorithm could
/// not order: name every cycle found, plus the downstream nodes blocked by
/// it. Each diagnosis line reads `a` depends on `b`; `b` depends on `a`.
fn diagnose_cycles(
    plugins: &[LibraryPlugin],
    deps: &[Vec<DepEdge>],
    remaining: &[usize],
) -> Vec<String> {
    // depends-on adjacency restricted to the remaining set, with original
    // node indices mapped to local indices
    let to_local: HashMap<usize, usize> = remaining
        .iter()
        .enumerate()
        .map(|(local, &node)| (node, local))
        .collect();
    let depends_on: Vec<Vec<(usize, bool)>> = remaining
        .iter()
        .map(|&i| {
            deps[i]
                .iter()
                .filter_map(|e| {
                    let target = e.target?;
                    to_local.get(&target).map(|&local| (local, e.soft))
                })
                .collect()
        })
        .collect();

    const WHITE: u8 = 0;
    const GRAY: u8 = 1;
    const BLACK: u8 = 2;
    let mut state = vec![WHITE; remaining.len()];
    let mut stack: Vec<usize> = Vec::with_capacity(remaining.len());
    let mut cycle_keys: HashSet<Vec<usize>> = HashSet::new();
    let mut cycles: Vec<Vec<usize>> = Vec::new();

    fn visit(
        local: usize,
        depends_on: &[Vec<(usize, bool)>],
        state: &mut [u8],
        stack: &mut Vec<usize>,
        cycle_keys: &mut HashSet<Vec<usize>>,
        cycles: &mut Vec<Vec<usize>>,
    ) {
        state[local] = GRAY;
        stack.push(local);
        for &(dep_local, _) in &depends_on[local] {
            match state[dep_local] {
                WHITE => visit(dep_local, depends_on, state, stack, cycle_keys, cycles),
                GRAY => {
                    // `local` walks into a node already on the stack:
                    // everything from that node up is one cycle
                    let start = stack
                        .iter()
                        .position(|&s| s == dep_local)
                        .expect("a gray node is on the stack");
                    let cycle: Vec<usize> = stack[start..].to_vec();
                    // normalize rotation so the same cycle yields one key
                    let pivot = (0..cycle.len())
                        .min_by_key(|&k| cycle[k])
                        .expect("a cycle is non-empty");
                    let mut normalized: Vec<usize> = cycle[pivot..]
                        .iter()
                        .chain(cycle[..pivot].iter())
                        .copied()
                        .collect();
                    if cycle_keys.insert(std::mem::take(&mut normalized)) {
                        cycles.push(cycle);
                    }
                }
                _ => {}
            }
        }
        stack.pop();
        state[local] = BLACK;
    }

    for local in 0..remaining.len() {
        if state[local] == WHITE {
            visit(
                local,
                &depends_on,
                &mut state,
                &mut stack,
                &mut cycle_keys,
                &mut cycles,
            );
        }
    }

    // render each cycle as `a` depends on `b`; `b` depends on `c`; ...
    let mut diagnosis: Vec<String> = Vec::with_capacity(cycles.len() + 1);
    let mut members: HashSet<usize> = HashSet::new();
    for cycle in &cycles {
        let mut parts = Vec::with_capacity(cycle.len());
        for (k, &local) in cycle.iter().enumerate() {
            let next_local = cycle[(k + 1) % cycle.len()];
            let soft = depends_on[local]
                .iter()
                .find(|&&(node, _)| node == next_local)
                .map(|&(_, soft)| soft)
                .unwrap_or(false);
            let word = if soft { "soft-depends" } else { "depends" };
            parts.push(format!(
                "`{}` {word} on `{}`",
                plugins[remaining[local]].id(),
                plugins[remaining[next_local]].id()
            ));
            members.insert(local);
        }
        diagnosis.push(parts.join("; "));
    }

    // survivors blocked by a cycle (hard/soft-downstream of cycle members)
    // are named so the diagnosis is complete
    let blocked: Vec<&str> = remaining
        .iter()
        .enumerate()
        .filter(|(local, _)| !members.contains(local))
        .map(|(_, &node)| plugins[node].id())
        .collect();
    if !blocked.is_empty() {
        diagnosis.push(format!(
            "blocked by the cycle(s) above: {}",
            blocked
                .iter()
                .map(|id| format!("`{id}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    diagnosis
}

fn resolve_inner(plugins: Vec<LibraryPlugin>) -> Result<Resolution, PluginError> {
    validate_set(&plugins)?;

    let (deps, _index) = collect_deps(&plugins);
    let (dropped, failures, mut issues) = drop_unsatisfiable(&plugins, &deps);

    // soft dependency outcomes for the survivors (the dropped ones have no
    // meaningful "companion found?" answer — they never run)
    let mut soft_deps: BTreeMap<String, Vec<SoftDependency>> = BTreeMap::new();
    for (i, plugin) in plugins.iter().enumerate() {
        if dropped[i] {
            continue;
        }
        let outcomes: Vec<SoftDependency> = deps[i]
            .iter()
            .filter(|e| e.soft)
            .map(|e| {
                let present = e.target.is_some_and(|t| !dropped[t]);
                if !present {
                    issues.push((
                        plugin.dir().to_path_buf(),
                        format!(
                            "soft dependency `{}` of plugin `{}` was not found; optional features stay off",
                            e.id,
                            plugin.id()
                        ),
                    ));
                }
                SoftDependency {
                    companion: e.id.clone(),
                    present,
                }
            })
            .collect();
        if !outcomes.is_empty() {
            soft_deps.insert(plugin.id().to_owned(), outcomes);
        }
    }

    let order = topo_sort(&plugins, &deps, &dropped)
        .map_err(|diagnosis| PluginError::DependencyCycle { diagnosis })?;

    // consume the records in pop order
    let mut slots: Vec<Option<LibraryPlugin>> = plugins.into_iter().map(Some).collect();
    let mut ordered = Vec::with_capacity(order.len());
    let mut exports: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for idx in order {
        let plugin = slots[idx].take().expect("each survivor is taken once");
        let export_names: Vec<String> = plugin
            .manifest()
            .metadata()
            .api
            .exports
            .iter()
            .map(|export| export.name.clone())
            .collect();
        if !export_names.is_empty() {
            exports.insert(plugin.id().to_owned(), export_names);
        }
        ordered.push(plugin);
    }

    Ok(Resolution {
        ordered,
        issues,
        soft_deps,
        exports,
        failures,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::PluginManifest;
    use std::path::PathBuf;

    /// A plugin record with no wasm behind it (Failed state) — enough for
    /// resolution, which only cares about presence, manifests and origins.
    fn fake_plugin(
        id: &str,
        origin: PluginOrigin,
        depend: &[&str],
        softdepend: &[&str],
        exports: &[&str],
    ) -> LibraryPlugin {
        let depend = depend
            .iter()
            .map(|d| format!("\"{d}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let softdepend = softdepend
            .iter()
            .map(|d| format!("\"{d}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let exports = exports
            .iter()
            .map(|e| format!("{{ name = \"{e}\" }}"))
            .collect::<Vec<_>>()
            .join(", ");
        let toml = format!(
            "id = \"{id}\"\n\
             version = \"0.1.0\"\n\
             name = \"Test {id}\"\n\
             plugin_main = \"greet\"\n\
             plugin_api = \"0.1\"\n\
             depend = [{depend}]\n\
             softdepend = [{softdepend}]\n\
             [api]\n\
             exports = [{exports}]\n"
        );
        let manifest = PluginManifest::from_toml(&toml).expect("fake manifest is valid");
        LibraryPlugin::failed(
            PathBuf::from(format!("/fake/{id}")),
            manifest,
            origin,
            "test record, no wasm".to_owned(),
        )
    }

    fn ids(plugins: &[LibraryPlugin]) -> Vec<&str> {
        plugins.iter().map(|p| p.id()).collect()
    }

    fn resolve(plugins: Vec<LibraryPlugin>) -> DependencyResolution {
        resolve_plugins(plugins).expect("resolution succeeds")
    }

    #[test]
    fn simple_chain_loads_in_dependency_order() {
        let plugins = vec![
            fake_plugin("a", PluginOrigin::Plugin, &["b"], &[], &[]),
            fake_plugin("b", PluginOrigin::Plugin, &["c"], &[], &[]),
            fake_plugin("c", PluginOrigin::Plugin, &[], &[], &[]),
        ];
        let resolution = resolve(plugins);
        assert_eq!(ids(&resolution.report.plugins), ["c", "b", "a"]);
        assert!(resolution.report.issues.is_empty());
    }

    #[test]
    fn missing_hard_depend_skips_plugin_and_reports_all() {
        let build = || {
            vec![
                fake_plugin("good", PluginOrigin::Plugin, &[], &[], &[]),
                fake_plugin("broken1", PluginOrigin::Plugin, &["absent1"], &[], &[]),
                fake_plugin("also-good", PluginOrigin::Plugin, &[], &[], &[]),
                fake_plugin("broken2", PluginOrigin::Plugin, &["absent2"], &[], &[]),
            ]
        };

        // every failure lands in one report, nothing is stopped at the first
        match resolve_plugin_order(build()) {
            Err(PluginError::UnresolvedDependencies {
                failures, loadable, ..
            }) => {
                let missing: Vec<&str> = failures.iter().map(|f| f.missing.as_str()).collect();
                assert!(missing.contains(&"absent1"));
                assert!(missing.contains(&"absent2"));
                assert_eq!(failures.len(), 2);
                assert_eq!(ids(&loadable), ["also-good", "good"]);
            }
            Err(e) => panic!("expected UnresolvedDependencies, got {e}"),
            Ok(_) => panic!("expected UnresolvedDependencies, got Ok(..)"),
        }

        // the keep-going view: survivors load, every drop is an issue
        let resolution = resolve(build());
        assert_eq!(ids(&resolution.report.plugins), ["also-good", "good"]);
        let mentioned: Vec<String> = resolution
            .report
            .issues
            .iter()
            .map(|(_, why)| why.clone())
            .collect();
        assert!(mentioned.iter().any(|w| w.contains("absent1")));
        assert!(mentioned.iter().any(|w| w.contains("absent2")));
    }

    #[test]
    fn hard_dependents_of_dropped_plugins_are_dropped_too() {
        let plugins = vec![
            fake_plugin("a", PluginOrigin::Plugin, &["b"], &[], &[]),
            fake_plugin("b", PluginOrigin::Plugin, &["absent"], &[], &[]),
            fake_plugin("independent", PluginOrigin::Plugin, &[], &[], &[]),
        ];
        let resolution = resolve(plugins);
        assert_eq!(ids(&resolution.report.plugins), ["independent"]);
        let mentioned: Vec<String> = resolution
            .report
            .issues
            .iter()
            .map(|(_, why)| why.clone())
            .collect();
        assert!(mentioned.iter().any(|w| w.contains("`b`")));
        // `a` is dropped because its hard dependency `b` was dropped
        assert!(
            mentioned
                .iter()
                .any(|w| w.contains("`a`") && w.contains("`b`"))
        );
    }

    #[test]
    fn cycle_between_two_plugins_is_reported() {
        let plugins = vec![
            fake_plugin("a", PluginOrigin::Plugin, &["b"], &[], &[]),
            fake_plugin("b", PluginOrigin::Plugin, &["a"], &[], &[]),
        ];
        match resolve_plugin_order(plugins) {
            Err(PluginError::DependencyCycle { diagnosis }) => {
                assert!(
                    diagnosis
                        .iter()
                        .any(|line| line.contains('a') && line.contains('b'))
                );
            }
            Err(e) => panic!("expected DependencyCycle, got {e}"),
            Ok(_) => panic!("expected DependencyCycle, got Ok(..)"),
        }
    }

    #[test]
    fn cycle_through_three_plugins_is_reported() {
        let plugins = vec![
            fake_plugin("a", PluginOrigin::Plugin, &["b"], &[], &[]),
            fake_plugin("b", PluginOrigin::Plugin, &["c"], &[], &[]),
            fake_plugin("c", PluginOrigin::Plugin, &["a"], &[], &[]),
        ];
        match resolve_plugin_order(plugins) {
            Err(PluginError::DependencyCycle { diagnosis }) => {
                let joined = diagnosis.join("\n");
                assert!(joined.contains('a') && joined.contains('b') && joined.contains('c'));
            }
            Err(e) => panic!("expected DependencyCycle, got {e}"),
            Ok(_) => panic!("expected DependencyCycle, got Ok(..)"),
        }
    }

    #[test]
    fn self_cycle_is_reported() {
        let plugins = vec![fake_plugin("a", PluginOrigin::Plugin, &["a"], &[], &[])];
        match resolve_plugin_order(plugins) {
            Err(PluginError::DependencyCycle { diagnosis }) => {
                assert!(
                    diagnosis
                        .iter()
                        .any(|line| line.contains("`a` depends on `a`"))
                );
            }
            Err(e) => panic!("expected DependencyCycle, got {e}"),
            Ok(_) => panic!("expected DependencyCycle, got Ok(..)"),
        }
    }

    #[test]
    fn soft_depend_present_loads_before_the_consumer() {
        let plugins = vec![
            fake_plugin("consumer", PluginOrigin::Plugin, &[], &["companion"], &[]),
            fake_plugin("companion", PluginOrigin::Plugin, &[], &[], &[]),
        ];
        let resolution = resolve(plugins);
        assert_eq!(ids(&resolution.report.plugins), ["companion", "consumer"]);
        let outcomes = &resolution.soft_deps["consumer"];
        assert_eq!(outcomes.len(), 1);
        assert!(outcomes[0].present);
        assert_eq!(outcomes[0].companion, "companion");
        assert!(resolution.report.issues.is_empty());
    }

    #[test]
    fn soft_depend_missing_still_loads_with_flag_false() {
        let plugins = vec![fake_plugin(
            "consumer",
            PluginOrigin::Plugin,
            &[],
            &["companion"],
            &[],
        )];
        let resolution = resolve(plugins);
        assert_eq!(ids(&resolution.report.plugins), ["consumer"]);
        let outcomes = &resolution.soft_deps["consumer"];
        assert!(!outcomes[0].present);
        assert!(
            resolution
                .report
                .issues
                .iter()
                .any(|(_, why)| why.contains("companion"))
        );
        // soft miss is not fatal for the strict entry point either
        let survivors = resolve_plugin_order(vec![fake_plugin(
            "consumer2",
            PluginOrigin::Plugin,
            &[],
            &["companion"],
            &[],
        )])
        .expect("soft miss keeps the set loadable");
        assert_eq!(ids(&survivors), ["consumer2"]);
    }

    #[test]
    fn plugin_depends_on_library_all_in_plugins_group_is_reversed_by_dep() {
        // libraries still sort before plugins; alphabetical inside plugins,
        // but dependency order wins over alphabetical inside one group
        let plugins = vec![
            fake_plugin("zzz-lib", PluginOrigin::Library, &[], &[], &[]),
            fake_plugin("aaa-lib2", PluginOrigin::Library, &[], &[], &[]),
            fake_plugin("b", PluginOrigin::Plugin, &["a"], &[], &[]),
            fake_plugin("a", PluginOrigin::Plugin, &[], &[], &[]),
        ];
        let resolution = resolve(plugins);
        // libraries first (alphabetical), then the plugins reversed by the
        // dependency edge b -> a
        assert_eq!(
            ids(&resolution.report.plugins),
            ["aaa-lib2", "zzz-lib", "a", "b"]
        );
    }

    #[test]
    fn plugin_can_depend_on_a_library_sorted_later() {
        let plugins = vec![
            fake_plugin("aaa", PluginOrigin::Plugin, &["zzz-lib"], &[], &[]),
            fake_plugin("zzz-lib", PluginOrigin::Library, &[], &[], &[]),
        ];
        let resolution = resolve(plugins);
        assert_eq!(ids(&resolution.report.plugins), ["zzz-lib", "aaa"]);
    }

    #[test]
    fn unsorted_input_gets_normalized_libraries_first() {
        let plugins = vec![
            fake_plugin("zebra", PluginOrigin::Plugin, &[], &[], &[]),
            fake_plugin("apple", PluginOrigin::Library, &[], &[], &[]),
        ];
        let resolution = resolve(plugins);
        assert_eq!(ids(&resolution.report.plugins), ["apple", "zebra"]);
    }

    #[test]
    fn duplicate_ids_are_detected() {
        let plugins = vec![
            fake_plugin("dup", PluginOrigin::Library, &[], &[], &[]),
            fake_plugin("dup", PluginOrigin::Plugin, &[], &[], &[]),
        ];
        match resolve_plugin_order(plugins) {
            Err(PluginError::DuplicatePluginIds(error)) => {
                assert_eq!(error.groups.len(), 1);
                assert_eq!(error.groups[0].id, "dup");
            }
            Err(e) => panic!("expected DuplicatePluginIds, got {e}"),
            Ok(_) => panic!("expected DuplicatePluginIds, got Ok(..)"),
        }
    }

    #[test]
    fn exports_table_is_built_for_survivors() {
        let plugins = vec![
            fake_plugin("api", PluginOrigin::Library, &[], &[], &["greet", "ping"]),
            fake_plugin("user", PluginOrigin::Plugin, &["api"], &[], &[]),
            fake_plugin(
                "dropped",
                PluginOrigin::Plugin,
                &["absent"],
                &[],
                &["hidden"],
            ),
        ];
        let resolution = resolve(plugins);
        assert_eq!(
            resolution.exports.get("api").map(Vec::as_slice),
            Some(["greet".to_owned(), "ping".to_owned()].as_slice())
        );
        assert!(!resolution.exports.contains_key("dropped"));
    }

    #[test]
    fn downstream_of_a_cycle_is_named_as_blocked() {
        let plugins = vec![
            fake_plugin("a", PluginOrigin::Plugin, &["b"], &[], &[]),
            fake_plugin("b", PluginOrigin::Plugin, &["a"], &[], &[]),
            fake_plugin("downstream", PluginOrigin::Plugin, &["a"], &[], &[]),
        ];
        match resolve_plugin_order(plugins) {
            Err(PluginError::DependencyCycle { diagnosis }) => {
                let joined = diagnosis.join("\n");
                assert!(joined.contains("downstream"));
            }
            Err(e) => panic!("expected DependencyCycle, got {e}"),
            Ok(_) => panic!("expected DependencyCycle, got Ok(..)"),
        }
    }
}
