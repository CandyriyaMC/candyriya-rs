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
use crate::host_state::HostState;
use crate::instance::PluginInstance;
use crate::library_plugin::{LibraryPlugin, PluginState};
use crate::limits::Limits;
use crate::manifest::PluginManifest;
use crate::plugin_loader::select_wasm;
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
        }
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

        let data_dir = self.config_root.join("config").join(id);
        std::fs::create_dir_all(&data_dir).map_err(|source| PluginError::DataDir {
            path: data_dir.clone(),
            source,
        })?;

        let instance = PluginInstance::new(&wasm_path, HostState::new(data_dir), id, &self.limits)?;
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

        // the replacement is ready; the swap drops the old world's
        // services and exchanges the record in place
        let mut replacement = LibraryPlugin::new(dir, manifest, origin, instance);
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
