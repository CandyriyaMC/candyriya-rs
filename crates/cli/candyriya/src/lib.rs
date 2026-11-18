//! `candyriya` — the composition root and process lifecycle of the proxy.
//!
//! This crate wires every subsystem into one binary:
//!
//! 1. **CLI** — `--version`, `--help`, `--config <PATH>` (`-c`), defaulting to
//!    `./candyriya.toml`. A missing default file is replaced with the embedded
//!    template and the proxy proceeds; a missing *explicit* path is an error.
//! 2. **Telemetry** — `tracing-subscriber` fmt layer on stdout (ANSI colors
//!    when stdout is a TTY) plus a daily-rotating file log at
//!    `./logs/candyriya.log`. Installed before anything else happens.
//! 3. **Configuration** — [`ConfigStore`] owns the live config behind an
//!    `ArcSwap`; the boot phase reads it directly (it precedes the tree).
//!    Once the tree is up, every runtime read goes to the `config` unit
//!    through capability IPC — through the tree or not at all. Hot reload
//!    re-reads the file the proxy was started with and swaps the store
//!    atomically, so the next read through the unit sees the new snapshot.
//! 4. **Runtime** — tokio multi-thread pool sized from `[runtime] worker-threads`,
//!    and the central [`TaskService`] with a fixed rayon blocking pool from
//!    `[runtime] blocking-threads`. Every task in the process is registered
//!    with the service by name; there are no ad-hoc spawns.
//! 5. **Subsystems** — the cache manager (fronting the supervised `cache`
//!    unit every runtime cache read/write goes through), the Prometheus
//!    exporter (only when `[metrics] prometheus = true`), the
//!    Mojang/offline session validators selected from `online-mode`, the
//!    routing registry, the command tree and the interactive console REPL.
//!    Routing reads go to the `route` unit through the same IPC plane —
//!    login, the stats line and the directory all call it; `/candyriya
//!    reload` swaps the very store the unit serves, so a reload is visible
//!    to the next call. The plugin-facing core services the `candyriya:*`
//!    functions resolve through — the plugin dispatcher fan-out, the live
//!    play-state roster and the backend name list — are served by the
//!    `host` unit spawned alongside the rest of the serving units (see
//!    below), never by a registry or a stale snapshot.
//!    All of the above is the loader phase: everything that ran before
//!    the task service existed is adopted into the tree as an
//!    already-exited unit once the tree is up.
//! 6. **Lifecycle** — once the loader is done, the phases that keep
//!    running boot as units in `init`: `config` (the configuration source
//!    of truth), `cache` (the supervised owner of the live cache manager,
//!    a separate record from the `cache.manager` bootstrap adoption),
//!    `proxy.accept` (long-lived, the unit body drives the
//!    listener), `host` (the plugin-facing core services), `players` (the
//!    group node every connection unit hangs under — accounting and
//!    shutdown scoping, no work of its own), `plugins` (the group node for
//!    the plugin subsystem), `stats.every-60s`
//!    (periodic), `console` (the interactive REPL) and, at shutdown,
//!    `cache.shutdown` and `metrics.shutdown` (one-shot finalizers, the
//!    last units to run, in order). Connections accepted by `proxy.accept`
//!    are `conn-<n>` units under `players`; every loaded plugin rides a
//!    `plugin-<id>` unit under `plugins`. Ctrl-C or
//!    `/candyriya end` fires both the shutdown watch and a tree-wide
//!    cancel sweep; the serving units are waited on through the tree, the
//!    finalizers run last, the task service drains as the final safety
//!    net, and the process exits with code 0. If a boot unit fails before
//!    or during serving, the remaining units are unwound in reverse spawn
//!    order instead and the process exits with code 1.
//!
//! Plugins under `plugins/` and `libraries/` are loaded at startup; the
//! same machinery is exposed through `/candyriya reload` for a restart-free
//! refresh. Each loaded plugin rides a `plugin-<id>` unit under the
//! `plugins` group unit: the wasm worker flags genuinely unrecoverable
//! outcomes (out of fuel, the manifest timeout, a host-side panic) into a
//! health cell, the unit's parked body observes the flag, reports
//! `ExitStatus::Failure`, and the tree's `OnFailure` policy re-stages the
//! plugin through the same machinery `/candyriya reload` uses — bounded by
//! the start limit, after which the unit goes `FailedTerminal` and stays
//! visible. What never moves on a reload: the listener socket, live
//! sessions, and the forwarding secret — those are restart-only.

#![forbid(unsafe_code)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use clap::Parser;
use tokio::sync::watch;
use tracing::{debug, error, info, warn};

use candyriya_auth::{AuthConfig, AuthMode};
use candyriya_cache::{CacheManager, cache_spec};
use candyriya_command::{CommandRegistry, CommandState, ShutdownHandle};
use candyriya_config::{Config, ConfigStore, DEFAULT_CONFIG_TOML};
use candyriya_i18n::I18n;
use candyriya_init::{
    ExitStatus, Init, Pid, ReapOutcome, RestartPolicy, ServiceKind, UnitKind, UnitSpec,
};
use candyriya_plugin::{
    HostIpc, LimitBuilder, Limits, PluginHealthSet, PluginLoader, PluginReloader, PluginSet,
    PluginUnitContext, ProxyEventDispatcher, ServiceRegistry, SharedLocales, resolve_plugins,
    sync_plugin_units,
};
use candyriya_plugin_api::EventDispatcher;
use candyriya_proxy::{
    FaviconCache, ProxySocket, ProxyState, SocketConfig, accept_loop, config_spec, host_spec,
};
use candyriya_route::{
    BackendRegistry, DEFAULT_HEALTH_CHECK_INTERVAL, RouteMsg, RouteResp, health_spec, route_spec,
    router_store,
};
use candyriya_scheduler::TaskService;
use candyriya_telemetry::LogConfig;

mod ansi;
mod directory;
mod effects;
mod repl;

use crate::directory::{CliDirectory, TreeLinks};
use crate::effects::CliEffects;

/// The `--version` output line.
pub const VERSION_LINE: &str = concat!(
    "candyriya ",
    env!("CARGO_PKG_VERSION"),
    " (build ",
    env!("CANDYRIYA_BUILD_SHA"),
    ")"
);

/// The one banner line printed after the version line.
pub const BANNER: &str = "Candyriya - a Rust proxy for Minecraft Java Edition";

/// Command-line arguments.
#[derive(Debug, Clone, Parser)]
#[command(
    name = "candyriya",
    about = "Candyriya - a Rust proxy for Minecraft Java Edition",
    disable_version_flag = true,
    long_about = None
)]
pub struct Args {
    /// Print the version and banner, then exit.
    #[arg(long = "version", action = clap::ArgAction::SetTrue)]
    pub version: bool,

    /// Path to the configuration file (default: ./candyriya.toml). A missing
    /// default file is created from the embedded template; a missing
    /// explicitly-passed path is an error.
    #[arg(long = "config", short = 'c', value_name = "PATH")]
    pub config: Option<PathBuf>,
}

/// Errors the composition root can return; `main` renders them on stderr.
#[derive(Debug)]
pub enum CliError {
    /// The configuration file could not be loaded or validated.
    Config {
        /// The path that was read.
        path: PathBuf,
        /// What went wrong (secret resolution, TOML, validation).
        message: String,
    },
    /// `[servers]` failed to resolve to backend addresses.
    Router(String),
    /// A `host:port` in the configuration could not be turned into an address.
    Bind(String),
    /// Binding or installing the listener failed.
    Proxy(String),
    /// The cache manager failed to initialize or shut down.
    Cache(String),
    /// Installing the Prometheus exporter failed.
    Metrics(String),
    /// Initializing the telemetry stack failed.
    Telemetry(String),
    /// Building the runtime, the task service or the command tree failed.
    Runtime(String),
    /// Filesystem access failed.
    Io(std::io::Error),
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config { path, message } => {
                write!(f, "configuration file {}: {message}", path.display())
            }
            Self::Router(message) => write!(f, "backend registry: {message}"),
            Self::Bind(message) => write!(f, "address resolution: {message}"),
            Self::Proxy(message) => write!(f, "proxy: {message}"),
            Self::Cache(message) => write!(f, "cache: {message}"),
            Self::Metrics(message) => write!(f, "metrics: {message}"),
            Self::Telemetry(message) => write!(f, "telemetry: {message}"),
            Self::Runtime(message) => write!(f, "startup: {message}"),
            Self::Io(source) => write!(f, "io: {source}"),
        }
    }
}

impl std::error::Error for CliError {}

impl From<std::io::Error> for CliError {
    fn from(source: std::io::Error) -> Self {
        Self::Io(source)
    }
}

/// Parse arguments, initialize telemetry and run the proxy to shutdown.
///
/// This is the whole process: it never returns until shutdown is complete.
/// Failures are logged, and the exit code is 1.
pub fn run(args: Args) -> Result<(), CliError> {
    let data_dir = std::env::current_dir()?;

    // telemetry first, so every diagnostic from here on reaches stdout and
    // the rotating daily file under <data_dir>/logs
    let logs_dir = data_dir.join("logs");
    let level = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());
    let log_config = LogConfig::new()
        .level(level.clone())
        .colors(candyriya_telemetry::Colors::Auto)
        .with_file_rotation(&logs_dir);
    let use_ansi = log_config.effective_ansi();
    let telemetry = candyriya_telemetry::init(&log_config)
        .map_err(|error| CliError::Telemetry(error.to_string()))?;

    // anything from here on is logged; `main` prints the final error line
    let result = run_proxy(&data_dir, &level, use_ansi, args);
    match &result {
        Ok(()) => info!("candyriya stopped"),
        Err(error) => error!("{error}"),
    }
    telemetry.shutdown();
    result
}

/// Poll cadence for the selection loop's unit-failure watch and for the
/// bounded waits in the shutdown cascade. Short enough that a failed accept
/// stops the process without a human noticing, cheap enough to ignore.
const UNIT_FAILURE_POLL: Duration = Duration::from_millis(100);

/// Budget for a group of units to exit after they were asked to, before
/// the cascade stops waiting on them and the drain deadline decides.
const UNIT_STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// Register a bootstrap step that has already run (successfully or not) as
/// an adopted unit, so the tree stays the source of truth for what ran.
///
/// Adoption failures are logged and swallowed on purpose: if the kernel
/// cannot record the step here the boot must surface the *real* error,
/// not this one. `status` records what actually happened to the step.
fn adopt_bootstrap(tree: &Init, name: &str, status: ExitStatus) {
    if let Err(error) = tree.adopt_unit(Pid::INIT, name, UnitKind::Oneshot, status) {
        warn!(%error, step = name, "failed to adopt bootstrap step");
    }
}

/// Whether an exit status is a failure worth aborting the boot for.
///
/// `Cancelled` is excluded on purpose: a unit we stopped ourselves is not
/// a failure. Mirrors the restart-policy notion of a failure exit.
fn exit_is_failure(status: &ExitStatus) -> bool {
    matches!(
        status,
        ExitStatus::Failure(_) | ExitStatus::Panicked | ExitStatus::TimedOut
    )
}

/// Harvest failure exits among the root's direct children.
///
/// Returns a one-line detail of one failure, and nothing when every
/// harvested unit exited cleanly. Reaping happens as a side effect, which
/// keeps the stuck-zombie gauge truthful and lets the same helper drive
/// the boot checks and the mid-run failure watch.
fn unit_failure_detail(tree: &Init) -> Option<String> {
    tree.reap_children(Pid::INIT)
        .into_iter()
        .find_map(|outcome| {
            match outcome {
                ReapOutcome::Reaped(record) | ReapOutcome::Restarting { record, .. }
                    if exit_is_failure(&record.status) =>
                {
                    Some(format!(
                        "unit {} (pid {}) exited with {}",
                        record.name, record.pid, record.status
                    ))
                }
                ReapOutcome::Reaped(_) | ReapOutcome::Restarting { .. } => None,
                // Never-policy units (everything the composition root spawns)
                // never land here; a restart-limited unit running out of budget
                // certainly counts as a failure
                ReapOutcome::FailedTerminal { record } => Some(format!(
                    "unit {} (pid {}) reached failed-terminal after {}",
                    record.name, record.pid, record.status
                )),
            }
        })
}

/// The group units that carry children for shutdown-scan purposes.
///
/// `players` carries the `conn-<n>` units, `plugins` carries the
/// `plugin-<id>` units. Both are reaped while their parent is still
/// alive during teardown so their zombies are harvested (and pids leave
/// the registry) before the final wait; a dead conn or plugin unit must
/// never linger unreaped into the finalizers.
const GROUP_UNIT_NAMES: &[&str] = &["players", "plugins"];

/// Reap the zombie children of every group unit currently in the
/// registry.
///
/// Group names are unique by construction, so a registry scan finds at
/// most one of each; a group that was already reaped is simply absent.
/// Children exit constantly (conns, plugin units) and stay zombies until
/// harvested — the mid-run tick, the boot abort, the cascade and the
/// shutdown wait all close that window through this one call.
fn reap_group_children(tree: &Init) {
    for unit in tree.units() {
        if GROUP_UNIT_NAMES.contains(&unit.name.as_str()) {
            let _ = tree.reap_children(unit.pid);
        }
    }
}

/// Unwind a boot that is not allowed to continue serving.
///
/// Cancels every unit in the tree through the kernel's sweep — newest
/// first, including connection and plugin units the spawn sites do not
/// track by name —
/// harvests whatever already exited (both group units' children and
/// the root's), drains the kernel and hands the reason back for the
/// caller to report. Boot-adopted units cannot appear here; they are
/// reaped as they adopt. The drain is the same safety net the normal
/// shutdown uses, so a unit wedged mid-await still gets bounded by it.
async fn boot_abort(tree: &Init, service: &TaskService, error: CliError) -> CliError {
    let swept = tree.cancel_all();
    debug!(
        swept = swept.len(),
        "tree-wide cancel sweep requested stops"
    );
    reap_group_children(tree);
    let _ = tree.reap_children(Pid::INIT);
    let report = service.drain(Duration::from_secs(10)).await;
    info!(?report, "task service drained after boot abort");
    error
}

/// Wait until every pid in `pids` has left the registry (harvested by the
/// reaper) or the budget runs out. Group children are reaped alongside
/// the root's while they are still alive; returns the pids still tracked.
async fn wait_units_gone(tree: &Init, pids: &[Pid], budget: Duration) -> Vec<Pid> {
    let start = Instant::now();
    let mut pending: Vec<Pid> = pids.to_vec();
    while !pending.is_empty() && start.elapsed() < budget {
        reap_group_children(tree);
        let _ = tree.reap_children(Pid::INIT);
        pending.retain(|pid| tree.unit(*pid).is_some());
        if !pending.is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    pending
}

/// Close the cache subsystem, logging the outcome the way the inline
/// teardown always did.
async fn shutdown_cache(cache: &CacheManager) -> ExitStatus {
    match cache.shutdown() {
        Ok(report) => {
            debug!(?report, "cache shut down");
            ExitStatus::Success
        }
        Err(error) => {
            warn!(%error, "cache shutdown failed");
            ExitStatus::failure(error.to_string())
        }
    }
}

/// Close the Prometheus exporter installation. In pull mode this is a
/// no-op beyond the state flag, exactly as when it ran inline.
async fn shutdown_metrics(handle: candyriya_metrics::MetricsHandle) -> ExitStatus {
    handle.shutdown();
    ExitStatus::Success
}

/// Run one finalizer unit to completion.
///
/// Finalizers are the last units to run at shutdown and they run in
/// order — this function waits for the unit to exit before returning.
/// If the kernel cannot spawn it (a wedged registry at the end of the
/// process, practically unreachable), the finalizer runs inline instead:
/// the teardown must still happen, and the caller has nothing else to
/// fall back to.
async fn run_finalizer<F, Fut>(tree: &Init, name: &'static str, service_kind: ServiceKind, make: F)
where
    F: Fn() -> Fut + Send + Sync + Clone + 'static,
    Fut: std::future::Future<Output = ExitStatus> + Send + 'static,
{
    let spec = UnitSpec::new(name, UnitKind::Oneshot, RestartPolicy::Never, make.clone())
        .with_service_kind(service_kind);
    match tree.spawn_unit(Pid::INIT, spec) {
        Ok(pid) => {
            let _ = wait_units_gone(tree, &[pid], UNIT_STOP_TIMEOUT).await;
        }
        Err(error) => {
            error!(%error, %name, "failed to spawn the finalizer unit; running it inline");
            let status = make().await;
            if !status.is_success() {
                warn!(%status, %name, "finalizer failed via the inline fallback");
            }
        }
    }
}

/// The cascade: the shutdown cause has already been logged by the caller.
///
/// Sequence: flip the watch (bridge for the accept loop and the REPL),
/// cancel every unit in the tree through the kernel's sweep (newest
/// first), wait for them through the tree, run the finalizers in order
/// (`cache.shutdown` then `metrics.shutdown`), and drain the kernel as the
/// final safety net. The exit ring keeps every unit exit for the
/// post-mortem.
async fn cascade_shutdown(
    tree: &Init,
    service: &TaskService,
    shutdown_tx: &tokio::sync::watch::Sender<bool>,
    cache: Arc<CacheManager>,
    metrics_handle: candyriya_metrics::MetricsHandle,
) {
    // bridge: the accept loop and the REPL still watch the flip
    let _ = shutdown_tx.send(true);

    // tree-wide sweep: every non-root unit, newest first, one call. A
    // connection accepted a second ago, a plugin supervision unit and the
    // players/plugins groups are ordinary units to this walk — spawn
    // order bounds nothing. route.health (child of route)
    // is reached directly; when route exits, orphaned children re-parent to
    // the root and the wait below reaps them there. Connections die at
    // their next loop boundary: pump cancel = client disconnect. Plugin
    // units die at the parked await inside their body, plugin threads keep
    // running until the set is dropped — the drain accounts stragglers by
    // contract
    let swept = tree.cancel_all();
    debug!(
        swept = swept.len(),
        "tree-wide cancel sweep requested stops"
    );
    // children of a still-live group stay zombies until reaped; the group
    // re-parents them to the root only at its own exit — close the window
    // explicitly before waiting
    reap_group_children(tree);

    // wait for the swept units to exit; the reaper stays warm
    let pending = wait_units_gone(tree, &swept, UNIT_STOP_TIMEOUT).await;
    if !pending.is_empty() {
        let names: Vec<String> = pending
            .iter()
            .map(|pid| {
                tree.unit(*pid)
                    .map(|unit| unit.name)
                    .unwrap_or_else(|| format!("pid {pid}"))
            })
            .collect();
        warn!(
            ?names,
            "swept units did not stop before the cascade deadline; the drain deadline applies"
        );
    }

    // finalizers, last and in order: cache first, metrics second
    let cache_make = {
        let cache = Arc::clone(&cache);
        move || {
            let cache = Arc::clone(&cache);
            async move { shutdown_cache(&cache).await }
        }
    };
    run_finalizer(tree, "cache.shutdown", ServiceKind::Cache, cache_make).await;

    // metrics handle is consumed on shutdown, so it travels in a slot
    let metrics_slot = Arc::new(std::sync::Mutex::new(Some(metrics_handle)));
    let metrics_make = {
        let slot = Arc::clone(&metrics_slot);
        move || {
            let slot = Arc::clone(&slot);
            async move {
                match slot.lock().ok().and_then(|mut guard| guard.take()) {
                    Some(handle) => shutdown_metrics(handle).await,
                    None => {
                        warn!(
                            "metrics finalizer ran with the handle already consumed; nothing to close"
                        );
                        ExitStatus::Success
                    }
                }
            }
        }
    };
    run_finalizer(tree, "metrics.shutdown", ServiceKind::Unit, metrics_make).await;

    // final safety net, accounting timed-out stragglers by contract
    let report = service.drain(Duration::from_secs(10)).await;
    info!(?report, "task service drained");
}

/// Full startup: configuration, subsystems, tasks, wait for shutdown, drain.
#[allow(clippy::too_many_lines)]
fn run_proxy(data_dir: &Path, log_level: &str, use_ansi: bool, args: Args) -> Result<(), CliError> {
    // ---- configuration: an explicitly passed path must exist; the default
    // path is created from the embedded template when missing
    let explicit_config = args.config.is_some();
    let config_path = args
        .config
        .clone()
        .unwrap_or_else(|| PathBuf::from("candyriya.toml"));
    if !config_path.exists() {
        if explicit_config {
            return Err(CliError::Config {
                path: config_path,
                message: "configuration file does not exist; pass --config <PATH>, or run without arguments to create ./candyriya.toml from the embedded template".to_string(),
            });
        }
        match std::fs::write(&config_path, DEFAULT_CONFIG_TOML) {
            Ok(()) => {
                info!(path = %config_path.display(), "no configuration file found; wrote the embedded default template")
            }
            Err(source) => warn!(
                path = %config_path.display(),
                %source,
                "failed to write the default configuration template; running on embedded defaults"
            ),
        }
    }
    let config_name = config_path.display().to_string();
    let store =
        Arc::new(
            ConfigStore::from_path(&config_path).map_err(|error| CliError::Config {
                path: config_path.clone(),
                message: error.to_string(),
            })?,
        );
    let config = store.get();
    info!(
        config = %config_name,
        bind = %config.bind,
        online_mode = config.online_mode,
        forwarding_mode = ?config.player_info_forwarding_mode,
        "configuration loaded"
    );

    // ---- backend registry: resolve addresses once, synchronously, before
    // anything serves (blocking DNS here is fine: we are not online yet).
    // No backends configured is a legal setup (status-only or a first run):
    // routing stays unwired and authenticated players are disconnected with
    // "No available server" — that is the proxy's documented behavior here.
    let router: Option<Arc<BackendRegistry>> = if config.servers.servers.is_empty() {
        info!("no [servers] configured; backend routing is disabled");
        None
    } else {
        match candyriya_proxy::registry_from_config(&config) {
            Ok(registry) => Some(Arc::new(registry)),
            Err(message) => return Err(CliError::Router(message)),
        }
    };

    // ---- runtime + task service (thread counts come from [runtime])
    let worker_threads = config.runtime.worker_threads.max(1);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads)
        .enable_all()
        .build()
        .map_err(|error| {
            CliError::Runtime(format!("failed to build the tokio runtime: {error}"))
        })?;

    let result = runtime.block_on(run_async(
        data_dir,
        log_level,
        use_ansi,
        &config_name,
        &config_path,
        config,
        store,
        router,
    ));
    drop(runtime);
    result
}

/// Every subsystem, spawned and wired; blocks on the shutdown signal.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
async fn run_async(
    data_dir: &Path,
    log_level: &str,
    use_ansi: bool,
    config_name: &str,
    config_path: &Path,
    config: Arc<Config>,
    store: Arc<ConfigStore>,
    router: Option<Arc<BackendRegistry>>,
) -> Result<(), CliError> {
    let service = TaskService::builder()
        .handle(tokio::runtime::Handle::current())
        .blocking_threads(config.runtime.blocking_threads.max(1))
        .build()
        .map_err(|error| CliError::Runtime(format!("failed to build the task service: {error}")))?;

    // ---- the tree. Everything from here on is either adopted as an
    // already-exited loader step or spawned as a unit; the order of the
    // adoption calls mirrors the boot order.
    // ipc-migration: TaskService stays the kernel; units hand it off to the tree
    let tree = Init::new(service.clone());
    adopt_bootstrap(&tree, "cli.bootstrap", ExitStatus::Success);
    adopt_bootstrap(&tree, "config.load", ExitStatus::Success);
    adopt_bootstrap(&tree, "route.resolve", ExitStatus::Success);

    // one shared route store: the route unit serves from it, the reload
    // path swaps through it — the backend name list is served by the same
    // unit to the plugin host functions
    let router_store = router_store(router);

    // ---- the config unit: it owns the live store and is the single source
    // of truth for every runtime read of the configuration — through the
    // tree or not at all. The store is shared with the reload path
    // (which is allowed to keep its handle), but nobody reads the snapshot
    // without going through the unit's mailbox from here on.
    let config_pid = match tree.spawn_unit(Pid::INIT, config_spec(Arc::clone(&store))) {
        Ok(pid) => {
            info!(pid = %pid, "config unit spawned");
            pid
        }
        Err(error) => {
            return Err(boot_abort(
                &tree,
                &service,
                CliError::Runtime(format!("failed to spawn config: {error}")),
            )
            .await);
        }
    };
    // ---- the route unit: the single owner of runtime routing reads. The
    // store it serves is the same object the reload path swaps through, so
    // /candyriya reload works unchanged; nobody calls the registry
    // directly from here on. Always spawned, even with no [servers] — an
    // empty store is an honest state.
    let route_pid = match tree.spawn_unit(Pid::INIT, route_spec(Arc::clone(&router_store))) {
        Ok(pid) => {
            info!(pid = %pid, "route unit spawned");
            pid
        }
        Err(error) => {
            return Err(boot_abort(
                &tree,
                &service,
                CliError::Runtime(format!("failed to spawn route: {error}")),
            )
            .await);
        }
    };
    // ---- the route.health child: the health-check loop lives with the
    // route unit. Spawned only when [servers] is non-empty at boot — with
    // nothing to check the child would be dead weight. If a later
    // /candyriya reload adds servers, nothing watches them here; the
    // reload phase owns routing changes and does not start health checks
    // (that belongs to the reload phase, not this slice). The health
    // child belongs to the route unit and stays with it — no list
    // bookkeeping; the tree is the list.
    if !config.servers.servers.is_empty() {
        match tree.spawn_unit(
            route_pid,
            health_spec(Arc::clone(&router_store), DEFAULT_HEALTH_CHECK_INTERVAL),
        ) {
            Ok(pid) => {
                info!(
                    pid = %pid,
                    interval_secs = DEFAULT_HEALTH_CHECK_INTERVAL.as_secs(),
                    "route.health unit spawned"
                );
            }
            Err(error) => {
                return Err(boot_abort(
                    &tree,
                    &service,
                    CliError::Runtime(format!("failed to spawn route.health: {error}")),
                )
                .await);
            }
        }
    } else {
        info!("no [servers] configured; backend health checks are not wired");
    }

    // the registry now carries plugin-owned services only (entries tagged
    // with an owning plugin id, revoked in bulk on reload); it is shared
    // with the hot-reload path, where a plugin may re-register after a
    // swap. Every core service the registry used to hold has dissolved
    // into a supervised unit.
    let registry = ServiceRegistry::new();

    // ---- the cache manager (the `cache` unit spawned below serves the
    // runtime reads; plugins borrow the manager for their quotas)
    let cache = match CacheManager::new(data_dir) {
        Ok(cache) => {
            let cache = Arc::new(cache);
            adopt_bootstrap(&tree, "cache.manager", ExitStatus::Success);
            cache
        }
        Err(error) => {
            adopt_bootstrap(
                &tree,
                "cache.manager",
                ExitStatus::failure(error.to_string()),
            );
            return Err(boot_abort(&tree, &service, CliError::Cache(error.to_string())).await);
        }
    };

    // ---- the cache unit: the supervised owner of the live manager. The
    // loader adopted `cache.manager` above as an already-exited bootstrap
    // step; this is the real, long-lived unit that serves runtime cache
    // operations — the adopt marker and the running unit are separate
    // records in the tree. Always spawned: an empty cache is an honest
    // state (callers see misses), not a reason to run without the unit.
    let cache_pid = match tree.spawn_unit(Pid::INIT, cache_spec(Arc::clone(&cache))) {
        Ok(pid) => {
            info!(pid = %pid, "cache unit spawned");
            pid
        }
        Err(error) => {
            return Err(boot_abort(
                &tree,
                &service,
                CliError::Runtime(format!("failed to spawn cache: {error}")),
            )
            .await);
        }
    };
    // the favicon handle is a thin IPC shim over the cache unit: the proxy
    // holds `Init + cache_pid`, never a namespace handle. Always wired at
    // boot; the `Option` is the wiring contract the reload path was built
    // against. TTL-bounded so a `[motd-favicon]` edit is picked up without
    // a restart.
    let favicon_cache: Option<Arc<FaviconCache>> = Some(Arc::new(FaviconCache::new(
        tree.clone(),
        cache_pid,
        Duration::from_secs(3600),
    )));
    warm_favicon(&config, favicon_cache.as_deref()).await;

    // ---- metrics (off unless [metrics] prometheus = true)
    let metrics_addr = if config.metrics.prometheus {
        match resolve_socket_addr(&config.metrics.prometheus_address) {
            Ok(addr) => Some(addr),
            Err(error) => {
                adopt_bootstrap(
                    &tree,
                    "metrics.exporter",
                    ExitStatus::failure(error.to_string()),
                );
                return Err(boot_abort(&tree, &service, error).await);
            }
        }
    } else {
        None
    };
    let metrics_handle = match candyriya_metrics::install(metrics_addr) {
        Ok(handle) => {
            adopt_bootstrap(&tree, "metrics.exporter", ExitStatus::Success);
            handle
        }
        Err(error) => {
            adopt_bootstrap(
                &tree,
                "metrics.exporter",
                ExitStatus::failure(error.to_string()),
            );
            return Err(boot_abort(&tree, &service, CliError::Metrics(error.to_string())).await);
        }
    };

    // ---- session validators, selected from online-mode
    let auth = AuthConfig {
        mode: if config.online_mode {
            AuthMode::Online
        } else {
            AuthMode::Offline
        },
        ..AuthConfig::default()
    };
    info!(mode = ?auth.mode, "session validation configured");
    adopt_bootstrap(&tree, "auth.validators", ExitStatus::Success);

    // ---- command locales + the kick-locale handle, plus the shared pair
    // the plugin host functions resolve through (built before the plugin
    // load so the loader can register plugin locale files into the same
    // machines). One bootstrap step, one adopted unit.
    let (locales, plugin_locales) = match build_locales(data_dir) {
        Ok(pair) => pair,
        Err(error) => {
            adopt_bootstrap(&tree, "i18n.build", ExitStatus::failure(error.to_string()));
            return Err(boot_abort(&tree, &service, error).await);
        }
    };
    let kick_i18n = match build_kick_i18n(data_dir, &config) {
        Ok(i18n) => i18n,
        Err(error) => {
            adopt_bootstrap(&tree, "i18n.build", ExitStatus::failure(error.to_string()));
            return Err(boot_abort(&tree, &service, error).await);
        }
    };
    adopt_bootstrap(&tree, "i18n.build", ExitStatus::Success);

    // ---- the host IPC link: every plugin instance carries this slot; the
    // `host` unit is spawned below (it needs the shared proxy state), and
    // once it exists the link gets filled with the tree and the unit pid.
    // Startup plugins built before that see the honest "link is not
    // installed" error from the cross-subsystem host functions until the
    // fill lands — one process, one install, before serving starts
    let host_ipc = HostIpc::new();

    // ---- plugins: startup load (before the proxy state is built: the
    // shared dispatcher gets wired into it at construction time). The
    // health registry is created first: every instance build clears its
    // plugin's conviction in it, and the supervision units spawned right
    // after the load park on the same cells.
    let plugin_health = PluginHealthSet::new();
    let plugin_limits = LimitBuilder::from_config(&config).build();
    let plugin_set = load_plugins(
        data_dir,
        &cache,
        plugin_locales.clone(),
        host_ipc.clone(),
        plugin_limits.clone(),
        plugin_health.clone(),
    );
    adopt_bootstrap(&tree, "plugins.load", ExitStatus::Success);
    // ---- the `plugins` group unit: the supervisor-group node for the
    // whole plugin subsystem. A supervisor-group node — it holds no work
    // and owns no resources (a pending body is the honest shape); it
    // exists so the plugin units have a subtree: accounting, shutdown
    // scoping and restart attribution all ride it. LongLived + Never: it
    // does not come back and it is not re-creatable. Spawned right after
    // the load step completes (and before `players`/`host`/`accept`), so
    // the plugin subtree is accounted from birth; the `plugins.load`
    // marker stays as the bootstrap-step record.
    let plugins_spec = UnitSpec::new(
        "plugins",
        UnitKind::LongLived,
        RestartPolicy::Never,
        std::future::pending::<ExitStatus>,
    )
    .with_service_kind(ServiceKind::Plugins);
    let plugins_pid = match tree.spawn_unit(Pid::INIT, plugins_spec) {
        Ok(pid) => {
            info!(pid = %pid, "plugins group unit spawned");
            pid
        }
        Err(error) => {
            return Err(boot_abort(
                &tree,
                &service,
                CliError::Runtime(format!("failed to spawn plugins: {error}")),
            )
            .await);
        }
    };
    // one child per loaded plugin (libraries and plugins alike): the set
    // is the source of truth for what exists, the tree for how it behaves.
    // Spawn failures are loud but never abort the boot — a plugin whose
    // unit cannot spawn still runs, just unsupervised.
    let restage_reloader = PluginReloader::with_limits(
        (*plugin_set).clone(),
        data_dir.to_path_buf(),
        registry.clone(),
        plugin_limits.clone(),
    )
    .with_locales(plugin_locales.clone())
    .with_host_ipc(host_ipc.clone())
    .with_plugin_health(plugin_health.clone());
    let plugin_units = sync_plugin_units(
        &tree,
        plugins_pid,
        &plugin_set,
        &PluginUnitContext {
            service: service.clone(),
            reloader: restage_reloader,
            health: plugin_health.clone(),
        },
    );
    if !plugin_units.failed.is_empty() {
        error!(
            failed = ?plugin_units.failed,
            "some plugin supervision units failed to spawn; those plugins run unsupervised"
        );
    }
    // one dispatcher per process: the `host` unit carries it for the
    // plugin→plugin callApi fan-out, the proxy state carries it for the
    // lifecycle events — both hold handles to the same instance, so
    // `/candyriya reload` swaps what they both see without any
    // re-wiring
    let event_dispatcher = Arc::new(ProxyEventDispatcher::new((*plugin_set).clone()));

    // ---- the `players` group unit: the parent of every connection unit.
    // A supervisor-group node — it holds no sockets and owns no work (a
    // pending body is the honest shape); it exists so live sessions have a
    // subtree: accounting, shutdown scoping and the parent rule for grants
    // all ride it. LongLived + Never: it does not come back and it is not
    // re-creatable — its children dying with it is a shutdown, not a failure.
    // The grants ride its spawn because it passes them down to conn units
    // the way an inherited fd would: config for per-connection fetches,
    // route for backend resolution, cache for the favicon (when wired).
    let players_spec = UnitSpec::new(
        "players",
        UnitKind::LongLived,
        RestartPolicy::Never,
        std::future::pending::<ExitStatus>,
    )
    .with_grant(config_pid)
    .with_grant(route_pid)
    .with_grant(cache_pid);
    let players_pid = match tree.spawn_unit(Pid::INIT, players_spec) {
        Ok(pid) => {
            info!(pid = %pid, "players group unit spawned");
            pid
        }
        Err(error) => {
            return Err(boot_abort(
                &tree,
                &service,
                CliError::Runtime(format!("failed to spawn players: {error}")),
            )
            .await);
        }
    };

    // ---- shared proxy state
    // ipc-migration: these fields are cross-subsystem handles (Plugins.Dispatch
    // and I18n.T ride them); the IPC conversion gives each its own unit
    let started = Instant::now();
    let mut proxy_state = ProxyState::new(
        tree.clone(),
        config_pid,
        route_pid,
        players_pid,
        Arc::clone(&router_store),
    );
    // ipc-migration: ProxyState carries handles across the subsystem boundary
    // until each is a unit; the roster moved into the `host` unit
    proxy_state.auth = auth;
    proxy_state.favicon_cache = favicon_cache.clone();
    proxy_state.event_dispatcher = Some(Arc::clone(&event_dispatcher) as Arc<dyn EventDispatcher>);
    proxy_state.event_i18n = Some(Arc::new(kick_i18n));
    let proxy_state = Arc::new(proxy_state);

    // ---- the `host` unit: the supervised owner of the plugin-facing core
    // services (the plugin→plugin callApi fan-out, the live play-state
    // roster, the backend name list). Spawned after the shared proxy
    // state exists because the handler reads the roster and serves the
    // dispatcher from it; the route pid rides the spawn as a grant so the
    // unit can ask the route unit for the names itself — the subtree rule
    // does not cover sibling calls. The grant to route is all the unit
    // gets; nothing outside the tree answers to it.
    let host_pid = match tree.spawn_unit(
        Pid::INIT,
        host_spec(
            Arc::clone(&proxy_state),
            Arc::clone(&event_dispatcher),
            route_pid,
            tree.clone(),
            service.clone(),
        ),
    ) {
        Ok(pid) => {
            info!(pid = %pid, "host unit spawned");
            pid
        }
        Err(error) => {
            return Err(boot_abort(
                &tree,
                &service,
                CliError::Runtime(format!("failed to spawn host: {error}")),
            )
            .await);
        }
    };
    if !host_ipc.install(tree.clone(), host_pid, tokio::runtime::Handle::current()) {
        // one install per process; a second one would silently re-point
        // every host function — refuse to serve instead
        return Err(boot_abort(
            &tree,
            &service,
            CliError::Runtime("host IPC link was installed twice".to_owned()),
        )
        .await);
    }

    // ---- command tree: the registry itself is built after the plugin set
    // exists, so `/candyriya reload` can run both phases
    let shutdown = ShutdownHandle::new();
    let ps_watch = candyriya_command::PsWatchHandle::new();
    let directory = Arc::new(CliDirectory::new(
        Arc::clone(&proxy_state),
        TreeLinks {
            runtime: tokio::runtime::Handle::current(),
            init: tree.clone(),
            config_pid,
            boot_fallback: Arc::clone(&config),
        },
        config_name.to_string(),
        log_level.to_string(),
        started,
    ));
    // one trait-object view, shared by the command state and the console
    // unit (ps --watch frames)
    let directory: Arc<dyn candyriya_command::NetworkDirectory> = directory;
    let effects = Arc::new(CliEffects::new(use_ansi));
    let console_locales = locales.clone();
    let console_directory = Arc::clone(&directory);
    let console_watch = ps_watch.clone();
    let shutdown_ctrlc = shutdown.clone();
    let command_state = CommandState {
        locales: locales.clone(),
        directory,
        effects: effects.clone() as Arc<dyn candyriya_command::ProxyEffects>,
        watch: ps_watch,
    };

    // ---- hot reload: the same machinery, on demand
    // ipc-migration: Config.Reload / Plugins.Reload — the ReloadFn closure runs synchronously on the REPL's blocking thread
    let reload_fn = build_reload_fn(
        Arc::clone(&store),
        config_path.to_path_buf(),
        Arc::clone(&proxy_state),
        favicon_cache.clone(),
        locales,
        Arc::clone(&plugin_set),
        Arc::clone(&cache),
        registry.clone(),
        host_ipc.clone(),
        data_dir,
        plugin_locales,
        tokio::runtime::Handle::current(),
        tree.clone(),
        plugins_pid,
        service.clone(),
        plugin_health.clone(),
    );
    let command_registry =
        CommandRegistry::new(command_state, shutdown.clone()).with_reload(reload_fn);
    let dispatcher = Arc::new(command_registry.dispatcher().map_err(|error| {
        CliError::Runtime(format!("failed to assemble the built-in commands: {error}"))
    })?);

    // ---- one shutdown signal for Ctrl-C and /candyriya end; the accept
    // loop watches one receiver, the REPL flips it, the main loop watches
    // another. The tree gets a cancel sweep alongside the flip (in
    // cascade_shutdown); the watch stays as a bridge until phase 3
    // removes the accept half of it
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut console_rx = shutdown_tx.subscribe();

    // ---- bind the listener (loader phase: before the tree serves). If
    // resolution or the bind itself fails, the step is adopted as a
    // failure and the boot aborts before any serving unit exists.
    let bind_addr = match resolve_socket_addr(&config.bind) {
        Ok(addr) => addr,
        Err(error) => {
            adopt_bootstrap(&tree, "proxy.bind", ExitStatus::failure(error.to_string()));
            return Err(boot_abort(&tree, &service, error).await);
        }
    };
    let socket = match ProxySocket::bind(bind_addr, &SocketConfig::from(&config.advanced)) {
        Ok(socket) => socket,
        Err(error) => {
            adopt_bootstrap(&tree, "proxy.bind", ExitStatus::failure(error.to_string()));
            return Err(boot_abort(&tree, &service, CliError::Proxy(error.to_string())).await);
        }
    };
    adopt_bootstrap(&tree, "proxy.bind", ExitStatus::Success);
    let local_addr = socket.local_addr();
    let listener = socket.into_listener();

    // ---- every bootstrap step is adopted; reap them now so the ring
    // tells the boot story and no zombie lingers into serving
    let adopted = tree.reap_children(Pid::INIT).len();
    debug!(adopted, "bootstrap steps adopted and reaped");

    // ---- spawn the serving units, in boot order; a failure of any of
    // them unwinds everything through the tree-wide sweep (fail-fast
    // boot). Spawn order is enforced by code order — the tree is the
    // hierarchy, so there is no list to keep in sync: cancel_all reaches
    // every non-root unit, group units and their children included, and
    // the mid-run watchdog watches the root's direct children for
    // failures (plugin units are deliberately NOT watchdog material — a
    // crashed plugin must never abort the proxy, it degrades to
    // FailedTerminal and stays visible in ps; the mid-run tick reaps
    // them and the restart policy fires).

    // proxy.accept: the unit body drives listener.accept(); the connections
    // it accepts are registered as `conn-<n>` units under the `players`
    // group unit (spawned above), each carrying a cooperative park gate and
    // its own byte counters. The listener and the shutdown receiver are not
    // Clone, so they ride in a single slot consumed by the one factory
    // invocation a RestartPolicy::Never unit ever gets; a re-invocation
    // reports a failure instead of serving on a stolen socket.
    let accept_boot = Arc::new(std::sync::Mutex::new(Some((listener, shutdown_rx))));
    let accept_spec = UnitSpec::new("proxy.accept", UnitKind::LongLived, RestartPolicy::Never, {
        let state = Arc::clone(&proxy_state);
        let accept_boot = Arc::clone(&accept_boot);
        move || {
            let taken = accept_boot.lock().ok().and_then(|mut slot| slot.take());
            let state = Arc::clone(&state);
            async move {
                let Some((listener, shutdown)) = taken else {
                    debug!("proxy.accept factory ran without a listener; reporting failure");
                    return ExitStatus::failure("accept body rebuilt without a listener");
                };
                // a refusal to serve (the configuration was not readable
                // when the loop started) reports as a unit failure, which
                // the boot watchdog turns into a clean boot abort
                let served = accept_loop(state, listener, shutdown).await;
                if served {
                    ExitStatus::Success
                } else {
                    ExitStatus::failure("configuration unavailable at accept loop start")
                }
            }
        }
    })
    .with_service_kind(ServiceKind::Accept)
    // the accept unit reads the configuration and the cache; sibling units
    // never reach them through the subtree rule, so the grants ride its
    // spawn like inherited fds
    .with_grant(config_pid)
    .with_grant(cache_pid);
    match tree.spawn_unit(Pid::INIT, accept_spec) {
        Ok(pid) => {
            info!(bind = %local_addr, pid = %pid, "proxy listening");
        }
        Err(error) => {
            return Err(boot_abort(
                &tree,
                &service,
                CliError::Proxy(format!("failed to spawn proxy.accept: {error}")),
            )
            .await);
        }
    }

    // ---- periodic stats line
    let stats_state = Arc::clone(&proxy_state);
    let stats_spec = UnitSpec::new(
        "stats.every-60s",
        UnitKind::Every,
        RestartPolicy::Never,
        move || {
            let state = Arc::clone(&stats_state);
            async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    // ipc-migration: Proxy.Stats — counters (limiter, atomics) in-process, backends through the route unit
                    log_stats(&state).await;
                }
            }
        },
    )
    .with_service_kind(ServiceKind::Stats)
    // the stats unit reads routing over IPC; as a sibling of the route
    // unit the subtree rule does not authorize the call, so the grant
    // rides its spawn like an inherited fd
    .with_grant(route_pid);
    match tree.spawn_unit(Pid::INIT, stats_spec) {
        Ok(pid) => debug!(pid = %pid, "stats.every-60s unit spawned"),
        Err(error) => {
            return Err(boot_abort(
                &tree,
                &service,
                CliError::Runtime(format!("failed to spawn stats.every-60s: {error}")),
            )
            .await);
        }
    }

    // ---- interactive console REPL (only when stdin is a TTY)
    let console_spec = repl::console_spec(
        Arc::clone(&dispatcher),
        effects.clone(),
        repl::ConsoleWiring {
            service: service.clone(),
            shutdown_handle: shutdown,
            shutdown_tx: shutdown_tx.clone(),
            use_ansi,
            directory: console_directory,
            locales: console_locales,
            watch: console_watch,
        },
    );
    match tree.spawn_unit(Pid::INIT, console_spec) {
        Ok(pid) => debug!(pid = %pid, "console unit spawned"),
        Err(error) => {
            return Err(boot_abort(
                &tree,
                &service,
                CliError::Runtime(format!("failed to spawn console: {error}")),
            )
            .await);
        }
    }

    // ---- boot fail-fast: before entering the serving loop, harvest
    // anything that already failed at birth (e.g. the console unit
    // failing to spawn its blocking loop is impossible today; an accept
    // body could fail immediately in principle)
    if let Some(detail) = unit_failure_detail(&tree) {
        return Err(boot_abort(
            &tree,
            &service,
            CliError::Proxy(format!("supervision tree aborted during boot: {detail}")),
        )
        .await);
    }

    // ---- wait for shutdown: Ctrl-C, the console REPL flipping the flag,
    // or a serving unit dying under the watch
    let mut unit_tick = tokio::time::interval(UNIT_FAILURE_POLL);
    unit_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let unit_failure: Option<String> = loop {
        tokio::select! {
            biased;

            result = tokio::signal::ctrl_c() => {
                match result {
                    Ok(()) => info!("Ctrl-C received; shutting down"),
                    Err(error) => warn!(%error, "failed to listen for Ctrl-C; continuing until told otherwise"),
                }
                // flip the shared console flag too, so the REPL's watch
                // loop sees the shutdown the same way /candyriya end is seen
                let _ = shutdown_ctrlc.trigger();
                break None;
            }
            result = console_rx.changed() => {
                if result.is_ok() {
                    info!("shutdown requested via the console; shutting down");
                }
                break None;
            }
            _ = unit_tick.tick() => {
                // conns and plugin units exit; harvest their zombies here
                // so the registry stays flat, ps tells the truth mid-run
                // and the restart policy of plugin units actually fires
                reap_group_children(&tree);
                if let Some(detail) = unit_failure_detail(&tree) {
                    break Some(detail);
                }
            }
        }
    };
    if let Some(detail) = unit_failure {
        return Err(boot_abort(
            &tree,
            &service,
            CliError::Proxy(format!("supervision tree aborted: {detail}")),
        )
        .await);
    }

    // ---- the cascade: watch + tree-wide cancel sweep, serving units,
    // finalizers last (cache then metrics), drain as the final safety net
    cascade_shutdown(&tree, &service, &shutdown_tx, cache, metrics_handle).await;

    Ok(())
}

/// Log one compact stats line for the periodic unit.
///
/// The backend counters are read from the `route` unit as one snapshot; on
/// a route-unit outage the line is logged without them (and the error with
/// it) — fabricating zeros would look like "all backends offline", which is
/// a different, stronger claim than "unknown".
// ipc-migration: Proxy.Stats (reads ProxyState counters and limiter directly)
async fn log_stats(state: &ProxyState) {
    let snapshot = match state.route(RouteMsg::Snapshot).await {
        Ok(RouteResp::Snapshot(snapshot)) => Some(snapshot),
        Ok(other) => {
            error!(reply = ?other, "unexpected route unit reply to a Snapshot call");
            None
        }
        Err(error) => {
            error!(%error, "route unit unavailable; backend counts omitted from the stats line");
            None
        }
    };
    match snapshot {
        Some(entries) => {
            let backends_online = entries
                .iter()
                .filter(|entry| entry.state() == candyriya_route::BackendState::Online)
                .count();
            info!(
                active_connections = state.limiter.current_connections(),
                accepted = state.conn_counter.load(Ordering::Relaxed),
                rejected = state.connection_rejections.load(Ordering::Relaxed),
                backends_online,
                backends_total = entries.len(),
                "proxy stats"
            );
        }
        None => {
            info!(
                active_connections = state.limiter.current_connections(),
                accepted = state.conn_counter.load(Ordering::Relaxed),
                rejected = state.connection_rejections.load(Ordering::Relaxed),
                "proxy stats"
            );
        }
    }
}

/// Load the configured favicon into the cache unconditionally.
///
/// Used by `/candyriya reload` to drop a stale cached icon and by the boot
/// warm-up. The reload closure runs synchronously on the REPL's blocking
/// thread, so that caller drives this future with a blocking hand-off (the
/// same pattern the directory uses); the async boot path awaits it directly.
/// Returns `true` when the cache now holds the configured icon; `false` keeps
/// whatever the cache held before (or the embedded fallback).
async fn refresh_favicon(config: &Config, favicon_cache: Option<&FaviconCache>) -> bool {
    const MAX_FAVICON_BYTES: u64 = 1024 * 1024;
    let Some(cache) = favicon_cache else {
        return false;
    };
    let path = Path::new(&config.motd_favicon);
    match std::fs::read(path) {
        Ok(bytes) => {
            let magic_ok = bytes.len() >= 8 && bytes.starts_with(b"\x89PNG\r\n\x1a\n");
            if magic_ok && (bytes.len() as u64) <= MAX_FAVICON_BYTES {
                cache.put(&bytes).await;
                info!(path = %path.display(), bytes = bytes.len(), "favicon loaded and cached");
                true
            } else {
                warn!(
                    path = %path.display(),
                    bytes = bytes.len(),
                    "favicon is not a PNG within the size cap; the embedded fallback icon will be served"
                );
                false
            }
        }
        Err(error) => {
            debug!(
                path = %path.display(),
                %error,
                "favicon unreadable; the embedded fallback icon will be served"
            );
            false
        }
    }
}

/// Reload the configuration phase: re-read the file, swap atomically,
/// rebuild what was derived from it. The forwarding secret is deliberately
/// **not** re-read or rotated — it is a once-per-process credential. The
/// listener socket and live sessions are likewise untouched.
///
/// On any failure the running configuration is left untouched (the
/// [`ConfigStore::reload`] contract); the routing registry and the
/// favicon are only swapped when their own rebuild succeeds, so a
/// half-applied reload cannot leak out. The plugin-facing backend name
/// list rides the same store the route unit serves, so a successful
/// reload is visible to the next host-function call without any
/// re-registration.
/// Returns an operator-facing note on success.
///
/// The favicon swap reaches the `cache` unit through the tree: this phase
/// runs synchronously on the REPL's blocking thread, so the IPC round trip
/// is driven inline with a blocking hand-off — the same bridge the command
/// directory uses for its config and route calls.
// ipc-migration: Config.Reload / Route.Replace / I18n.Reload — the phase swaps shared state straight across subsystem boundaries
fn reload_config_phase(
    store: &ConfigStore,
    config_path: &Path,
    proxy_state: &ProxyState,
    favicon_cache: Option<&FaviconCache>,
    locales: &candyriya_command::Locales,
    runtime: &tokio::runtime::Handle,
) -> Result<String, String> {
    let config = store
        .reload(config_path)
        .map_err(|error| error.to_string())?;
    let mut notes = Vec::new();

    // backends: rebuild the registry from the new configuration and swap
    // it atomically; on failure the previous routing stays in place
    if config.servers.servers.is_empty() {
        // ipc-migration: Route.Replace (swap-through-the-shared-state path)
        proxy_state.set_router(None);
        notes.push("routing disabled (no [servers] configured)".to_owned());
    } else {
        match candyriya_proxy::registry_from_config(&config) {
            Ok(registry) => {
                let count = registry.iter().count();
                // ipc-migration: Route.Replace (swap-through-the-shared-state path)
                proxy_state.set_router(Some(Arc::new(registry)));
                notes.push(format!("{count} backends re-registered"));
            }
            Err(message) => {
                tracing::error!(
                    %message,
                    "backend registry rebuild failed; keeping the previous routing"
                );
                notes.push(format!("backends kept from before the reload ({message})"));
            }
        }
    }

    // the favicon rides the tree: driven inline on the blocking REPL
    // thread, exactly the way the directory drives its config and route
    // calls; a failure of the cache unit degrades to the disk read and the
    // embedded fallback, never to a failed ping
    if runtime.block_on(refresh_favicon(&config, favicon_cache)) {
        notes.push("favicon refreshed".to_owned());
    }

    // ipc-migration: I18n.Reload — re-reads the locale tables in place
    match locales.reload() {
        Ok(()) => notes.push("locales reloaded".to_owned()),
        Err(error) => {
            tracing::warn!(%error, "locale reload failed; keeping the previous tables");
            notes.push(format!("locales kept from before the reload ({error})"));
        }
    }

    Ok(notes.join("; "))
}

/// Wiring the plugin reload phase drives: the reload-time handles plus
/// the supervision plumbing. One bundle instead of a dozen loose handles —
/// boot and `/candyriya reload` both go through the same names.
struct PluginReloadWiring<'a> {
    store: &'a ConfigStore,
    data_dir: &'a Path,
    plugin_set: &'a PluginSet,
    cache: &'a CacheManager,
    services: &'a ServiceRegistry,
    host_ipc: &'a HostIpc,
    plugin_locales: &'a SharedLocales,
    tree: &'a Init,
    plugins_pid: Pid,
    service: &'a TaskService,
    plugin_health: PluginHealthSet,
}

/// Reload the plugin phase: re-stage every running plugin from disk and
/// register newcomers found under the roots. The running set survives
/// any failure — broken replacements never evict a working instance.
/// Reloaded plugins resolve their host functions through the same
/// `host` unit the startup-loaded ones do. After the reload, the
/// supervision tree is brought back in line: units appear for plugins
/// that entered the set and disappear for ids that left it — the tree
/// is the hierarchy, never a forked one.
// ipc-migration: Plugins.Reload — the phase reaches the plugin set, the shared registry and the cache across subsystem boundaries
fn reload_plugins_phase(wiring: &PluginReloadWiring<'_>) -> candyriya_command::PluginPhaseReport {
    let plugin_health = wiring.plugin_health.clone();
    // limits come from the current configuration, i.e. the post-config-reload
    // one when both phases run — a raised [plugins] cap applies immediately
    let config = wiring.store.get();
    let limits = LimitBuilder::from_config(&config).build();
    let reloader = PluginReloader::with_limits(
        wiring.plugin_set.clone(),
        wiring.data_dir.to_path_buf(),
        wiring.services.clone(),
        limits.clone(),
    )
    .with_locales(wiring.plugin_locales.clone())
    .with_host_ipc(wiring.host_ipc.clone())
    .with_plugin_health(plugin_health.clone());
    let loader = PluginLoader::with_limits(
        wiring.data_dir.join("plugins"),
        wiring.data_dir.join("libraries"),
        wiring.cache,
        limits.clone(),
    )
    .with_locales(wiring.plugin_locales.clone())
    .with_host_ipc(wiring.host_ipc.clone())
    .with_plugin_health(plugin_health.clone());
    let report = reloader.reload_all(&loader);
    // the tree is the hierarchy: a newly discovered plugin gets its unit,
    // an id that left the set loses it; spawn failures are loud but do
    // not fail the reload — the plugin still runs, just unsupervised
    let units = sync_plugin_units(
        wiring.tree,
        wiring.plugins_pid,
        wiring.plugin_set,
        &PluginUnitContext {
            service: wiring.service.clone(),
            reloader: reloader.clone(),
            health: plugin_health,
        },
    );
    for id in &units.spawned {
        info!(plugin = %id, "plugin supervision unit spawned by reload");
    }
    for id in &units.cancelled {
        info!(plugin = %id, "plugin supervision unit cancelled by reload");
    }

    let mut details = Vec::new();
    for missing in &report.missing {
        warn!(
            plugin = %missing,
            "plugin directory is gone; the running plugin was kept"
        );
        details.push(format!(
            "{missing}: files are gone; the running plugin was kept"
        ));
    }
    for (where_, why) in &report.issues {
        warn!(path = %where_.display(), "plugin reload: {why}");
    }
    for id in &report.loaded {
        info!(plugin = %id, "new plugin registered by reload");
    }
    for (id, error) in &report.failed {
        error!(
            plugin = %id,
            %error,
            "plugin reload failed; the running instance (if any) was kept"
        );
        details.push(format!("{id}: {error}"));
    }
    for (id, reason) in &report.new_failed {
        error!(plugin = %id, %reason, "new plugin failed to load");
        details.push(format!("{id}: {reason}"));
    }
    for (id, why) in &units.failed {
        error!(plugin = %id, %why, "plugin supervision unit failed to spawn");
        details.push(format!("{id}: supervision unit failed to spawn: {why}"));
    }

    let reloaded = report.reloaded.len() + report.loaded.len();
    let failed = report.failed.len() + report.new_failed.len();
    candyriya_command::PluginPhaseReport {
        reloaded,
        failed,
        details,
    }
}

/// Assemble the reload closure the `/candyriya reload` command runs.
///
/// The closure is synchronous and total: every failure is reported through
/// the returned report, never panicked. Scope selects the phases; the
/// default scope is both, config first.
///
/// `runtime` is the handle the closure uses to drive its IPC calls inline —
/// the closure runs on the REPL's blocking thread, never on an async worker.
// ipc-migration: Config.Reload / Plugins.Reload — ReloadFn runs synchronously on the REPL's blocking thread
#[allow(clippy::too_many_arguments)]
fn build_reload_fn(
    store: Arc<ConfigStore>,
    config_path: PathBuf,
    proxy_state: Arc<ProxyState>,
    favicon_cache: Option<Arc<FaviconCache>>,
    locales: candyriya_command::Locales,
    plugin_set: Arc<PluginSet>,
    cache: Arc<CacheManager>,
    services: ServiceRegistry,
    host_ipc: HostIpc,
    data_dir: &Path,
    plugin_locales: SharedLocales,
    runtime: tokio::runtime::Handle,
    tree: Init,
    plugins_pid: Pid,
    service: TaskService,
    plugin_health: PluginHealthSet,
) -> candyriya_command::ReloadFn {
    use candyriya_command::{ReloadReport, ReloadScope};

    let data_dir = data_dir.to_path_buf();
    Arc::new(move |scope: ReloadScope| -> ReloadReport {
        let config_phase = matches!(scope, ReloadScope::All | ReloadScope::Config);
        let plugin_phase = matches!(scope, ReloadScope::All | ReloadScope::Plugins);

        let config = config_phase.then(|| {
            reload_config_phase(
                &store,
                &config_path,
                &proxy_state,
                favicon_cache.as_deref(),
                &locales,
                &runtime,
            )
        });
        let plugins = plugin_phase.then(|| {
            reload_plugins_phase(&PluginReloadWiring {
                store: &store,
                data_dir: &data_dir,
                plugin_set: &plugin_set,
                cache: &cache,
                services: &services,
                host_ipc: &host_ipc,
                plugin_locales: &plugin_locales,
                tree: &tree,
                plugins_pid,
                service: &service,
                plugin_health: plugin_health.clone(),
            })
        });

        ReloadReport { config, plugins }
    })
}

/// Cache the configured favicon on first use; fall back to the embedded icon.
async fn warm_favicon(config: &Config, favicon_cache: Option<&FaviconCache>) {
    let Some(cache) = favicon_cache else {
        return; // no cache: the status handler reads from disk per request
    };
    if cache.get().await.is_some() {
        debug!("favicon already present in the cache");
        return;
    }
    let _ = refresh_favicon(config, Some(cache)).await;
}

/// Build the locale handle the proxy uses to render kick screens for
/// plugin-vetoed actions.
///
/// Same shape as the command locales: the configured locale, with the
/// data-dir `locales/` overrides layered on top when the directory
/// exists. An unusable override directory degrades to the embedded
/// tables with a warning; an embedded failure is a build defect and
/// fails hard, exactly like [`build_locales`].
fn build_kick_i18n(data_dir: &Path, config: &Config) -> Result<I18n, CliError> {
    let dir = data_dir.join("locales");
    match I18n::with_override_dir(&config.locale, &dir) {
        Ok(i18n) => Ok(i18n),
        Err(error) => {
            warn!(
                %error,
                locale = %config.locale,
                "failed to build the kick locale from the override directory; using the embedded default"
            );
            I18n::new("en").map_err(|embedded| {
                CliError::Runtime(format!("embedded locales are broken: {embedded}"))
            })
        }
    }
}

/// Build the command locales plus the shared pair the plugin host
/// functions resolve through.
///
/// `<data_dir>/locales/{en,ex_girl}/*.toml` overrides the embedded tables;
/// a broken override degrades to embedded with a warning instead of blocking
/// startup, and an embedded failure is a build defect and fails hard. The
/// returned [`SharedLocales`] is built from the same underlying handles, so
/// plugin registrations and `/candyriya reload` apply to both.
fn build_locales(data_dir: &Path) -> Result<(candyriya_command::Locales, SharedLocales), CliError> {
    let pair = |en: I18n, ex_girl: I18n| {
        (
            candyriya_command::Locales::new(en.clone(), ex_girl.clone()),
            SharedLocales::new(en, ex_girl),
        )
    };
    let locales_dir = data_dir.join("locales");
    if locales_dir.is_dir() {
        match (
            I18n::with_override_dir("en", &locales_dir),
            I18n::with_override_dir("ex_girl", &locales_dir),
        ) {
            (Ok(en), Ok(ex_girl)) => return Ok(pair(en, ex_girl)),
            (en, ex_girl) => {
                if let Err(error) = en {
                    warn!(%error, "failed to load en locale overrides; using embedded locale");
                }
                if let Err(error) = ex_girl {
                    warn!(%error, "failed to load ex_girl locale overrides; using embedded locale");
                }
            }
        }
    }
    // embedded fallback: one pair of handles, cloned for the command and
    // plugin sides so registrations and reloads stay in sync
    match (I18n::new("en"), I18n::new("ex_girl")) {
        (Ok(en), Ok(ex_girl)) => Ok(pair(en, ex_girl)),
        (Err(error), _) | (_, Err(error)) => Err(CliError::Runtime(format!(
            "embedded locales are broken: {error}"
        ))),
    }
}

/// Scan `plugins/` and `libraries/`, resolve the dependency order, and build
/// the shared plugin set. Failures are logged, never fatal: a broken plugin
/// must not take the proxy down.
fn load_plugins(
    data_dir: &Path,
    cache: &CacheManager,
    plugin_locales: SharedLocales,
    host_ipc: HostIpc,
    limits: Limits,
    plugin_health: PluginHealthSet,
) -> Arc<PluginSet> {
    let plugins_dir = data_dir.join("plugins");
    let libraries_dir = data_dir.join("libraries");
    // the data-dir layout ships both roots; create them so a first run does
    // not warn about missing directories
    for dir in [&plugins_dir, &libraries_dir] {
        if let Err(error) = std::fs::create_dir_all(dir) {
            warn!(path = %dir.display(), %error, "failed to create the plugin directory");
        }
    }
    let loader = PluginLoader::with_limits(plugins_dir, libraries_dir, cache, limits)
        .with_locales(plugin_locales)
        .with_host_ipc(host_ipc)
        .with_plugin_health(plugin_health);
    let report = loader.load();
    for (where_, why) in &report.issues {
        warn!(path = %where_.display(), "plugin: {why}");
    }
    let plugins = match resolve_plugins(report.plugins) {
        Ok(resolution) => {
            for (where_, why) in &resolution.report.issues {
                warn!(path = %where_.display(), "plugin: {why}");
            }
            resolution.report.plugins
        }
        Err(error) => {
            error!("plugin dependency resolution failed: {error}; continuing with no plugins");
            Vec::new()
        }
    };
    let loaded = plugins
        .iter()
        .filter(|plugin| plugin.state().is_loaded())
        .count();
    let ids: Vec<&str> = plugins.iter().map(|plugin| plugin.id()).collect();
    info!(
        loaded,
        total = plugins.len(),
        ?ids,
        "plugin startup load complete"
    );
    Arc::new(PluginSet::from_vec(plugins))
}

/// Resolve a `host:port` from the configuration into a socket address.
///
/// Plain `IP:port` values parse directly; hostnames go through the system
/// resolver (blocking, once, at startup — before anything serves).
fn resolve_socket_addr(input: &str) -> Result<SocketAddr, CliError> {
    if let Ok(addr) = input.parse::<SocketAddr>() {
        return Ok(addr);
    }
    use std::net::ToSocketAddrs;
    input
        .to_socket_addrs()
        .map_err(|error| CliError::Bind(format!("{input:?} does not resolve: {error}")))?
        .next()
        .ok_or_else(|| CliError::Bind(format!("{input:?} resolved to no addresses")))
}

/// `main` helper: run the process and map the outcome to an exit code.
pub fn run_to_exit_code(args: Args) -> ExitCode {
    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_tree() -> Init {
        let service = TaskService::builder()
            .handle(tokio::runtime::Handle::current())
            .blocking_threads(1)
            .build()
            .expect("service builds");
        Init::new(service)
    }

    async fn wait_zombies(tree: &Init, want: u64) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if tree.zombie_count() == want {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for zombies"
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    #[test]
    fn exit_is_failure_covers_the_failure_vocabulary() {
        assert!(exit_is_failure(&ExitStatus::failure("x")));
        assert!(exit_is_failure(&ExitStatus::Panicked));
        assert!(exit_is_failure(&ExitStatus::TimedOut));
        assert!(!exit_is_failure(&ExitStatus::Success));
        assert!(!exit_is_failure(&ExitStatus::Cancelled));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unit_failure_detail_reports_failed_units_only() {
        let tree = test_tree();
        tree.spawn_unit(
            Pid::INIT,
            UnitSpec::new(
                "test.fails",
                UnitKind::Oneshot,
                RestartPolicy::Never,
                || async { ExitStatus::failure("boom") },
            ),
        )
        .expect("failing unit spawns");
        tree.spawn_unit(
            Pid::INIT,
            UnitSpec::new(
                "test.ok",
                UnitKind::Oneshot,
                RestartPolicy::Never,
                || async { ExitStatus::Success },
            ),
        )
        .expect("succeeding unit spawns");
        wait_zombies(&tree, 2).await;

        let detail = unit_failure_detail(&tree).expect("the failure must be reported");
        assert!(detail.contains("test.fails"), "unexpected detail: {detail}");
        // both zombies were harvested; the ring holds both exits
        assert_eq!(tree.zombie_count(), 0);
        assert_eq!(tree.recent_exits(2).len(), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unit_failure_detail_stays_silent_without_failures() {
        let tree = test_tree();
        tree.spawn_unit(
            Pid::INIT,
            UnitSpec::new(
                "test.ok",
                UnitKind::Oneshot,
                RestartPolicy::Never,
                || async { ExitStatus::Success },
            ),
        )
        .expect("succeeding unit spawns");
        wait_zombies(&tree, 1).await;

        assert!(unit_failure_detail(&tree).is_none());
        assert_eq!(tree.zombie_count(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reap_group_children_harvests_group_zombies() {
        let tree = test_tree();
        let group_pid = tree
            .spawn_unit(
                Pid::INIT,
                UnitSpec::new(
                    "plugins",
                    UnitKind::LongLived,
                    RestartPolicy::Never,
                    std::future::pending::<ExitStatus>,
                ),
            )
            .expect("plugins group spawns");
        let child_pid = tree
            .spawn_unit(
                group_pid,
                UnitSpec::new(
                    "plugin-x",
                    UnitKind::Oneshot,
                    RestartPolicy::Never,
                    || async { ExitStatus::Success },
                ),
            )
            .expect("group child spawns");
        wait_zombies(&tree, 1).await;

        reap_group_children(&tree);
        assert_eq!(tree.zombie_count(), 0);
        assert!(
            tree.unit(child_pid).is_none(),
            "the group reaper removed the harvested child"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_group_child_failure_does_not_trip_the_boot_watch() {
        // a crashed plugin must never abort the proxy: the fail-fast
        // watchdog looks at the root's direct children only, so a failure
        // inside a group stays inside the group and never becomes a boot
        // abort detail
        let tree = test_tree();
        let group_pid = tree
            .spawn_unit(
                Pid::INIT,
                UnitSpec::new(
                    "plugins",
                    UnitKind::LongLived,
                    RestartPolicy::Never,
                    std::future::pending::<ExitStatus>,
                ),
            )
            .expect("plugins group spawns");
        tree.spawn_unit(
            group_pid,
            UnitSpec::new(
                "plugin-x",
                UnitKind::Oneshot,
                RestartPolicy::Never,
                || async { ExitStatus::failure("a crashed plugin") },
            ),
        )
        .expect("group child spawns");
        wait_zombies(&tree, 1).await;

        assert!(
            unit_failure_detail(&tree).is_none(),
            "a group child's failure must not reach the root's fail-fast watch"
        );
        // the group reaper harvests it instead, like the mid-run tick does
        reap_group_children(&tree);
        assert_eq!(tree.zombie_count(), 0);
        assert!(unit_failure_detail(&tree).is_none());
    }
}
