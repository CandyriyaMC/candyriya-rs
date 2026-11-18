//! Hot reload of the plugin tree.
//!
//! Two cooperating pieces:
//!
//! - [`Watch`] wraps a `notify` filesystem watcher on `plugins/`. Any
//!   change is turned into one `plugin-reload-<id>` blocking task on the
//!   scheduler; the task calls [`PluginReloader::reload`].
//! - [`PluginReloader`] performs the swap itself: verify the files on
//!   disk stage cleanly into a fresh instance, and only then replace the
//!   running one.
//!
//! # Failure policy
//!
//! A reload that cannot be staged (the wasm file is unreadable, the
//! manifest no longer parses or moved to a different id, the new module
//! does not export `plugin_main`) fails with an error and **leaves the
//! running plugin untouched**. Only once the replacement is fully ready —
//! compiled, `plugin_main` verified — is the old record swapped out, and
//! the swap itself is infallible.
//!
//! The old plugin is never joined during a reload: its mailbox is closed
//! and its record dropped. A healthy worker sees the closed channel and
//! exits on its own; a plugin stuck inside a wasm call keeps its thread
//! until the call returns, but never blocks the reloader or anyone else.
//! A `Failed` plugin (no instance) can be resurrected the same way: fix
//! the files, reload, it comes up.
//!
//! # What survives a reload
//!
//! - Load order — the record is swapped in place.
//! - The plugin's `Loaded`/`Disabled` switch — a disabled plugin stays
//!   disabled across reloads.
//! - Core services in the [`ServiceRegistry`]. Plugin-owned services
//!   ([`ServiceRegistry::register_owned_service`]) are revoked in bulk:
//!   the owning plugin is gone, its handles must go with it.
//!   Re-registering is the new instance's job (through host functions,
//!   which land with the host-function milestone); a freshly reloaded
//!   plugin must not inherit the dead one's service entries.
//! - The plugin's data directory and cache namespace/quotas — both live
//!   outside this crate's swap and are untouched.
//!
//! Deleting a plugin's files does **not** unload the running plugin —
//! once loaded, a plugin stays loaded until it is unloaded explicitly or
//! the proxy shuts down. Watch mode only reloads.
//!
//! # What a reload means for native plugins
//!
//! `/candyriya reload plugins` flows through two phases (see
//! [`PluginReloader::reload_all`]): phase A swaps every record already in
//! the set for a freshly staged replacement; phase B re-scans the roots
//! and registers plugins that were not running before.
//!
//! Phase A is wasm-specific by construction — it stages a wasm module
//! and swaps the record in place. A native record has no wasm module to
//! stage, so `reload()` on a native id fails with `MissingWasm`, lands in
//! the report's `failed` list and leaves the running native instance
//! untouched. A native stage-then-swap (restage the new library while
//! the old one keeps running, swap under quiescence) is a separate
//! phase-2 slice and is **not** attempted here — an operator who
//! restages a native library hears a loud `MissingWasm` failure, never a
//! half-swapped set.
//!
//! Phase B re-scans the roots through [`PluginLoader::load`], so a native
//! candidate found on disk runs the exact same bring-up the loader runs
//! at boot (data dirs, cache quotas, the controller-registry walk
//! `DISCOVERED` → … → `RUNNING`, or a `Failed` record naming the native
//! step that refused). A native plugin that already holds a record in
//! the shared process-wide controller registry is refused at insert time
//! (duplicate id) when phase B rediscovers it; the refusal lands in the
//! reload issues, the rebuilt duplicate is dropped before registration,
//! and the running record is untouched — it never reaches `loaded` or
//! `new_failed`. Either way, a native plugin whose bring-up fails on
//! reload keeps the same visible-not-silent semantics as a wasm one:
//! a `Failed` record with the reason in the issues list, never silent.
//!
//! A native plugin that was `Failed` at boot is in the set (that is how
//! startup surfaces broken plugins), so it also goes through phase A,
//! fails there with `MissingWasm` and keeps its stale `Failed` reason
//! until the native stage-then-swap slice lands. Its failure stays
//! visible through the `Failed` state and the issues list.
//!
//! The staged replacement gets a fresh, private instance registry: the
//! outgoing and staged instance are briefly both alive, and `max-instances`
//! (default 1) must not reject a legitimate stage. The running instance's
//! own registry is unaffected either way.

use std::collections::{BTreeSet, HashMap};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use notify::Watcher;
use parking_lot::Mutex;

use candyriya_scheduler::TaskService;

use crate::error::PluginError;
use crate::health::PluginHealthSet;
use crate::host_ipc::HostIpc;
use crate::host_state::HostState;
use crate::instance::{PluginFormat, PluginInstance};
use crate::library_plugin::{LibraryPlugin, PluginState};
use crate::limits::Limits;
use crate::locales::SharedLocales;
use crate::manifest::PluginManifest;
use crate::plugin_loader::{LoadIssue, PluginLoader, select_wasm};
use crate::plugin_set::PluginSet;
use crate::service_registry::ServiceRegistry;

/// Files in the same plugin directory are usually written in bursts
/// (temp file + rename); reloading more than once per window is waste.
const DEBOUNCE_WINDOW: Duration = Duration::from_millis(250);

/// How long dead debounce entries are kept before the map is compacted.
const DEBOUNCE_RETENTION: Duration = Duration::from_secs(10);

/// Upper bound of debounce entries before pruning kicks in.
const DEBOUNCE_MAX_ENTRIES: usize = 256;

/// What a successful [`PluginReloader::reload`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReloadReport {
    /// The id that was reloaded.
    pub plugin: String,
    /// The wasm module the new instance was built from.
    pub wasm_path: PathBuf,
    /// The state of the new record (`Loaded`, or `Disabled` when the
    /// plugin was disabled before the reload).
    pub state: PluginState,
    /// How many plugin-owned services were revoked from the registry.
    pub revoked_services: usize,
}

/// What a [`PluginReloader::reload_all`] pass did, across both the
/// existing set and whatever the directories had to add.
///
/// Phase A reloads every running plugin from disk in place; phase B
/// re-scans the roots and registers plugins that were not running before.
/// Failures never tear down the running set: a plugin that cannot stage
/// keeps its current instance (or stays absent, for newcomers).
#[derive(Debug)]
pub struct ReloadAllReport {
    /// Existing plugins that were reloaded successfully, in load order.
    pub reloaded: Vec<ReloadReport>,
    /// Plugins that were discovered on disk and registered by this
    /// reload pass, in (dependency-resolved) registration order.
    pub loaded: Vec<String>,
    /// Existing plugins whose reload failed. The running instance (if
    /// any) is untouched either way.
    pub failed: Vec<(String, PluginError)>,
    /// Newly discovered plugins that failed to make it into the set —
    /// stage errors land in the discovery issues, anything else here
    /// gets a one-line reason. The running set is unaffected.
    pub new_failed: Vec<(String, String)>,
    /// Running plugins whose directory vanished (or is not a directory)
    /// since startup. They are kept running on purpose — deleting files
    /// is not an unload — and listed here so callers can warn.
    pub missing: Vec<String>,
    /// Everything discovery or dependency resolution had to complain
    /// about while looking for *new* plugins. Informational only.
    pub issues: Vec<LoadIssue>,
}

/// Errors produced while setting up the filesystem watcher.
#[derive(Debug, thiserror::Error)]
pub enum HotReloadError {
    /// The plugins directory could not be resolved or is not a directory.
    #[error("failed to resolve plugins directory {}: {source}", .path.display())]
    Resolve {
        /// The directory that was asked for.
        path: PathBuf,
        /// What went wrong.
        source: std::io::Error,
    },

    /// The `notify` watcher could not be created or armed.
    #[error("failed to watch plugins directory {}: {source}", .path.display())]
    Watch {
        /// The directory that was watched.
        path: PathBuf,
        /// What went wrong.
        source: notify::Error,
    },
}

/// Unloads, reloads and re-registers a single plugin.
///
/// Clone freely; every clone refers to the same shared plugin set. Built
/// once at composition time alongside the [`PluginSet`].
#[derive(Clone)]
pub struct PluginReloader {
    plugins: PluginSet,
    config_root: PathBuf,
    services: ServiceRegistry,
    limits: Limits,
    locales: Option<SharedLocales>,
    host_ipc: HostIpc,
    health: Option<PluginHealthSet>,
}

impl PluginReloader {
    /// Create a reloader over the shared plugin set.
    ///
    /// `config_root` is the proxy's data directory; plugin data
    /// directories live at `<config_root>/config/<id>/`. It must match
    /// what the loader used so the reloaded plugin finds its own files.
    /// Sandbox limits default to [`Limits::default`]; use
    /// [`PluginReloader::with_limits`] to apply the `[plugins]` section of
    /// `candyriya-config` to reloaded plugins too.
    pub fn new(plugins: PluginSet, config_root: PathBuf, services: ServiceRegistry) -> Self {
        Self::with_limits(plugins, config_root, services, Limits::default())
    }

    /// Same as [`PluginReloader::new`] with explicit sandbox limits for the
    /// staged replacement (see `LimitBuilder::from_config`).
    pub fn with_limits(
        plugins: PluginSet,
        config_root: PathBuf,
        services: ServiceRegistry,
        limits: Limits,
    ) -> Self {
        Self {
            plugins,
            config_root,
            services,
            limits,
            locales: None,
            host_ipc: HostIpc::new(),
            health: None,
        }
    }

    /// The shared i18n machines reloaded plugins resolve through and
    /// re-register their `locales/` directory into.
    ///
    /// Same clones the loader and the command layer use. Without this,
    /// `[locales]` in a reloaded plugin.toml has no effect on resolution
    /// (the file contents of an already-registered plugin are picked up
    /// by `I18n::reload` regardless).
    #[must_use]
    pub fn with_locales(mut self, locales: SharedLocales) -> Self {
        self.locales = Some(locales);
        self
    }

    /// The shared link to the supervised `host` unit staged replacements
    /// resolve through. Same slot the loader used; without it a reloaded
    /// plugin's host functions answer with the honest "host unit link is
    /// not installed" error.
    #[must_use]
    pub fn with_host_ipc(mut self, host_ipc: HostIpc) -> Self {
        self.host_ipc = host_ipc;
        self
    }

    /// The shared per-plugin health registry staged replacements flag
    /// into. Same slot the loader used; every fresh instance this reloader
    /// builds clears its plugin's conviction. Without it a restaged plugin
    /// is invisible to the supervision units until its first call, and a
    /// supervision-driven restage leaves the old conviction in place.
    #[must_use]
    pub fn with_plugin_health(mut self, health: PluginHealthSet) -> Self {
        self.health = Some(health);
        self
    }

    /// Reload plugin `id` from the files currently on disk.
    ///
    /// Stages the replacement (manifest, wasm compile, `plugin_main`)
    /// while the running plugin keeps serving; only then is the old
    /// record swapped out — load order and the `Loaded`/`Disabled`
    /// switch are preserved. Plugin-owned services in the registry are
    /// revoked as part of the swap.
    ///
    /// # Errors
    ///
    /// - [`PluginError::InvalidState`] — no record for `id`, or a
    ///   concurrent reload of the same plugin won the swap race; the
    ///   running plugin is untouched either way.
    /// - [`PluginError::Io`] / [`PluginError::Manifest`] — the manifest
    ///   is unreadable or invalid; the running plugin is untouched.
    /// - [`PluginError::MissingWasm`] — no `<id>.wasm`/`lib.wasm` found;
    ///   the running plugin is untouched.
    /// - [`PluginError::WasmLoad`] / [`PluginError::MissingExport`] —
    ///   the new module does not stage; the running plugin is untouched.
    pub fn reload(&self, id: &str) -> Result<ReloadReport, PluginError> {
        // stage everything before touching the running record; a failure
        // anywhere up to (but excluding) the swap leaves the old plugin
        // serving
        let (dir, origin, old_state) = {
            let guard = self.plugins.read();
            let record = guard.get(id).ok_or_else(|| PluginError::InvalidState {
                plugin_id: id.to_owned(),
                action: "reload",
                state: "not registered".to_owned(),
            })?;
            (
                record.dir().to_path_buf(),
                record.origin(),
                record.state().clone(),
            )
        };

        let manifest = read_manifest(&dir, id)?;
        let wasm_path = select_wasm(&dir, id).map_err(|_| PluginError::MissingWasm {
            plugin_id: id.to_owned(),
            dir: dir.clone(),
        })?;

        // the replacement inherits the shared plugin-owned services and
        // the host unit link (the dispatcher, the roster, ...) so its
        // host functions work the same as the startup-loaded instance did
        let data_root = self.config_root.clone();
        let config_dir = data_root.join("config").join(id);
        std::fs::create_dir_all(&config_dir).map_err(|source| PluginError::DataDir {
            path: config_dir.clone(),
            source,
        })?;
        let cache_dir = data_root.join("cache").join(id);
        std::fs::create_dir_all(&cache_dir).map_err(|source| PluginError::DataDir {
            path: cache_dir.clone(),
            source,
        })?;

        let mut host_state = HostState::new(data_root, id)
            .with_services(Arc::new(self.services.clone()))
            .with_host_ipc(self.host_ipc.clone());
        if let Some(locales) = &self.locales {
            host_state = host_state.with_locales(locales.clone());
        }
        if let Some(health) = &self.health {
            host_state = host_state.with_plugin_health(health.clone());
        }
        let instance = PluginInstance::new(&wasm_path, host_state, id, &self.limits)?;
        match instance.function_exists(manifest.plugin_main()) {
            Ok(true) => {}
            Ok(false) => {
                return Err(PluginError::MissingExport {
                    plugin_id: id.to_owned(),
                    name: manifest.plugin_main().to_owned(),
                });
            }
            Err(error) => return Err(error),
        }

        // the replacement staged cleanly: refresh the plugin's locale
        // registration so edits (or a newly declared [locales] section)
        // are visible right after the swap. never fatal — the swap
        // proceeds either way, the plugin just falls back to common keys
        if manifest.has_locales()
            && let Some(locales) = &self.locales
        {
            let locales_dir = dir.join("locales");
            if let Err(error) = locales.register_plugin_locale(id, &locales_dir) {
                tracing::warn!(
                    plugin = %id,
                    path = %locales_dir.display(),
                    %error,
                    "plugin locale files not registered; plugin keys fall back to common",
                );
            }
        }

        // the replacement is ready; the swap drops the old world's
        // services and exchanges the record in place
        let mut replacement =
            LibraryPlugin::new(dir, manifest, origin, instance, PluginFormat::Wasm);
        if old_state == PluginState::Disabled {
            replacement.disable()?;
        }

        let (old, revoked_services) = {
            let mut guard = self.plugins.write();
            let Some((index, old_key, old_record)) = guard.shift_remove_full(id) else {
                // a concurrent reload won the race between staging and
                // the swap; the staged replacement is dropped here,
                // which never joins and cannot deadlock. nothing was
                // revoked — the winner's own swap handles that.
                return Err(PluginError::InvalidState {
                    plugin_id: id.to_owned(),
                    action: "reload",
                    state: "lost the swap race to a concurrent reload".to_owned(),
                });
            };
            let revoked_services = self.services.remove_services_owned_by(id);
            let insert_at = index.min(guard.len());
            guard.shift_insert(insert_at, old_key, replacement);
            (old_record, revoked_services)
        };

        // closing the mailbox is the whole teardown; a healthy worker
        // sees the closed channel and exits on its own. we do not join —
        // a plugin stuck in wasm must never wedge a reload.
        let mut old = old;
        if let Some(instance) = old.take_instance() {
            drop(instance);
        }

        Ok(ReloadReport {
            plugin: id.to_owned(),
            wasm_path,
            state: if old_state == PluginState::Disabled {
                PluginState::Disabled
            } else {
                PluginState::Loaded
            },
            revoked_services,
        })
    }

    /// Reload every running plugin **and** pick up newcomers.
    ///
    /// Two phases, in order:
    ///
    /// - **Phase A (existing).** Every id currently in the set is
    ///   reloaded exactly like [`PluginReloader::reload`] does for one
    ///   plugin — staged replacement, swap, `Loaded`/`Disabled`
    ///   preserved, plugin-owned services revoked. A plugin whose
    ///   directory vanished from disk is left alone (deleting files is
    ///   not an unload) and reported under [`ReloadAllReport::missing`].
    /// - **Phase B (new).** The roots are re-scanned with `loader`.
    ///   Any discovered id that is not already in the set is run
    ///   through the dependency resolver and registered; survivors are
    ///   reported under [`ReloadAllReport::loaded`] in registration
    ///   order. Discovery/resolution problems land in
    ///   [`ReloadAllReport::issues`], and ids that drop out of the
    ///   resolution land in [`ReloadAllReport::new_failed`].
    ///
    /// A dependency-resolution failure of the whole discovered set means
    /// no new plugin is registered; the running set keeps whatever phase
    /// A produced. New plugins are appended after the existing ones —
    /// a brand-new library never jumps ahead of already-running plugins,
    /// and already-running plugins keep their instances (a plugin that
    /// failed its missing hard dependency at startup is absent and can
    /// only come back as a reloaded record, which phase A handles once
    /// the dependency exists on disk).
    pub fn reload_all(&self, loader: &PluginLoader<'_>) -> ReloadAllReport {
        let mut report = ReloadAllReport {
            reloaded: Vec::new(),
            loaded: Vec::new(),
            failed: Vec::new(),
            new_failed: Vec::new(),
            missing: Vec::new(),
            issues: Vec::new(),
        };

        // phase A: swap every running record in place, in load order
        let running: Vec<String> = self.plugins.ids();
        for id in &running {
            let dir_gone = {
                let guard = self.plugins.read();
                guard.get(id).is_some_and(|record| !record.dir().is_dir())
            };
            if dir_gone {
                // warn-worthy, never fatal: the running instance keeps
                // serving; an operator removed the files on purpose or
                // by accident, either way the proxy is not the disk
                report.missing.push(id.clone());
                continue;
            }
            match self.reload(id) {
                Ok(one) => report.reloaded.push(one),
                Err(error) => report.failed.push((id.clone(), error)),
            }
        }

        // phase B: discover and register newcomers
        let discovered = loader.load();
        report.issues.extend(discovered.issues);
        let running_set: std::collections::HashSet<&str> =
            running.iter().map(String::as_str).collect();
        let discovered_ids: Vec<String> = discovered
            .plugins
            .iter()
            .map(|plugin| plugin.id().to_owned())
            .collect();

        match crate::dep_resolution::resolve_plugins(discovered.plugins) {
            Ok(resolution) => {
                report.issues.extend(resolution.report.issues);
                let mut surviving: Vec<String> = Vec::new();
                for plugin in resolution.report.plugins {
                    let id = plugin.id().to_owned();
                    if running_set.contains(id.as_str()) {
                        // rebuilt during discovery, but phase A already
                        // swapped the running record in — drop the
                        // duplicate instance
                        continue;
                    }
                    surviving.push(id.clone());
                    // a newcomer recorded as Failed still gets inserted —
                    // that is how startup surfaces broken plugins too —
                    // but it is announced as a failure, never as loaded
                    let staged_ok = plugin.state().is_loaded();
                    if self.plugins.insert(plugin) {
                        if staged_ok {
                            report.loaded.push(id);
                        } else {
                            report
                                .new_failed
                                .push((id, "failed to load; see the discovery issues".to_owned()));
                        }
                    } else {
                        report
                            .new_failed
                            .push((id, "already registered during this reload".to_owned()));
                    }
                }
                for id in discovered_ids
                    .iter()
                    .filter(|id| !running_set.contains(id.as_str()))
                {
                    if !surviving.contains(id) {
                        report.new_failed.push((
                            id.clone(),
                            "dropped during dependency resolution; see the issues list".to_owned(),
                        ));
                    }
                }
            }
            Err(error) => {
                // the discovered set itself cannot be ordered (cycle or
                // duplicates); no new plugin is registered, full stop.
                // running plugins already went through phase A.
                report.new_failed.push((
                    "<resolution>".to_owned(),
                    format!("{error}; no new plugins were registered"),
                ));
            }
        }

        report
    }
}

/// Re-read and validate the on-disk manifest for `id` in `dir`.
fn read_manifest(dir: &Path, id: &str) -> Result<PluginManifest, PluginError> {
    let manifest_path = dir.join("plugin.toml");
    let content = std::fs::read_to_string(&manifest_path).map_err(|source| PluginError::Io {
        path: manifest_path.clone(),
        source,
    })?;
    let manifest = PluginManifest::from_toml(&content)?;
    if manifest.id() != id {
        return Err(PluginError::InvalidState {
            plugin_id: id.to_owned(),
            action: "reload",
            state: format!("manifest now declares id `{}`", manifest.id()),
        });
    }
    if let Some(dir_name) = dir.file_name().and_then(|n| n.to_str())
        && manifest.id() != dir_name
    {
        return Err(PluginError::InvalidState {
            plugin_id: id.to_owned(),
            action: "reload",
            state: format!(
                "manifest id `{}` does not match directory `{dir_name}`",
                manifest.id()
            ),
        });
    }
    Ok(manifest)
}

/// A recursive `notify` watcher on `plugins/` that triggers a
/// `plugin-reload-<id>` task on the scheduler for every plugin the
/// change touches.
///
/// Watching is best effort: events that cannot be attributed to a plugin
/// id are ignored, duplicate bursts within the debounce window (250 ms)
/// are dropped, and a plugin whose files were *deleted* is not unloaded —
/// reload only ever swaps an existing plugin for a new one. Drop the
/// value to stop watching.
pub struct Watch {
    // kept alive so the watcher keeps firing; dropping it stops the watch
    _watcher: notify::RecommendedWatcher,
    plugins_dir: PathBuf,
}

impl Watch {
    /// Arm a watcher on `plugins_dir`.
    ///
    /// `plugins_dir` must exist. On any create/modify/remove below it,
    /// one reload task per affected plugin id is registered on
    /// `task_service`; each task calls [`PluginReloader::reload`] and
    /// logs the outcome (failures are logged, never fatal — the running
    /// plugin stays untouched when a reload cannot be staged).
    ///
    /// Note that reloads registered after [`TaskService::drain`] are
    /// silently refused by the scheduler; watching is meaningful during
    /// normal operation only.
    ///
    /// # Errors
    ///
    /// [`HotReloadError::Resolve`] when the directory cannot be
    /// canonicalized, [`HotReloadError::Watch`] when the OS watcher
    /// cannot be created or armed.
    pub fn new(
        plugins_dir: PathBuf,
        task_service: Arc<TaskService>,
        reloader: PluginReloader,
    ) -> Result<Self, HotReloadError> {
        let root =
            std::fs::canonicalize(&plugins_dir).map_err(|source| HotReloadError::Resolve {
                path: plugins_dir.clone(),
                source,
            })?;
        if !root.is_dir() {
            return Err(HotReloadError::Resolve {
                path: root.clone(),
                source: std::io::Error::new(
                    std::io::ErrorKind::NotADirectory,
                    "plugins root is not a directory",
                ),
            });
        }

        let debounce = Arc::new(Mutex::new(HashMap::new()));
        let handler = {
            let root = root.clone();
            let debounce = Arc::clone(&debounce);
            move |res: notify::Result<notify::Event>| {
                handle_event(&res, &root, &task_service, &reloader, &debounce);
            }
        };

        let mut watcher = notify::RecommendedWatcher::new(handler, notify::Config::default())
            .map_err(|source| HotReloadError::Watch {
                path: root.clone(),
                source,
            })?;
        watcher
            .watch(&root, notify::RecursiveMode::Recursive)
            .map_err(|source| HotReloadError::Watch {
                path: root.clone(),
                source,
            })?;

        Ok(Self {
            _watcher: watcher,
            plugins_dir: root,
        })
    }

    /// The canonicalized plugins root being watched.
    pub fn plugins_dir(&self) -> &Path {
        &self.plugins_dir
    }
}

// the watcher handle is not Debug; the useful part is where we watch
impl std::fmt::Debug for Watch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Watch")
            .field("plugins_dir", &self.plugins_dir)
            .finish_non_exhaustive()
    }
}

fn handle_event(
    res: &notify::Result<notify::Event>,
    root: &Path,
    task_service: &TaskService,
    reloader: &PluginReloader,
    debounce: &Mutex<HashMap<String, Instant>>,
) {
    let event = match res {
        Ok(event) => event,
        Err(error) => {
            tracing::error!(%error, "filesystem watcher reported an error");
            return;
        }
    };
    if !matches!(
        event.kind,
        notify::EventKind::Create(_) | notify::EventKind::Modify(_) | notify::EventKind::Remove(_)
    ) {
        return;
    }

    let mut ids = BTreeSet::new();
    for path in &event.paths {
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let Some(Component::Normal(first)) = relative.components().next() else {
            continue;
        };
        if let Some(id) = first.to_str().filter(|s| is_valid_plugin_id(s)) {
            ids.insert(id.to_owned());
        }
    }

    let now = Instant::now();
    for id in ids {
        {
            let mut map = debounce.lock();
            if map
                .get(&id)
                .is_some_and(|last| now.duration_since(*last) < DEBOUNCE_WINDOW)
            {
                continue;
            }
            map.insert(id.clone(), now);
            if map.len() > DEBOUNCE_MAX_ENTRIES {
                let cutoff = now - DEBOUNCE_RETENTION;
                map.retain(|_, last| *last >= cutoff);
            }
        }

        let reloader = reloader.clone();
        let task_id = task_service.spawn_blocking(format!("plugin-reload-{id}"), {
            let id = id.clone();
            move || match reloader.reload(&id) {
                Ok(report) => tracing::info!(
                    plugin = %id,
                    wasm = %report.wasm_path.display(),
                    state = ?report.state,
                    revoked_services = report.revoked_services,
                    "plugin reloaded",
                ),
                Err(error) => tracing::error!(
                    plugin = %id,
                    %error,
                    "plugin reload failed; the running instance (if any) is untouched",
                ),
            }
        });
        tracing::debug!(plugin = %id, ?task_id, "scheduled plugin reload task");
    }
}

fn is_valid_plugin_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-'))
}
