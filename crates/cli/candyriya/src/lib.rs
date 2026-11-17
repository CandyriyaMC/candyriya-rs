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
//!    `ArcSwap`; the snapshot flows to every subsystem. (Hot reload is wired
//!    for the future `/candyriya reload` command.)
//! 4. **Runtime** — tokio multi-thread pool sized from `[runtime] worker-threads`,
//!    and the central [`TaskService`] with a fixed rayon blocking pool from
//!    `[runtime] blocking-threads`. Every task in the process is registered
//!    with the service by name; there are no ad-hoc spawns.
//! 5. **Subsystems** — the cache manager (favicon uniquification), the
//!    Prometheus exporter (only when `[metrics] prometheus = true`), the
//!    Mojang/offline session validators selected from `online-mode`, the
//!    routing registry, the command tree and the interactive console REPL.
//! 6. **Lifecycle** — Ctrl-C or `/candyriya end` flips the shared shutdown
//!    signal; the accept loop stops, the task service drains, the subsystems
//!    shut down in reverse order of wiring, and the process exits with code 0.
//!
//! Plugins under `plugins/` and `libraries/` are scanned and loaded once at
//! startup. The reload path is deliberately not wired yet; the `/candyriya`
//! command only carries the built-in set for now.

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
use candyriya_plugin::{LimitBuilder, PluginLoader, PluginSet, ServiceRegistry, resolve_plugins};
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
    let store = ConfigStore::from_path(&config_path).map_err(|error| CliError::Config {
        path: config_path.clone(),
        message: error.to_string(),
    })?;
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
    config: Arc<Config>,
    store: ConfigStore,
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
        Arc::new(store) as Arc<dyn std::any::Any + Send + Sync>,
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
    proxy_state.router = router.clone();
    proxy_state.favicon_cache = favicon_cache.clone();
    let proxy_state = Arc::new(proxy_state);

    // ---- command tree (built-in commands only)
    let locales = build_locales(data_dir)?;
    let shutdown = ShutdownHandle::new();
    let directory = Arc::new(CliDirectory::new(
        Arc::clone(&config),
        Arc::clone(&proxy_state),
        config_name.to_string(),
        log_level.to_string(),
        started,
    ));
    let effects = Arc::new(CliEffects::new(use_ansi));
    let command_state = CommandState {
        locales,
        directory: directory as Arc<dyn candyriya_command::NetworkDirectory>,
        effects: effects.clone() as Arc<dyn candyriya_command::ProxyEffects>,
    };
    let command_registry = CommandRegistry::new(command_state, shutdown.clone());
    let dispatcher = Arc::new(command_registry.dispatcher().map_err(|error| {
        CliError::Runtime(format!("failed to assemble the built-in commands: {error}"))
    })?);
    registry.register_service(
        "commands.dispatcher".to_string(),
        dispatcher.clone() as Arc<dyn std::any::Any + Send + Sync>,
    );

    // ---- plugins: startup load only; the reload path lands later
    let plugin_set = load_plugins(data_dir, &config, &cache);
    registry.register_service(
        "plugins.set".to_string(),
        Arc::clone(&plugin_set) as Arc<dyn std::any::Any + Send + Sync>,
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

/// Cache the configured favicon on first use; fall back to the embedded icon.
fn warm_favicon(config: &Config, favicon_cache: Option<&FaviconCache>) {
    const MAX_FAVICON_BYTES: u64 = 1024 * 1024;
    let Some(cache) = favicon_cache else {
        return; // no cache: the status handler reads from disk per request
    };
    if cache.get().is_some() {
        debug!("favicon already present in the cache");
        return;
    }
    let path = Path::new(&config.motd_favicon);
    match std::fs::read(path) {
        Ok(bytes) => {
            let magic_ok = bytes.len() >= 8 && bytes.starts_with(b"\x89PNG\r\n\x1a\n");
            if magic_ok && (bytes.len() as u64) <= MAX_FAVICON_BYTES {
                cache.put(&bytes);
                info!(path = %path.display(), bytes = bytes.len(), "favicon loaded and cached");
            } else {
                warn!(
                    path = %path.display(),
                    bytes = bytes.len(),
                    "favicon is not a PNG within the size cap; the embedded fallback icon will be served"
                );
            }
        }
        Err(error) => {
            debug!(
                path = %path.display(),
                %error,
                "favicon unreadable; the embedded fallback icon will be served"
            );
        }
    }
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
