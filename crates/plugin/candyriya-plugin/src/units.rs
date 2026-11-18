//! Supervision units for loaded plugins.
//!
//! Every plugin in the [`PluginSet`] gets a child unit under the `plugins`
//! group unit (the composition root spawns the group; this module spawns
//! its children). The unit body owns no work of its own — the plugin
//! keeps running on its dedicated OS thread, exactly as before. What the
//! unit adds is the missing failure signal: it parks on the plugin's
//! [`PluginHealth`] cell, and when the wasm worker flags the plugin
//! (fuel exhaustion, manifest-timeout interruption, or a host-side panic)
//! the body returns [`ExitStatus::Failure`], which is what the
//! [`RestartPolicy::OnFailure`] policy on the unit consumes.
//!
//! # Restart path
//!
//! On a failure exit the reaper moves the unit to `Restarting` (backoff
//! 1s -> 30s cap, jittered); when the backoff expires the body factory is
//! re-invoked and the new body *restages* the plugin — it drives
//! [`PluginReloader::reload`] for exactly this plugin (the same machinery
//! `/candyriya reload plugins` uses per running plugin: stage the
//! replacement, swap only on success). The staging runs on the
//! task-service blocking pool via [`TaskService::spawn_blocking`], never on
//! an async worker; a failed stage reports `Failure` again and the start
//! limit (5 attempts / 10 min) bounds the retries before the unit lands in
//! `FailedTerminal` and stays visible in the registry.
//!
//! The very first run after a boot load (or after any successful stage)
//! skips the restage — the plugin was just built from source of truth.
//! Records in [`PluginState::Failed`] have no live instance, so their unit
//! restages on its first run too: a plugin that failed preparation gets one
//! honest shot per attempt through the same bounded path, instead of
//! parking forever on a flag nothing will ever raise.
//!
//! # Reload integration
//!
//! [`sync_plugin_units`] is the one code path both boot and
//! `/candyriya reload plugins` drive. It spawns units for records that
//! appeared in the set and cancels units whose plugin id vanished from the
//! set (deleting plugin *files* never does that — records stay — so a
//! cancel here means a removal path removed the record, or a test called
//! the helper with a smaller set). Cancelled units stop at their next
//! await, exit `Cancelled`, and are harvested by the group's reaper.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use candyriya_init::{ExitStatus, Init, Pid, RestartPolicy, ServiceKind, UnitKind, UnitSpec};
use candyriya_scheduler::TaskService;
use tokio::sync::oneshot;
use tracing::{error, info, warn};

use crate::health::PluginHealthSet;
use crate::hot_reload::{PluginReloader, ReloadReport};
use crate::library_plugin::PluginState;
use crate::plugin_set::PluginSet;

/// How long a restage may take before the unit reports failure.
///
/// A wasm compile of a plugin module is seconds at worst in practice; if
/// it takes longer the plugin is effectively wedged, and the restart
/// policy bounds what happens next.
const RESTAGE_TIMEOUT: Duration = Duration::from_secs(30);

/// The name prefix of every plugin unit; `plugin-<id>`.
const UNIT_NAME_PREFIX: &str = "plugin-";

/// Shared plumbing a plugin supervision unit needs at spawn and at every
/// restart.
///
/// Cheap to clone — every clone refers to the same set, service and
/// health registry. `reloader` is the exact re-staging machinery the
/// reload command drives; the unit clones it per run so each restart
/// restages with the same configuration the spawn site was given.
#[derive(Clone)]
pub struct PluginUnitContext {
    /// The task service used to offload the blocking restage.
    pub service: TaskService,
    /// The reloader the restarted unit drives for its own id.
    pub reloader: PluginReloader,
    /// The shared health registry the unit parks on.
    pub health: PluginHealthSet,
}

/// What [`sync_plugin_units`] did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PluginUnitsReport {
    /// Ids for which a `plugin-<id>` unit was spawned.
    pub spawned: Vec<String>,
    /// Ids for which a running unit was cancelled (record left the set).
    pub cancelled: Vec<String>,
    /// Ids for which spawning failed (the plugin keeps running
    /// unsupervised; the reason is logged and does not abort anything).
    pub failed: Vec<(String, String)>,
}

/// Bring the `plugin-<id>` units under `parent` in line with the set.
///
/// Spawns a unit for every record in the set that does not have one
/// yet (in set order), and cancels units whose record id vanished from
/// the set. Idempotent: a converged tree is a no-op beyond one
/// `units()` scan.
///
/// Spawning does not happen while a unit with the same name is still
/// running — boot and reload both call this on the same tree, so the
/// check is the ordinary case, not an error path.
pub fn sync_plugin_units(
    tree: &Init,
    parent: Pid,
    plugins: &PluginSet,
    ctx: &PluginUnitContext,
) -> PluginUnitsReport {
    let mut report = PluginUnitsReport::default();
    // one scan, then all decisions are made against the same snapshot;
    // a concurrent reload cannot fork the hierarchy (it calls this too,
    // serially, on the operator thread)
    let existing: HashMap<String, Pid> = tree
        .units()
        .into_iter()
        .filter(|u| u.ppid == parent)
        .filter_map(|u| {
            u.name
                .strip_prefix(UNIT_NAME_PREFIX)
                .map(|id| (id.to_owned(), u.pid))
        })
        .collect();

    for id in plugins.ids() {
        if existing.contains_key(&id) {
            continue;
        }
        let fresh = plugins
            .state(&id)
            .is_some_and(|state| !matches!(state, PluginState::Failed(_)));
        match spawn_plugin_unit(tree, parent, &id, fresh, ctx) {
            Ok(pid) => {
                info!(plugin = %id, %pid, "plugin supervision unit spawned");
                report.spawned.push(id);
            }
            Err(why) => {
                error!(plugin = %id, %why, "failed to spawn the plugin supervision unit");
                report.failed.push((id, why));
            }
        }
    }

    for (id, pid) in &existing {
        if plugins.contains(id) {
            continue;
        }
        // the record is gone: the unit has nothing left to supervise
        match tree.cancel(*pid) {
            Ok(true) => {
                info!(plugin = %id, %pid, "plugin supervision unit cancelled; its plugin left the set");
                report.cancelled.push(id.clone());
            }
            Ok(false) => {
                // already finished; it will be reaped by the group's reaper
                report.cancelled.push(id.clone());
            }
            Err(error) => {
                warn!(plugin = %id, %pid, %error, "failed to cancel the plugin supervision unit");
                report.failed.push((id.clone(), error.to_string()));
            }
        }
    }

    report
}

/// Spawn one `plugin-<id>` unit under `parent`.
///
/// `fresh` should be `true` when the record was just built from source of
/// truth (a successful load or stage): the body then skips the restage on
/// its first run and parks on the health cell directly.
fn spawn_plugin_unit(
    tree: &Init,
    parent: Pid,
    id: &str,
    fresh: bool,
    ctx: &PluginUnitContext,
) -> Result<Pid, String> {
    let name = format!("{UNIT_NAME_PREFIX}{id}");
    // the factory is re-invoked on every restart; the flag is shared
    // across invocations so only the first run skips the restage
    let first = Arc::new(AtomicBool::new(fresh));
    let reloader = ctx.reloader.clone();
    let health = ctx.health.clone();
    let service = ctx.service.clone();
    let id_owned = id.to_owned();
    let factory = move || {
        let id = id_owned.clone();
        let reloader = reloader.clone();
        let health = health.clone();
        let service = service.clone();
        let first = Arc::clone(&first);
        async move {
            let skip_restage = first.swap(false, Ordering::SeqCst);
            if !skip_restage {
                match restage_one(&service, &reloader, &id).await {
                    Ok(report) => {
                        info!(plugin = %id, state = ?report.state, "plugin restaged by its supervision unit");
                    }
                    Err(why) => {
                        warn!(plugin = %id, %why, "plugin restage failed");
                        return ExitStatus::failure(format!("plugin `{id}` restage failed: {why}"));
                    }
                }
            }
            let cell = health.cell(&id);
            let mut rx = cell.subscribe();
            loop {
                if *rx.borrow_and_update() {
                    let reason = cell.reason().unwrap_or_else(|| "unknown".to_owned());
                    return ExitStatus::failure(format!(
                        "plugin `{id}` flagged unrecoverable: {reason}"
                    ));
                }
                if rx.changed().await.is_err() {
                    // the body holds the cell, so the sender cannot be
                    // gone in this shape; park instead of spinning if it
                    // ever happens
                    std::future::pending::<()>().await;
                }
            }
        }
    };
    let spec = UnitSpec::new(name, UnitKind::LongLived, RestartPolicy::OnFailure, factory)
        .with_service_kind(ServiceKind::Plugins);
    tree.spawn_unit(parent, spec).map_err(|e| e.to_string())
}

/// Drive [`PluginReloader::reload`] for a single plugin, off the async
/// worker.
///
/// The reload machinery is synchronous (filesystem reads and a wasm
/// compile); it runs on the task-service blocking pool and the body awaits
/// the outcome. `Ok` means the replacement was staged and swapped in
/// (plugins stays healthy); `Err` carries the staging failure — the
/// running plugin is untouched either way.
async fn restage_one(
    service: &TaskService,
    reloader: &PluginReloader,
    id: &str,
) -> Result<ReloadReport, String> {
    let (tx, rx) = oneshot::channel();
    let reloader = reloader.clone();
    let id = id.to_owned();
    let _task = service.spawn_blocking(format!("plugin-restage-{id}"), move || {
        let result = reloader.reload(&id);
        // the receiver disappears only when the body was dropped at a
        // cancellation boundary, in which case nobody wants the outcome
        let _ = tx.send(result);
    });
    match tokio::time::timeout(RESTAGE_TIMEOUT, rx).await {
        Ok(Ok(Ok(report))) => Ok(report),
        Ok(Ok(Err(error))) => Err(error.to_string()),
        Ok(Err(_)) => Err("the restage task ended without reporting".to_owned()),
        Err(_) => Err(format!("restage exceeded {}s", RESTAGE_TIMEOUT.as_secs())),
    }
}
