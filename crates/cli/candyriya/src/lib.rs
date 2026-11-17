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
//!    `ArcSwap`; the snapshot flows to every subsystem. Hot reload is
//!    wired end to end: `/candyriya reload` re-reads the file the proxy
//!    was started with, swaps atomically and refreshes what it safely
//!    can (routes, favicon, locales, plugins).
//! 4. **Runtime** — tokio multi-thread pool sized from `[runtime] worker-threads`,
//!    and the central [`TaskService`] with a fixed rayon blocking pool from
//!    `[runtime] blocking-threads`. Every task in the process is registered
//!    with the service by name; there are no ad-hoc spawns.
//! 5. **Subsystems** — the cache manager (favicon uniquification), the
//!    Prometheus exporter (only when `[metrics] prometheus = true`), the
//!    Mojang/offline session validators selected from `online-mode`, the
//!    routing registry, the command tree and the interactive console REPL.
//!    The plugin-facing host services the `candyriya:*` functions resolve
//!    through — the plugin dispatcher, the play-state roster snapshot and
//!    the backend name list — are registered right after the plugin load.
//! 6. **Lifecycle** — Ctrl-C or `/candyriya end` flips the shared shutdown
//!    signal; the accept loop stops, the task service drains, the subsystems
//!    shut down in reverse order of wiring, and the process exits with code 0.
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
use candyriya_plugin::{
    DISPATCHER_SERVICE_KEY, LimitBuilder, PLAYERS_ROSTER_KEY, PluginLoader, PluginReloader,
    PluginSet, ProxyEventDispatcher, SERVERS_NAMES_KEY, ServiceRegistry, resolve_plugins,
};
use candyriya_plugin_api::PlayerRef;
use candyriya_proxy::{FaviconCache, ProxySocket, ProxyState, SocketConfig, serve};
use candyriya_route::BackendRegistry;
use candyriya_scheduler::TaskService;
use candyriya_telemetry::LogConfig;

mod ansi;
mod directory;
mod effects;
mod repl;

use crate::directory::CliDirectory;
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
/// Failures are logged (and printed on stderr by `main`), exit code 1.
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

    let registry = ServiceRegistry::new();
    registry.register_service(
        "scheduler.service".to_string(),
        Arc::new(service.clone()) as Arc<dyn std::any::Any + Send + Sync>,
    );

    // ---- cache (favicon uniquification goes through here)
    let cache = CacheManager::new(data_dir).map_err(|error| CliError::Cache(error.to_string()))?;
    let cache = Arc::new(cache);
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
        Some(resolve_socket_addr(&config.metrics.prometheus_address)?)
    } else {
        None
    };
    let metrics_handle = candyriya_metrics::install(metrics_addr)
        .map_err(|error| CliError::Metrics(error.to_string()))?;

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
    registry.register_service(
        "auth.offline".to_string(),
        offline_validator as Arc<dyn std::any::Any + Send + Sync>,
    );
    registry.register_service(
        "auth.mojang".to_string(),
        mojang_validator as Arc<dyn std::any::Any + Send + Sync>,
    );
    registry.register_service(
        "config.store".to_string(),
        Arc::clone(&store) as Arc<dyn std::any::Any + Send + Sync>,
    );
    if let Some(router) = &router {
        registry.register_service(
            "route.backends".to_string(),
            Arc::clone(router) as Arc<dyn std::any::Any + Send + Sync>,
        );
    }

    // ---- shared proxy state
    let started = Instant::now();
    let mut proxy_state = ProxyState::new(Arc::clone(&config));
    proxy_state.auth = auth;
    proxy_state.set_router(router.clone());
    proxy_state.favicon_cache = favicon_cache.clone();
    let proxy_state = Arc::new(proxy_state);

    // ---- command tree: the registry itself is built after the plugin set
    // exists, so `/candyriya reload` can run both phases
    let locales = build_locales(data_dir)?;
    let shutdown = ShutdownHandle::new();
    let directory = Arc::new(CliDirectory::new(
        Arc::clone(&store),
        Arc::clone(&proxy_state),
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

    // ---- plugins: startup load
    let plugin_set = load_plugins(data_dir, &config, &cache);
    registry.register_service(
        "plugins.set".to_string(),
        Arc::clone(&plugin_set) as Arc<dyn std::any::Any + Send + Sync>,
    );
    // ---- plugin host services: without these, the candyriya:* host
    // functions fall back to their built-in answers (callApi errors out,
    // the roster reads empty, listServers answers ["local"])
    register_plugin_host_services(
        &registry,
        &service,
        &plugin_set,
        &proxy_state,
        router.as_deref(),
    );

    // ---- hot reload: the same machinery, on demand
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
    );
    let command_registry =
        CommandRegistry::new(command_state, shutdown.clone()).with_reload(reload_fn);
    let dispatcher = Arc::new(command_registry.dispatcher().map_err(|error| {
        CliError::Runtime(format!("failed to assemble the built-in commands: {error}"))
    })?);
    registry.register_service(
        "commands.dispatcher".to_string(),
        dispatcher.clone() as Arc<dyn std::any::Any + Send + Sync>,
    );

    // ---- one shutdown signal for Ctrl-C and /candyriya end; the accept
    // loop watches one receiver, the main loop watches another
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut console_rx = shutdown_tx.subscribe();

    let bind_addr = resolve_socket_addr(&config.bind)?;
    let socket = ProxySocket::bind(bind_addr, &SocketConfig::from(&config.advanced))
        .map_err(|error| CliError::Proxy(error.to_string()))?;
    let local_addr = socket.local_addr();
    let listener = socket.into_listener();
    let accept_task = serve(Arc::clone(&proxy_state), listener, shutdown_rx, &service);
    info!(bind = %local_addr, task = ?accept_task, "proxy listening");

    // ---- periodic stats line
    let stats_state = Arc::clone(&proxy_state);
    let stats_router = router.clone();
    service.every("stats.every-60s", Duration::from_secs(60), move || {
        let state = Arc::clone(&stats_state);
        let router = stats_router.clone();
        async move {
            log_stats(&state, router.as_deref());
        }
    });

    // ---- interactive console REPL (only when stdin is a TTY)
    let _repl_task = repl::spawn_if_interactive(
        &service,
        Arc::clone(&dispatcher),
        effects.clone(),
        shutdown,
        shutdown_tx.clone(),
        use_ansi,
    );

    // ---- wait for shutdown: Ctrl-C, or the console REPL flipping the flag
    tokio::select! {
        result = tokio::signal::ctrl_c() => {
            match result {
                Ok(()) => info!("Ctrl-C received; shutting down"),
                Err(error) => warn!(%error, "failed to listen for Ctrl-C; continuing until told otherwise"),
            }
        }
        result = console_rx.changed() => {
            if result.is_ok() {
                info!("shutdown requested via the console; shutting down");
            }
        }
    }
    let _ = shutdown_tx.send(true);

    // ---- drain the task service before tearing anything down
    let report = service.drain(Duration::from_secs(10)).await;
    info!(?report, "task service drained");

    // ---- reverse-order teardown of the subsystems (drain handled the tasks)
    match cache.shutdown() {
        Ok(report) => debug!(?report, "cache shut down"),
        Err(error) => warn!(%error, "cache shutdown failed"),
    }
    metrics_handle.shutdown();

    Ok(())
}

/// How often the `players.roster` snapshot is rebuilt from the live
/// play-state roster. Plugin-facing `getPlayerRoster` answers from that
/// snapshot, so a join or a disconnect may lag by at most this interval
/// (plus task-scheduling jitter).
const ROSTER_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

/// Snapshot the live play-state roster into the `Vec<PlayerRef>` shape the
/// `players.roster` service key carries.
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
/// - `plugin.dispatcher` — built from the very [`PluginSet`] hot reload
///   mutates in place (a cheap `Arc`-shared clone), so the dispatcher
///   always fans out into the current plugin records with no refresh of
///   its own. Reusing the same `plugin.dispatcher` key another component
///   may register under keeps the two registrations interchangeable; the
///   registry just replaces on re-register.
/// - `players.roster` — an initial snapshot is registered here, then a
///   named task rebuilds it every [`ROSTER_REFRESH_INTERVAL`] from
///   `proxy_state.roster` (the accept/login pipeline owns that roster;
///   it is authoritative for the live play-state population). The host
///   function reads the registered snapshot, so this interval is the
///   staleness bound a plugin can observe. The swap happens only when the
///   snapshot changed, via remove+register: a registration over an
///   existing key logs a warning in the registry, and a player join/part
///   is not one.
/// - `players.ping_sink` — deliberately **not** registered. The play
///   state is a byte pump with no packet-injection path to a client, so
///   there is no honest way to deliver a chat ping today; leaving the key
///   absent keeps `pingPlayer` answering with its documented
///   "player messaging is not implemented yet" error instead of a sink
///   that pretends to deliver. Wire one in here once the proxy grows a
///   client-bound chat path.
/// - `servers.names` — the backend names from [`BackendRegistry`], in
///   registration order. With no `[servers]` configured the key stays
///   absent and the host function falls back to `["local"]`; the reload
///   path re-registers (or removes) the names next to the router swap.
fn register_plugin_host_services(
    registry: &ServiceRegistry,
    service: &TaskService,
    plugin_set: &Arc<PluginSet>,
    proxy_state: &Arc<ProxyState>,
    router: Option<&BackendRegistry>,
) {
    let dispatcher = Arc::new(ProxyEventDispatcher::new(plugin_set.as_ref().clone()));
    registry.register_service(
        DISPATCHER_SERVICE_KEY.to_owned(),
        dispatcher as Arc<dyn std::any::Any + Send + Sync>,
    );

    let roster = roster_snapshot(proxy_state);
    registry.register_service(
        PLAYERS_ROSTER_KEY.to_owned(),
        Arc::new(roster.clone()) as Arc<dyn std::any::Any + Send + Sync>,
    );
    let mut last_registered = roster;
    let roster_state = Arc::clone(proxy_state);
    let roster_registry = registry.clone();
    service.every(
        "players.roster-refresh",
        ROSTER_REFRESH_INTERVAL,
        move || {
            // the comparison is cheap (a few small strings), so it happens
            // synchronously in the factory and the returned future is empty
            let current = roster_snapshot(&roster_state);
            if current != last_registered {
                // swap without the registry's replace warning: a join/part
                // is normal traffic, not an operator-actionable event
                roster_registry.remove_service::<Vec<PlayerRef>>(PLAYERS_ROSTER_KEY);
                roster_registry.register_service(
                    PLAYERS_ROSTER_KEY.to_owned(),
                    Arc::new(current.clone()) as Arc<dyn std::any::Any + Send + Sync>,
                );
                last_registered = current;
            }
            async {}
        },
    );

    if let Some(router) = router {
        let names: Vec<String> = router
            .iter()
            .map(|backend| backend.name().to_owned())
            .collect();
        registry.register_service(
            SERVERS_NAMES_KEY.to_owned(),
            Arc::new(names) as Arc<dyn std::any::Any + Send + Sync>,
        );
    }
}

/// Log one compact stats line for the periodic task.
fn log_stats(state: &ProxyState, router: Option<&BackendRegistry>) {
    let (backends_online, backends_total) = match router {
        Some(router) => (
            router
                .iter()
                .filter(|backend| backend.state() == candyriya_route::BackendState::Online)
                .count(),
            router.iter().count(),
        ),
        None => (0, 0),
    };
    info!(
        active_connections = state.limiter.current_connections(),
        accepted = state.conn_counter.load(Ordering::Relaxed),
        rejected = state.connection_rejections.load(Ordering::Relaxed),
        backends_online,
        backends_total,
        "proxy stats"
    );
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
        proxy_state.set_router(None);
        // mirror a fresh start with no [servers]: the key goes away and
        // listServers keeps its documented ["local"] fallback
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
                proxy_state.set_router(Some(Arc::new(registry)));
                // keep the plugin-facing name list on the same swap, so a
                // plugin never sees names that no longer route; remove+register
                // to skip the registry's replace warning
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
fn reload_plugins_phase(
    store: &ConfigStore,
    data_dir: &Path,
    plugin_set: &PluginSet,
    cache: &CacheManager,
    services: &ServiceRegistry,
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
    );
    let loader = PluginLoader::with_limits(
        data_dir.join("plugins"),
        data_dir.join("libraries"),
        cache,
        limits.clone(),
    );
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
        let plugins = plugin_phase
            .then(|| reload_plugins_phase(&store, &data_dir, &plugin_set, &cache, &services));

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

/// Build the command locales.
///
/// `<data_dir>/locales/{en,ex_girl}/*.toml` overrides the embedded tables;
/// a broken override degrades to embedded with a warning instead of blocking
/// startup, and an embedded failure is a build defect and fails hard.
fn build_locales(data_dir: &Path) -> Result<candyriya_command::Locales, CliError> {
    let locales_dir = data_dir.join("locales");
    if locales_dir.is_dir() {
        match (
            I18n::with_override_dir("en", &locales_dir),
            I18n::with_override_dir("ex_girl", &locales_dir),
        ) {
            (Ok(en), Ok(ex_girl)) => {
                return Ok(candyriya_command::Locales::new(en, ex_girl));
            }
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
    candyriya_command::Locales::embedded()
        .map_err(|error| CliError::Runtime(format!("embedded locales are broken: {error}")))
}

/// Scan `plugins/` and `libraries/`, resolve the dependency order, and build
/// the shared plugin set. Failures are logged, never fatal: a broken plugin
/// must not take the proxy down.
fn load_plugins(data_dir: &Path, config: &Config, cache: &CacheManager) -> Arc<PluginSet> {
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
    let loader = PluginLoader::with_limits(plugins_dir, libraries_dir, cache, limits);
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
