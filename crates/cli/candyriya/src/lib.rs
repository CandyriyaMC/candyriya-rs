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
//! 5. **Subsystems** — the cache manager (favicon uniquification), the
//!    Prometheus exporter (only when `[metrics] prometheus = true`), the
//!    Mojang/offline session validators selected from `online-mode`, the
//!    routing registry, the command tree and the interactive console REPL.
//!    Routing reads go to the `route` unit through the same IPC plane —
//!    login, the stats line and the directory all call it; `/candyriya
//!    reload` swaps the very store the unit serves, so a reload is visible
//!    to the next call. The plugin-facing host services the `candyriya:*`
//!    functions resolve through — the plugin dispatcher, the play-state
//!    roster snapshot and the backend name list — are registered right
//!    after the plugin load.
//!    All of the above is the loader phase: everything that ran before
//!    the task service existed is adopted into the tree as an
//!    already-exited unit once the tree is up.
//! 6. **Lifecycle** — once the loader is done, the phases that keep
//!    running boot as units in `init`: `config` (the configuration source
//!    of truth), `proxy.accept` (long-lived, the unit body drives the
//!    listener), `stats.every-60s` and `players.roster-refresh`
//!    (periodic), `console` (the interactive REPL) and, at shutdown,
//!    `cache.shutdown` and `metrics.shutdown` (one-shot finalizers, the
//!    last units to run, in order). Ctrl-C or
//!    `/candyriya end` fires both the shutdown watch and a tree-wide
//!    cancel sweep; the serving units are waited on through the tree, the
//!    finalizers run last, the task service drains as the final safety
//!    net, and the process exits with code 0. If a boot unit fails before
//!    or during serving, the remaining units are unwound in reverse spawn
//!    order instead and the process exits with code 1.
//!
//! Plugins under `plugins/` and `libraries/` are loaded at startup; the
//! same machinery is exposed through `/candyriya reload` for a restart-free
//! refresh. What never moves on a reload: the listener socket, live
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

use candyriya_auth::{AuthConfig, AuthMode, MojangSessionValidator, OfflineSessionValidator};
use candyriya_cache::CacheManager;
use candyriya_command::{CommandRegistry, CommandState, ShutdownHandle};
use candyriya_config::{Config, ConfigStore, DEFAULT_CONFIG_TOML};
use candyriya_i18n::I18n;
use candyriya_init::{
    ExitStatus, Init, Pid, ReapOutcome, RestartPolicy, ServiceKind, UnitKind, UnitSpec,
};
use candyriya_plugin::{
    DISPATCHER_SERVICE_KEY, LimitBuilder, PLAYERS_ROSTER_KEY, PluginLoader, PluginReloader,
    PluginSet, ProxyEventDispatcher, SERVERS_NAMES_KEY, ServiceRegistry, SharedLocales,
    resolve_plugins,
};
use candyriya_plugin_api::{EventDispatcher, PlayerRef};
use candyriya_proxy::{
    FaviconCache, ProxySocket, ProxyState, SocketConfig, accept_loop, config_spec,
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

/// Unwind a boot that is not allowed to continue serving.
///
/// Cancels the units already spawned in reverse spawn order (newest
/// first), harvests whatever already exited, drains the kernel and hands
/// the reason back for the caller to report. Boot-adopted units cannot
/// appear here; they are reaped as they adopt. The drain is the same
/// safety net the normal shutdown uses, so a unit wedged mid-await still
/// gets bounded by it.
async fn boot_abort(
    tree: &Init,
    service: &TaskService,
    spawned: &[(Pid, &'static str)],
    error: CliError,
) -> CliError {
    // newest first, mirroring how the shutdown cascade unwinds
    for (pid, _) in spawned.iter().rev() {
        if let Err(cancel_error) = tree.cancel(*pid) {
            debug!(%cancel_error, %pid, "unit already gone while unwinding the boot abort");
        }
    }
    let _ = tree.reap_children(Pid::INIT);
    let report = service.drain(Duration::from_secs(10)).await;
    info!(?report, "task service drained after boot abort");
    error
}

/// Wait until every pid in `pids` has left the registry (harvested by the
/// reaper) or the budget runs out. Returns the pids still tracked.
async fn wait_units_gone(tree: &Init, pids: &[Pid], budget: Duration) -> Vec<Pid> {
    let start = Instant::now();
    let mut pending: Vec<Pid> = pids.to_vec();
    while !pending.is_empty() && start.elapsed() < budget {
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
/// cancel the serving units in reverse spawn order, wait for them through
/// the tree, run the finalizers in order (`cache.shutdown` then
/// `metrics.shutdown`), and drain the kernel as the final safety net. The
/// exit ring keeps every unit exit for the post-mortem.
async fn cascade_shutdown(
    tree: &Init,
    service: &TaskService,
    shutdown_tx: &tokio::sync::watch::Sender<bool>,
    boot_units: &[(Pid, &'static str)],
    cache: Arc<CacheManager>,
    metrics_handle: candyriya_metrics::MetricsHandle,
) {
    // bridge: the accept loop and the REPL still watch the flip
    let _ = shutdown_tx.send(true);

    // stop the serving units through the tree, newest first: console,
    // players.roster-refresh, stats.every-60s, proxy.accept, route.health,
    // route. route.health is a child of route, so cancelling it before route
    // keeps the order; when route exits the child re-parents to the root
    // and the wait below reaps it there
    for (pid, _) in boot_units.iter().rev() {
        if let Err(error) = tree.cancel(*pid) {
            debug!(%error, %pid, "unit already gone in the cancel sweep");
        }
    }

    // wait for the serving units to exit; the reaper stays warm
    let pids: Vec<Pid> = boot_units.iter().map(|(pid, _)| *pid).collect();
    let pending = wait_units_gone(tree, &pids, UNIT_STOP_TIMEOUT).await;
    if !pending.is_empty() {
        let names: Vec<&str> = boot_units
            .iter()
            .filter(|(pid, _)| pending.contains(pid))
            .map(|(_, name)| *name)
            .collect();
        warn!(
            ?names,
            "serving units did not stop before the cascade deadline; the drain deadline applies"
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
    // path swaps through it — the names list is collected once here from
    // the same registry that lands in the store
    let router_names: Option<Vec<String>> = router.as_ref().map(|registry| {
        registry
            .iter()
            .map(|backend| backend.name().to_owned())
            .collect()
    });
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
                &[],
                CliError::Runtime(format!("failed to spawn config: {error}")),
            )
            .await);
        }
    };
    // the config unit alone in the first phase of boot: unwinds with the
    // other early failures so it never outlives an aborted boot
    let config_unwind = [(config_pid, "config")];

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
                &config_unwind,
                CliError::Runtime(format!("failed to spawn route: {error}")),
            )
            .await);
        }
    };
    // the route unit joins the first boot phase too: early boot failures
    // unwind it alongside the config unit
    let config_unwind = [(config_pid, "config"), (route_pid, "route")];

    // ---- the route.health child: the health-check loop lives with the
    // route unit. Spawned only when [servers] is non-empty at boot — with
    // nothing to check the child would be dead weight. If a later
    // /candyriya reload adds servers, nothing watches them here; the
    // reload phase owns routing changes and does not start health checks
    // (that belongs to the reload phase, not this slice).
    let health_boot: Option<(Pid, &'static str)> = if config.servers.servers.is_empty() {
        info!("no [servers] configured; backend health checks are not wired");
        None
    } else {
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
                Some((pid, "route.health"))
            }
            Err(error) => {
                return Err(boot_abort(
                    &tree,
                    &service,
                    &config_unwind,
                    CliError::Runtime(format!("failed to spawn route.health: {error}")),
                )
                .await);
            }
        }
    };

    let registry = ServiceRegistry::new();
    // ipc-migration: dissolve with the host-function IPC conversion; key has zero prod readers (write-only)
    registry.register_service(
        "scheduler.service".to_string(),
        Arc::new(service.clone()) as Arc<dyn std::any::Any + Send + Sync>,
    );

    // ---- cache (favicon uniquification goes through here)
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
            return Err(boot_abort(
                &tree,
                &service,
                &config_unwind,
                CliError::Cache(error.to_string()),
            )
            .await);
        }
    };
    // ipc-migration: dissolve with the host-function IPC conversion; key has zero prod readers (write-only)
    registry.register_service(
        "cache.manager".to_string(),
        Arc::clone(&cache) as Arc<dyn std::any::Any + Send + Sync>,
    );

    let favicon_cache: Option<Arc<FaviconCache>> =
        match FaviconCache::new(&cache, Duration::from_secs(3600)) {
            Ok(favicon_cache) => Some(Arc::new(favicon_cache)),
            Err(error) => {
                warn!(%error, "favicon cache unavailable; favicons are read from disk per ping");
                None
            }
        };
    warm_favicon(&config, favicon_cache.as_deref());

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
                return Err(boot_abort(&tree, &service, &config_unwind, error).await);
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
            return Err(boot_abort(
                &tree,
                &service,
                &config_unwind,
                CliError::Metrics(error.to_string()),
            )
            .await);
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
    let offline_validator = Arc::new(OfflineSessionValidator);
    let mojang_validator = Arc::new(MojangSessionValidator::new(
        auth.session_server_url.clone(),
        auth.timeout,
    ));
    info!(mode = ?auth.mode, "session validation configured");
    adopt_bootstrap(&tree, "auth.validators", ExitStatus::Success);
    // ipc-migration: dissolve with the host-function IPC conversion; key has zero prod readers (write-only)
    registry.register_service(
        "auth.offline".to_string(),
        offline_validator as Arc<dyn std::any::Any + Send + Sync>,
    );
    // ipc-migration: dissolve with the host-function IPC conversion; key has zero prod readers (write-only)
    registry.register_service(
        "auth.mojang".to_string(),
        mojang_validator as Arc<dyn std::any::Any + Send + Sync>,
    );
    // ipc-migration: dissolve with the host-function IPC conversion; key has zero prod readers (write-only)
    registry.register_service(
        "config.store".to_string(),
        Arc::clone(&store) as Arc<dyn std::any::Any + Send + Sync>,
    );
    // the route.backends key is gone: routing reads the shared store the
    // route unit serves (and swaps on reload), and the key had zero prod
    // readers to begin with; no detached Arc escapes the store this way

    // ---- command locales + the kick-locale handle, plus the shared pair
    // the plugin host functions resolve through (built before the plugin
    // load so the loader can register plugin locale files into the same
    // machines). One bootstrap step, one adopted unit.
    let (locales, plugin_locales) = match build_locales(data_dir) {
        Ok(pair) => pair,
        Err(error) => {
            adopt_bootstrap(&tree, "i18n.build", ExitStatus::failure(error.to_string()));
            return Err(boot_abort(&tree, &service, &config_unwind, error).await);
        }
    };
    let kick_i18n = match build_kick_i18n(data_dir, &config) {
        Ok(i18n) => i18n,
        Err(error) => {
            adopt_bootstrap(&tree, "i18n.build", ExitStatus::failure(error.to_string()));
            return Err(boot_abort(&tree, &service, &config_unwind, error).await);
        }
    };
    adopt_bootstrap(&tree, "i18n.build", ExitStatus::Success);

    // ---- plugins: startup load (before the proxy state is built: the
    // shared dispatcher gets wired into it at construction time)
    let plugin_set = load_plugins(data_dir, &config, &cache, plugin_locales.clone());
    adopt_bootstrap(&tree, "plugins.load", ExitStatus::Success);
    // ipc-migration: dissolve with the host-function IPC conversion; key has zero prod readers (write-only)
    registry.register_service(
        "plugins.set".to_string(),
        Arc::clone(&plugin_set) as Arc<dyn std::any::Any + Send + Sync>,
    );
    // one dispatcher per process: the registry carries it for the host
    // functions, the proxy state carries it for the lifecycle events —
    // both hold handles to the same instance (registered inside
    // register_plugin_host_services below), so `/candyriya reload`
    // swaps what they both see without any re-registration
    let event_dispatcher = Arc::new(ProxyEventDispatcher::new((*plugin_set).clone()));

    // ---- shared proxy state
    // ipc-migration: these fields are cross-subsystem handles (Plugins.Dispatch
    // and I18n.T ride them); the IPC conversion gives each its own unit
    let started = Instant::now();
    let mut proxy_state = ProxyState::new(
        tree.clone(),
        config_pid,
        route_pid,
        Arc::clone(&router_store),
    );
    // ipc-migration: Roster/Route/Proxy snapshot — ProxyState carries handles across the subsystem boundary until each is a unit
    proxy_state.auth = auth;
    proxy_state.favicon_cache = favicon_cache.clone();
    proxy_state.event_dispatcher = Some(Arc::clone(&event_dispatcher) as Arc<dyn EventDispatcher>);
    proxy_state.event_i18n = Some(Arc::new(kick_i18n));
    let proxy_state = Arc::new(proxy_state);

    // ---- command tree: the registry itself is built after the plugin set
    // exists, so `/candyriya reload` can run both phases
    let shutdown = ShutdownHandle::new();
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
    let effects = Arc::new(CliEffects::new(use_ansi));
    let command_state = CommandState {
        locales: locales.clone(),
        directory: directory as Arc<dyn candyriya_command::NetworkDirectory>,
        effects: effects.clone() as Arc<dyn candyriya_command::ProxyEffects>,
    };

    // ---- plugin host services: without these, the candyriya:* host
    // functions fall back to their built-in answers (callApi errors out,
    // the roster reads empty, listServers answers ["local"])
    // ipc-migration: Roster.Snapshot / Router.Names — these keys exist only for the host-function path; both become capability calls in the IPC phase
    register_plugin_host_services(
        &registry,
        &proxy_state,
        router_names.as_deref(),
        &event_dispatcher,
    );
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
        data_dir,
        plugin_locales,
    );
    let command_registry =
        CommandRegistry::new(command_state, shutdown.clone()).with_reload(reload_fn);
    let dispatcher = Arc::new(command_registry.dispatcher().map_err(|error| {
        CliError::Runtime(format!("failed to assemble the built-in commands: {error}"))
    })?);
    // ipc-migration: dissolve with the host-function IPC conversion; key has zero prod readers (write-only)
    registry.register_service(
        "commands.dispatcher".to_string(),
        dispatcher.clone() as Arc<dyn std::any::Any + Send + Sync>,
    );

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
            return Err(boot_abort(&tree, &service, &config_unwind, error).await);
        }
    };
    let socket = match ProxySocket::bind(bind_addr, &SocketConfig::from(&config.advanced)) {
        Ok(socket) => socket,
        Err(error) => {
            adopt_bootstrap(&tree, "proxy.bind", ExitStatus::failure(error.to_string()));
            return Err(boot_abort(
                &tree,
                &service,
                &config_unwind,
                CliError::Proxy(error.to_string()),
            )
            .await);
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
    // them unwinds the ones before it in reverse order (fail-fast boot).
    // The config and route units are already up (spawned before the boot
    // abort machinery needs them) and ride in the list so a runtime
    // collapse is caught by the same watchdog as a serving unit dying;
    // route.health rides along too and is cancelled before its parent
    // (reverse spawn order).
    let mut boot_units: Vec<(Pid, &'static str)> =
        vec![(config_pid, "config"), (route_pid, "route")];
    if let Some((pid, name)) = health_boot {
        boot_units.push((pid, name));
    }

    // proxy.accept: the unit body drives listener.accept(); connections
    // keep their own kernel tasks until phase 3 turns them into units.
    // The listener and the shutdown receiver are not Clone, so they ride
    // in a single slot consumed by the one factory invocation a
    // RestartPolicy::Never unit ever gets; a re-invocation reports a
    // failure instead of serving on a stolen socket.
    let accept_boot = Arc::new(std::sync::Mutex::new(Some((listener, shutdown_rx))));
    let accept_spec = UnitSpec::new("proxy.accept", UnitKind::LongLived, RestartPolicy::Never, {
        let state = Arc::clone(&proxy_state);
        let service = service.clone();
        let accept_boot = Arc::clone(&accept_boot);
        move || {
            let taken = accept_boot.lock().ok().and_then(|mut slot| slot.take());
            let state = Arc::clone(&state);
            let service = service.clone();
            async move {
                let Some((listener, shutdown)) = taken else {
                    debug!("proxy.accept factory ran without a listener; reporting failure");
                    return ExitStatus::failure("accept body rebuilt without a listener");
                };
                // a refusal to serve (the configuration was not readable
                // when the loop started) reports as a unit failure, which
                // the boot watchdog turns into a clean boot abort
                let served = accept_loop(state, listener, shutdown, service).await;
                if served {
                    ExitStatus::Success
                } else {
                    ExitStatus::failure("configuration unavailable at accept loop start")
                }
            }
        }
    })
    .with_service_kind(ServiceKind::Accept)
    // the accept unit reads the configuration; sibling units never reach
    // it through the subtree rule, so the grant rides its spawn like an
    // inherited fd
    .with_grant(config_pid);
    match tree.spawn_unit(Pid::INIT, accept_spec) {
        Ok(pid) => {
            boot_units.push((pid, "proxy.accept"));
            info!(bind = %local_addr, pid = %pid, "proxy listening");
        }
        Err(error) => {
            return Err(boot_abort(
                &tree,
                &service,
                &boot_units,
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
        Ok(pid) => boot_units.push((pid, "stats.every-60s")),
        Err(error) => {
            return Err(boot_abort(
                &tree,
                &service,
                &boot_units,
                CliError::Runtime(format!("failed to spawn stats.every-60s: {error}")),
            )
            .await);
        }
    }

    // ---- play-state roster snapshot: rebuild every second from the live
    // roster; plugin-facing getPlayerRoster answers from the key
    let roster_state = Arc::clone(&proxy_state);
    let roster_registry = registry.clone();
    let roster_spec = UnitSpec::new(
        "players.roster-refresh",
        UnitKind::Every,
        RestartPolicy::Never,
        move || {
            let state = Arc::clone(&roster_state);
            let registry = roster_registry.clone();
            let initial = roster_snapshot(&state);
            // the comparison is cheap (a few small strings), so it happens
            // synchronously per tick and a no-change tick is just one pass
            async move {
                let mut last_registered = initial;
                loop {
                    tokio::time::sleep(ROSTER_REFRESH_INTERVAL).await;
                    // ipc-migration: Roster.Snapshot (reads the shared proxy state directly)
                    let current = roster_snapshot(&state);
                    if current != last_registered {
                        // swap without the registry's replace warning: a join/part
                        // is normal traffic, not an operator-actionable event
                        // ipc-migration: Roster.Snapshot — remove+register swap kept until the IPC conversion
                        registry.remove_service::<Vec<PlayerRef>>(PLAYERS_ROSTER_KEY);
                        registry.register_service(
                            PLAYERS_ROSTER_KEY.to_owned(),
                            Arc::new(current.clone()) as Arc<dyn std::any::Any + Send + Sync>,
                        );
                        last_registered = current;
                    }
                }
            }
        },
    )
    .with_service_kind(ServiceKind::Roster);
    match tree.spawn_unit(Pid::INIT, roster_spec) {
        Ok(pid) => boot_units.push((pid, "players.roster-refresh")),
        Err(error) => {
            return Err(boot_abort(
                &tree,
                &service,
                &boot_units,
                CliError::Runtime(format!("failed to spawn players.roster-refresh: {error}")),
            )
            .await);
        }
    }

    // ---- interactive console REPL (only when stdin is a TTY)
    let console_spec = repl::console_spec(
        service.clone(),
        Arc::clone(&dispatcher),
        effects.clone(),
        shutdown,
        shutdown_tx.clone(),
        use_ansi,
    );
    match tree.spawn_unit(Pid::INIT, console_spec) {
        Ok(pid) => boot_units.push((pid, "console")),
        Err(error) => {
            return Err(boot_abort(
                &tree,
                &service,
                &boot_units,
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
            &boot_units,
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
                break None;
            }
            result = console_rx.changed() => {
                if result.is_ok() {
                    info!("shutdown requested via the console; shutting down");
                }
                break None;
            }
            _ = unit_tick.tick() => {
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
            &boot_units,
            CliError::Proxy(format!("supervision tree aborted: {detail}")),
        )
        .await);
    }

    // ---- the cascade: watch + tree-wide cancel sweep, serving units,
    // finalizers last (cache then metrics), drain as the final safety net
    cascade_shutdown(
        &tree,
        &service,
        &shutdown_tx,
        &boot_units,
        cache,
        metrics_handle,
    )
    .await;

    Ok(())
}

/// How often the `players.roster` snapshot is rebuilt from the live
/// play-state roster. Plugin-facing `getPlayerRoster` answers from that
/// snapshot, so a join or a disconnect may lag by at most this interval
/// (plus task-scheduling jitter).
const ROSTER_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

/// Snapshot the live play-state roster into the `Vec<PlayerRef>` shape the
/// `players.roster` service key carries.
// ipc-migration: Roster.Snapshot (reads the shared proxy state directly)
fn roster_snapshot(state: &ProxyState) -> Vec<PlayerRef> {
    state
        .roster
        .snapshot()
        .into_iter()
        .map(|entry| PlayerRef {
            uuid: entry.uuid.to_string(),
            name: entry.name,
        })
        .collect()
}

/// Register the host services the `candyriya:*` plugin host functions
/// resolve through the registry.
///
/// - `plugin.dispatcher` — the process-wide dispatcher built at startup
///   from the very [`PluginSet`] hot reload mutates in place; the proxy
///   state holds the same instance for the lifecycle events, so both
///   sides see the same fan-out with no re-registration. The registry
///   just replaces on re-register.
/// - `players.roster` — an initial snapshot is registered here; the
///   `players.roster-refresh` unit (spawned with the rest of the boot
///   units) then rebuilds it every [`ROSTER_REFRESH_INTERVAL`] from
///   `proxy_state.roster` (the accept/login pipeline owns that roster;
///   it is authoritative for the live play-state population). The host
///   function reads the registered snapshot, so this interval is the
///   staleness bound a plugin can observe. The swap happens only when
///   the snapshot changed, via remove+register: a registration over an
///   existing key logs a warning in the registry, and a player join/part
///   is not one.
/// - `players.ping_sink` — deliberately **not** registered. The play
///   state is a byte pump with no packet-injection path to a client, so
///   there is no honest way to deliver a chat ping today; leaving the key
///   absent keeps `pingPlayer` answering with its documented
///   "player messaging is not implemented yet" error instead of a sink
///   that pretends to deliver. Wire one in here once the proxy grows a
///   client-bound chat path.
/// - `servers.names` — the backend names the registry seeded into the
///   route store, in registration order. With no `[servers]` configured the
///   key stays absent and the host function falls back to `["local"]`; the
///   reload path re-registers (or removes) the names next to the router
///   swap.
// ipc-migration: Proxy.Stats / Router.Names / Roster.Snapshot — all three keys exist only for the host-function path and become capability calls in the IPC phase
fn register_plugin_host_services(
    registry: &ServiceRegistry,
    proxy_state: &Arc<ProxyState>,
    router_names: Option<&[String]>,
    dispatcher: &Arc<ProxyEventDispatcher>,
) {
    // ipc-migration: Plugins.CallApi — the registry fans plugin callApi out to this dispatcher instance
    registry.register_service(
        DISPATCHER_SERVICE_KEY.to_owned(),
        Arc::clone(dispatcher) as Arc<dyn std::any::Any + Send + Sync>,
    );

    // ipc-migration: Roster.Snapshot — read straight off the shared proxy state
    let roster = roster_snapshot(proxy_state);
    registry.register_service(
        PLAYERS_ROSTER_KEY.to_owned(),
        Arc::new(roster) as Arc<dyn std::any::Any + Send + Sync>,
    );

    if let Some(names) = router_names {
        // ipc-migration: Router.Names — the names the route unit serves, collected once from the registry at boot
        registry.register_service(
            SERVERS_NAMES_KEY.to_owned(),
            Arc::new(names.to_vec()) as Arc<dyn std::any::Any + Send + Sync>,
        );
    }
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
/// Used by `/candyriya reload` to drop a stale cached icon. Returns
/// `true` when the cache now holds the configured icon; `false` keeps
/// whatever the cache held before (or the embedded fallback).
fn refresh_favicon(config: &Config, favicon_cache: Option<&FaviconCache>) -> bool {
    const MAX_FAVICON_BYTES: u64 = 1024 * 1024;
    let Some(cache) = favicon_cache else {
        return false;
    };
    let path = Path::new(&config.motd_favicon);
    match std::fs::read(path) {
        Ok(bytes) => {
            let magic_ok = bytes.len() >= 8 && bytes.starts_with(b"\x89PNG\r\n\x1a\n");
            if magic_ok && (bytes.len() as u64) <= MAX_FAVICON_BYTES {
                cache.put(&bytes);
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
/// [`ConfigStore::reload`] contract); the routing registry, the
/// `servers.names` host service and the favicon are only swapped when
/// their own rebuild succeeds, so a half-applied reload cannot leak out.
/// Returns an operator-facing note on success.
// ipc-migration: Config.Reload / Route.Replace / I18n.Reload — the phase swaps shared state straight across subsystem boundaries
fn reload_config_phase(
    store: &ConfigStore,
    config_path: &Path,
    proxy_state: &ProxyState,
    favicon_cache: Option<&FaviconCache>,
    locales: &candyriya_command::Locales,
    services: &ServiceRegistry,
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
        // mirror a fresh start with no [servers]: the key goes away and
        // listServers keeps its documented ["local"] fallback
        // ipc-migration: Router.Names — the key is host-function-facing; swap and delete both ride the registry
        services.remove_service::<Vec<String>>(SERVERS_NAMES_KEY);
        notes.push("routing disabled (no [servers] configured)".to_owned());
    } else {
        match candyriya_proxy::registry_from_config(&config) {
            Ok(registry) => {
                let names: Vec<String> = registry
                    .iter()
                    .map(|backend| backend.name().to_owned())
                    .collect();
                let count = names.len();
                // ipc-migration: Route.Replace (swap-through-the-shared-state path)
                proxy_state.set_router(Some(Arc::new(registry)));
                // keep the plugin-facing name list on the same swap, so a
                // plugin never sees names that no longer route; remove+register
                // to skip the registry's replace warning
                // ipc-migration: Router.Names — same swap, host-function-facing
                services.remove_service::<Vec<String>>(SERVERS_NAMES_KEY);
                services.register_service(
                    SERVERS_NAMES_KEY.to_owned(),
                    Arc::new(names) as Arc<dyn std::any::Any + Send + Sync>,
                );
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

    if refresh_favicon(&config, favicon_cache) {
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

/// Reload the plugin phase: re-stage every running plugin from disk and
/// register newcomers found under the roots. The running set survives
/// any failure — broken replacements never evict a working instance.
// ipc-migration: Plugins.Reload — the phase reaches the plugin set, the shared registry and the cache across subsystem boundaries
fn reload_plugins_phase(
    store: &ConfigStore,
    data_dir: &Path,
    plugin_set: &PluginSet,
    cache: &CacheManager,
    services: &ServiceRegistry,
    plugin_locales: &SharedLocales,
) -> candyriya_command::PluginPhaseReport {
    // limits come from the current configuration, i.e. the post-config-reload
    // one when both phases run — a raised [plugins] cap applies immediately
    let config = store.get();
    let limits = LimitBuilder::from_config(&config).build();
    let reloader = PluginReloader::with_limits(
        plugin_set.clone(),
        data_dir.to_path_buf(),
        services.clone(),
        limits.clone(),
    )
    .with_locales(plugin_locales.clone());
    let loader = PluginLoader::with_limits(
        data_dir.join("plugins"),
        data_dir.join("libraries"),
        cache,
        limits.clone(),
    )
    .with_locales(plugin_locales.clone());
    let report = reloader.reload_all(&loader);

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
    data_dir: &Path,
    plugin_locales: SharedLocales,
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
                &services,
            )
        });
        let plugins = plugin_phase.then(|| {
            reload_plugins_phase(
                &store,
                &data_dir,
                &plugin_set,
                &cache,
                &services,
                &plugin_locales,
            )
        });

        ReloadReport { config, plugins }
    })
}

/// Cache the configured favicon on first use; fall back to the embedded icon.
fn warm_favicon(config: &Config, favicon_cache: Option<&FaviconCache>) {
    let Some(cache) = favicon_cache else {
        return; // no cache: the status handler reads from disk per request
    };
    if cache.get().is_some() {
        debug!("favicon already present in the cache");
        return;
    }
    let _ = refresh_favicon(config, Some(cache));
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
    config: &Config,
    cache: &CacheManager,
    plugin_locales: SharedLocales,
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
    let limits = LimitBuilder::from_config(config).build();
    let loader = PluginLoader::with_limits(plugins_dir, libraries_dir, cache, limits)
        .with_locales(plugin_locales);
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
}
