//! Filesystem discovery for plugins and libraries.
//!
//! [`PluginLoader::new`] takes the two static roots the proxy ships with —
//! `plugins/` and `libraries/` — scans both, and instantiates whatever it
//! finds. Nothing here talks to the network; everything is one directory
//! walk plus one wasm compile per plugin.
//!
//! # Layout
//!
//! ```text
//! plugins/
//!   my-plugin/
//!     plugin.toml      # required
//!     my-plugin.wasm   # or lib.wasm
//!     my-plugin.so     # or .dll/.dylib — the native flavor candidate
//! libraries/
//!   some-library/
//!     plugin.toml
//!     some-library.wasm
//! ```
//!
//! Libraries and plugins share one layout and one loader; they differ only
//! in load order (libraries first, so a plugin can depend on a library that
//! is guaranteed to have been discovered first) and in the `origin` tag on
//! the resulting records. Ids are unique across both directories — an id
//! collision is an error, not a precedence rule.
//!
//! # Native flavor detection
//!
//! Alongside the wasm module, the scanner also stats for a native library
//! directly inside the plugin directory: `<id>.<ext>`, then cargo's cdylib
//! default `lib<id>.<ext>` on unix (no `lib` prefix on Windows). `<ext>` is
//! `dll` on Windows, `dylib` on macOS/iOS, and `so` everywhere else
//! (Linux, Android) — the preferred extension lives in exactly one cfg
//! block (`PREFERRED_NATIVE_EXT`), the fallback list in
//! [`native_library_names`]. Detection is a stat only: the library is never
//! opened, mapped or executed during discovery, no thread is spawned and no
//! handshake is attempted. A candidate that exists but is not a regular file
//! is not a candidate at all.
//!
//! A plugin shipping both a wasm module and a native library gets the
//! *native* flavor (OQ13): a warning naming the plugin id and both paths is
//! logged, the wasm module is left untouched on disk, and the record
//! reports the native flavor. The flavor is additive information on the
//! record; the wasm path ignores it.
//!
//! # Native bring-up
//!
//! Discovery finds native candidates; instantiating them is a different
//! step. When the loader was built with
//! [`PluginLoader::with_plugin_controller`], a native candidate is brought
//! up at load time through [`crate::PluginInstance::native`] — the same
//! code path at boot and at `/candyriya reload plugins` (phase B re-scan):
//! the plugin's config/cache directories and cache quotas are applied
//! first, exactly like the wasm path, then the shared host state (instance
//! slots, services, host IPC, locales, health cells) is built the same way,
//! a [`crate::PluginDescriptor`] native descriptor is stamped from the
//! manifest, and the library is mapped, handshaken, `on_load`d and
//! `on_enable`d on the calling thread. On success the record carries a
//! live instance and reports `Loaded` with [`crate::PluginFormat::Native`];
//! the controller registry walks `DISCOVERED` → … → `RUNNING`. Any refusal
//! — open, handshake, `on_load`, `on_enable` or the registry itself — lands
//! a `Failed` record whose reason names the native step that refused, and
//! the registry walks the record into terminal `UNLOADED` with the reason
//! preserved. Never a silent drop, never a pretend success.
//!
//! Without a controller installed, native-flavor records stay exactly what
//! they were before bring-up existed: a `Failed` record whose reason says
//! native bring-up is not wired into this loader. No directories are
//! created, nothing is mapped, no registry record appears — an honest
//! refusal, never a half-wired pretend success.
//!
//! # Failure policy
//!
//! Discovery never fails the whole load. Everything that is wrong with one
//! plugin — a stray `.wasm` with no manifest, an unreadable or invalid
//! `plugin.toml`, an id that does not match its directory, a duplicate id,
//! a missing module — is collected as a [`LoadIssue`] `(where, why)` tuple.
//! A plugin whose wasm compiles but fails preparation (missing `plugin_main`
//! export, uncreatable data directory, cache quota rejected) is recorded as
//! a [`LibraryPlugin`] in [`PluginState::Failed`] — visible, not silent — and
//! its reason is also listed in `issues`. A native candidate that refuses
//! bring-up (or never gets the chance, because no controller registry is
//! installed) follows the same rule: a `Failed` record whose reason names
//! the library and the step that refused, never a silently dropped
//! directory.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use candyriya_cache::CacheManager;

use crate::health::PluginHealthSet;
use crate::host_ipc::HostIpc;
use crate::host_state::HostState;
use crate::instance::{PluginDescriptor, PluginFormat, PluginInstance};
use crate::library_plugin::{LibraryPlugin, PluginOrigin};
use crate::limits::{InstanceSlots, Limits};
use crate::locales::SharedLocales;
use crate::manifest::{PluginManifest, validate_unique};

/// One discovery problem, as a `(where, why)` tuple.
///
/// The first element is the path that caused the issue (the stray `.wasm`,
/// the bad `plugin.toml`, the duplicate's directory, ...); the second is a
/// human-readable reason.
pub type LoadIssue = (PathBuf, String);

/// The outcome of one `PluginLoader::load` pass.
///
/// `plugins` holds a record per discovered plugin in load order —
/// libraries first, then plugins, alphabetical within each root. `issues`
/// holds every problem encountered while loading; a plugin that ended up in
/// `plugins` as `Failed` carries the same reason in its state — the list
/// exists so operators can scan one flat collection without inspecting
/// records.
pub struct LoadReport {
    /// Discovered plugins, in load order (libraries first).
    pub plugins: Vec<LibraryPlugin>,
    /// Problems found during discovery, in the order they were encountered.
    pub issues: Vec<LoadIssue>,
}

/// Scans the plugin and library roots and instantiates what it finds.
///
/// The `cache` handle is used to locate the proxy's data directory (plugins
/// own `<data_dir>/config/<plugin-id>/`) and to apply the cache quotas a
/// plugin declares in its manifest. The proxy builds one loader for startup
/// and re-uses it for reload passes. Sandbox limits default to
/// [`Limits::default`]; use [`PluginLoader::with_limits`] to apply the
/// `[plugins]` section of `candyriya-config` (see `LimitBuilder::from_config`).
/// Host functions resolve plugin-owned services through the same
/// [`crate::ServiceRegistry`] the loader is built with;
/// use [`PluginLoader::with_services`] to share one with the rest of the
/// proxy (the registry keeps the plugin-owned revocation semantics on
/// reload). The cross-subsystem host functions (`callApi`, the roster,
/// the backend names) instead reach the supervised `host` unit through
/// the link installed by [`PluginLoader::with_host_ipc`].
/// The shared i18n machines are likewise injectable via
/// [`PluginLoader::with_locales`]; without them, plugin host functions
/// resolve against a private embedded-defaults pair and plugin locale
/// files are never registered. The per-plugin health registry goes
/// through [`PluginLoader::with_plugin_health`]; without it the
/// supervision units never observe an unrecoverable plugin. The
/// process-wide plugin-controller registry — the one the native lifecycle
/// story tracks native plugins in — goes through
/// [`PluginLoader::with_plugin_controller`]; without it, native-flavor
/// candidates stay detection-only `Failed` records (see the module docs).
pub struct PluginLoader<'a> {
    plugins_dir: PathBuf,
    libraries_dir: PathBuf,
    cache: &'a CacheManager,
    limits: Limits,
    services: crate::service_registry::ServiceRegistry,
    locales: Option<SharedLocales>,
    host_ipc: HostIpc,
    health: Option<PluginHealthSet>,
    /// The process-wide plugin-controller registry, shared with the rest
    /// of the composition root. `None` means this loader discovers
    /// native candidates but never brings them up.
    controller: Option<Arc<candyriya_plugin_controller::PluginRegistry>>,
}

impl<'a> PluginLoader<'a> {
    /// Store the roots. Directories are not touched until [`PluginLoader::load`].
    pub fn new(plugins_dir: PathBuf, libraries_dir: PathBuf, cache: &'a CacheManager) -> Self {
        Self::with_limits(plugins_dir, libraries_dir, cache, Limits::default())
    }

    /// Same as [`PluginLoader::new`] with explicit sandbox limits.
    pub fn with_limits(
        plugins_dir: PathBuf,
        libraries_dir: PathBuf,
        cache: &'a CacheManager,
        limits: Limits,
    ) -> Self {
        Self {
            plugins_dir,
            libraries_dir,
            cache,
            limits,
            services: crate::service_registry::ServiceRegistry::new(),
            locales: None,
            host_ipc: HostIpc::new(),
            health: None,
            controller: None,
        }
    }

    /// The service registry this loader hands to every plugin's host state.
    ///
    /// Exists for plugin-owned services: entries tagged with an owning
    /// plugin id are revoked in bulk when that plugin unloads or reloads.
    /// Host functions resolve them through this registry; the default is
    /// a fresh, empty one — the composition root should share its own so
    /// reloads see the same registrations.
    #[must_use]
    pub fn with_services(mut self, services: crate::service_registry::ServiceRegistry) -> Self {
        self.services = services;
        self
    }

    /// The shared i18n machines this loader registers plugin locales into
    /// and hands to every plugin's host state.
    ///
    /// Pass the same clones the command layer serves from; plugin
    /// registrations and `/candyriya reload` then apply to the host
    /// functions too. Without this, `[locales]` in a plugin.toml has no
    /// effect on resolution.
    #[must_use]
    pub fn with_locales(mut self, locales: SharedLocales) -> Self {
        self.locales = Some(locales);
        self
    }

    /// The shared link to the supervised `host` unit this loader hands to
    /// every plugin's host state.
    ///
    /// The composition root fills the same slot once the unit exists; an
    /// empty slot means the cross-subsystem host functions answer with
    /// the honest "host unit link is not installed" error. Without this,
    /// plugins loaded by this loader never reach the unit.
    #[must_use]
    pub fn with_host_ipc(mut self, host_ipc: HostIpc) -> Self {
        self.host_ipc = host_ipc;
        self
    }

    /// The shared per-plugin health registry this loader's instances flag
    /// into.
    ///
    /// The composition root installs one process-wide registry (shared
    /// with the reloader and the `plugin-<id>` supervision units). Every
    /// instance this loader builds then clears its plugin's conviction;
    /// without it the units park on a flag nothing can raise.
    #[must_use]
    pub fn with_plugin_health(mut self, health: PluginHealthSet) -> Self {
        self.health = Some(health);
        self
    }

    /// The shared plugin-controller registry this loader brings native
    /// candidates up in.
    ///
    /// The composition root installs one process-wide registry (the same
    /// one the `plugin-<id>` supervision story and the controller unit
    /// keep); the crate root re-exports [`PluginRegistry`](crate::PluginRegistry),
    /// so the same `Arc` clone can be shared without extra imports — the
    /// parameter takes the type as `candyriya_plugin_controller::PluginRegistry`
    /// by that exact path, which is the same type either way. Boot and
    /// `/candyriya reload plugins` both run through this loader, so one
    /// installed registry serves both.
    ///
    /// Without this call, a native candidate is never mapped: it is a
    /// `Failed` record whose reason says bring-up is not wired into this
    /// loader — same visible-not-silent semantics the wasm path uses for
    /// its own failures, never a pretend success.
    #[must_use]
    pub fn with_plugin_controller(
        mut self,
        registry: Arc<candyriya_plugin_controller::PluginRegistry>,
    ) -> Self {
        self.controller = Some(registry);
        self
    }

    /// The sandbox limits this loader applies to every plugin.
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// The plugins root this loader was built with.
    pub fn plugins_dir(&self) -> &Path {
        &self.plugins_dir
    }

    /// The libraries root this loader was built with.
    pub fn libraries_dir(&self) -> &Path {
        &self.libraries_dir
    }

    /// Discover, validate and instantiate every plugin under both roots.
    ///
    /// Never fails as a whole: per-plugin problems land in
    /// [`LoadReport::issues`], wasm-stage problems in the records' states.
    pub fn load(&self) -> LoadReport {
        let mut issues = Vec::new();
        let mut candidates: Vec<Candidate> = Vec::new();

        // libraries load before plugins: stable discovery order for both
        // scanning and the dependency resolver that consumes this list.
        self.scan_dir(
            &self.libraries_dir,
            PluginOrigin::Library,
            &mut candidates,
            &mut issues,
        );
        self.scan_dir(
            &self.plugins_dir,
            PluginOrigin::Plugin,
            &mut candidates,
            &mut issues,
        );

        self.drop_duplicate_ids(&mut candidates, &mut issues);

        // one live-instance registry per discovery run, so the max-instances
        // cap is enforced collectively across every plugin loaded together
        let instance_slots = Arc::new(InstanceSlots::new());
        let plugins = candidates
            .into_iter()
            .map(|c| self.instantiate(c, &instance_slots, &mut issues))
            .collect();

        LoadReport { plugins, issues }
    }

    fn scan_dir(
        &self,
        dir: &Path,
        origin: PluginOrigin,
        candidates: &mut Vec<Candidate>,
        issues: &mut Vec<LoadIssue>,
    ) {
        let entries = match read_entries(dir) {
            Ok(entries) => entries,
            Err(why) => {
                issues.push((dir.to_path_buf(), why));
                return;
            }
        };

        for entry in entries {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                issues.push((path, "failed to determine entry type".to_owned()));
                continue;
            };

            if file_type.is_dir() {
                self.scan_plugin_dir(&path, origin, candidates, issues);
            } else if file_type.is_file() && is_wasm(&path) {
                issues.push((
                    path,
                    "found a .wasm file directly in this directory; plugins live in <id>/ subdirectories".to_owned(),
                ));
            } else if file_type.is_file() && is_native_library_file(&path) {
                issues.push((
                    path,
                    "found a native library directly in this directory; plugins live in <id>/ subdirectories".to_owned(),
                ));
            }
        }
    }

    fn scan_plugin_dir(
        &self,
        dir: &Path,
        origin: PluginOrigin,
        candidates: &mut Vec<Candidate>,
        issues: &mut Vec<LoadIssue>,
    ) {
        let Some(dir_name) = dir.file_name().and_then(|n| n.to_str()).map(str::to_owned) else {
            issues.push((
                dir.to_path_buf(),
                "directory name is not valid UTF-8".to_owned(),
            ));
            return;
        };
        // hidden directories are never plugins (and dot-dirs can't be valid ids)
        if dir_name.starts_with('.') {
            return;
        }

        let manifest_path = dir.join("plugin.toml");
        if !manifest_path.is_file() {
            if let Some(stray) = find_wasm(dir) {
                issues.push((
                    stray,
                    format!(
                        "found a wasm module but no plugin.toml next to it (expected {})",
                        manifest_path.display()
                    ),
                ));
            }
            if let Some(stray) = find_native_library_file(dir) {
                issues.push((
                    stray,
                    format!(
                        "found a native library but no plugin.toml next to it (expected {})",
                        manifest_path.display()
                    ),
                ));
            }
            // no wasm and no native library either: not a plugin directory, ignore silently
            return;
        }

        let content = match std::fs::read_to_string(&manifest_path) {
            Ok(content) => content,
            Err(e) => {
                issues.push((manifest_path, format!("failed to read plugin.toml: {e}")));
                return;
            }
        };
        let manifest = match PluginManifest::from_toml(&content) {
            Ok(manifest) => manifest,
            Err(e) => {
                issues.push((manifest_path, e.to_string()));
                return;
            }
        };
        if manifest.id() != dir_name {
            issues.push((
                manifest_path,
                format!(
                    "manifest id `{}` does not match directory name `{dir_name}`",
                    manifest.id()
                ),
            ));
            return;
        }

        // native flavor detection: a stat, never an open. when both formats
        // sit next to the manifest the native one wins (OQ13) and the wasm
        // module stays untouched on disk
        let native_path = find_native_library(dir, manifest.id());
        let wasm_path = select_wasm(dir, manifest.id());

        match (native_path, wasm_path) {
            (Some(native_path), wasm_path) => {
                if let Ok(wasm_path) = &wasm_path {
                    tracing::warn!(
                        plugin = %manifest.id(),
                        native = %native_path.display(),
                        wasm = %wasm_path.display(),
                        "plugin `{}` ships both a wasm module ({}) and a native library ({}); the native flavor wins (OQ13) and the wasm module is not loaded",
                        manifest.id(),
                        wasm_path.display(),
                        native_path.display(),
                    );
                }
                candidates.push(Candidate {
                    dir: dir.to_path_buf(),
                    manifest,
                    origin,
                    flavor: PluginFormat::Native,
                    wasm_path: wasm_path.ok(),
                    native_path: Some(native_path),
                });
            }
            (None, Ok(wasm_path)) => {
                candidates.push(Candidate {
                    dir: dir.to_path_buf(),
                    manifest,
                    origin,
                    flavor: PluginFormat::Wasm,
                    wasm_path: Some(wasm_path),
                    native_path: None,
                });
            }
            (None, Err(why)) => {
                issues.push((dir.to_path_buf(), why));
            }
        }
    }

    /// Reject candidates whose id collides with an earlier candidate.
    /// First in load order (libraries before plugins, alphabetical within a
    /// root) wins; the rest are reported and dropped.
    fn drop_duplicate_ids(&self, candidates: &mut Vec<Candidate>, issues: &mut Vec<LoadIssue>) {
        let set: Vec<(&str, &Path)> = candidates
            .iter()
            .map(|c| (c.manifest.id(), c.dir.as_path()))
            .collect();
        let Err(duplicates) = validate_unique(set) else {
            return;
        };

        let mut dropped = std::collections::HashSet::new();
        for group in duplicates.groups {
            let mut origins = group.origins.into_iter();
            let keeper = origins
                .next()
                .expect("a duplicate group has at least two origins");
            for origin in origins {
                issues.push((
                    origin.clone(),
                    format!(
                        "duplicate plugin id `{}` (already loaded from {})",
                        group.id,
                        keeper.display()
                    ),
                ));
                dropped.insert(origin);
            }
        }
        candidates.retain(|c| !dropped.contains(&c.dir));
    }

    /// Prepare one discovered plugin for execution. Every failure becomes a
    /// `Failed` record (or an issue if the failure happens before the record
    /// can be built); the other plugins are unaffected either way.
    fn instantiate(
        &self,
        candidate: Candidate,
        instance_slots: &Arc<InstanceSlots>,
        issues: &mut Vec<LoadIssue>,
    ) -> LibraryPlugin {
        let Candidate {
            dir,
            manifest,
            origin,
            flavor,
            wasm_path,
            native_path,
        } = candidate;
        let id = manifest.id().to_owned();

        // the native flavor maps the library, handshakes and enables the
        // plugin right here — the same code path at boot and at
        // `/candyriya reload plugins` (phase B re-scan). the data dirs are
        // applied first, exactly like the wasm path, so a native plugin
        // that refuses bring-up still lands its directories for the
        // record. without a controller installed the native candidate is
        // never mapped: an honest Failed record, never a pretend success.
        if flavor == PluginFormat::Native {
            let native_path = native_path.expect("native flavor always carries its winning path");
            let Some(registry) = &self.controller else {
                let reason = match &wasm_path {
                    Some(wasm_path) => format!(
                        "plugin `{id}` ships both a wasm module ({}) and a native library ({}); the native flavor wins (OQ13), the wasm module is left untouched and native bring-up is not wired into this loader (install one with `PluginLoader::with_plugin_controller`)",
                        wasm_path.display(),
                        native_path.display()
                    ),
                    None => format!(
                        "native library `{}` discovered for plugin `{id}`; no wasm module is present and native bring-up is not wired into this loader (install one with `PluginLoader::with_plugin_controller`)",
                        native_path.display()
                    ),
                };
                issues.push((native_path, reason.clone()));
                return LibraryPlugin::failed(dir, manifest, origin, reason, PluginFormat::Native);
            };
            let registry = Arc::clone(registry);

            // every plugin owns <data_dir>/config/<id>/ and <data_dir>/cache/<id>/;
            // creating them is the same error discipline the wasm path gets —
            // a directory-creation failure lands a Failed record with the reason
            let data_root = self.cache.root().to_path_buf();
            let config_dir = data_root.join("config").join(&id);
            if let Err(e) = std::fs::create_dir_all(&config_dir) {
                let reason = format!(
                    "failed to create plugin config directory {}: {e}",
                    config_dir.display()
                );
                issues.push((config_dir.clone(), reason.clone()));
                return LibraryPlugin::failed(dir, manifest, origin, reason, PluginFormat::Native);
            }
            let cache_dir = data_root.join("cache").join(&id);
            if let Err(e) = std::fs::create_dir_all(&cache_dir) {
                let reason = format!(
                    "failed to create plugin cache directory {}: {e}",
                    cache_dir.display()
                );
                issues.push((cache_dir.clone(), reason.clone()));
                return LibraryPlugin::failed(dir, manifest, origin, reason, PluginFormat::Native);
            }

            // honor the cache quotas the manifest declares, if any
            if let Err(why) = self.apply_cache_quotas(&id, manifest.metadata()) {
                issues.push((dir.clone(), why.clone()));
                return LibraryPlugin::failed(dir, manifest, origin, why, PluginFormat::Native);
            }

            let mut host_state =
                HostState::with_instance_slots(data_root, Arc::clone(instance_slots), &id)
                    .with_services(Arc::new(self.services.clone()))
                    .with_host_ipc(self.host_ipc.clone());
            if let Some(locales) = &self.locales {
                host_state = host_state.with_locales(locales.clone());
            }
            if let Some(health) = &self.health {
                host_state = host_state.with_plugin_health(health.clone());
            }

            // the descriptor carries the manifest identity; the handshake
            // checks it against what the library reports
            let descriptor = PluginDescriptor::native(
                &id,
                &manifest.metadata().name,
                &manifest.metadata().version,
                native_path.clone(),
            );
            return match PluginInstance::native(descriptor, host_state, &id, &self.limits, registry)
            {
                Ok(instance) => {
                    // the conviction reset rides on the host_state health
                    // plumbing: PluginInstance::native clears the plugin's
                    // cell at bring-up time, same discipline as the wasm
                    // path's fresh build
                    LibraryPlugin::new(dir, manifest, origin, instance, PluginFormat::Native)
                }
                Err(why) => {
                    let reason = why.to_string();
                    issues.push((native_path, reason.clone()));
                    LibraryPlugin::failed(dir, manifest, origin, reason, PluginFormat::Native)
                }
            };
        }
        let wasm_path = wasm_path.expect("wasm flavor always carries its module path");

        // every plugin owns <data_dir>/config/<id>/ and <data_dir>/cache/<id>/
        let data_root = self.cache.root().to_path_buf();
        let config_dir = data_root.join("config").join(&id);
        if let Err(e) = std::fs::create_dir_all(&config_dir) {
            let reason = format!(
                "failed to create plugin config directory {}: {e}",
                config_dir.display()
            );
            issues.push((config_dir.clone(), reason.clone()));
            return LibraryPlugin::failed(dir, manifest, origin, reason, PluginFormat::Wasm);
        }
        let cache_dir = data_root.join("cache").join(&id);
        if let Err(e) = std::fs::create_dir_all(&cache_dir) {
            let reason = format!(
                "failed to create plugin cache directory {}: {e}",
                cache_dir.display()
            );
            issues.push((cache_dir.clone(), reason.clone()));
            return LibraryPlugin::failed(dir, manifest, origin, reason, PluginFormat::Wasm);
        }

        // honor the cache quotas the manifest declares, if any
        if let Err(why) = self.apply_cache_quotas(&id, manifest.metadata()) {
            issues.push((dir.clone(), why.clone()));
            return LibraryPlugin::failed(dir, manifest, origin, why, PluginFormat::Wasm);
        }

        let mut host_state =
            HostState::with_instance_slots(data_root, Arc::clone(instance_slots), &id)
                .with_services(Arc::new(self.services.clone()))
                .with_host_ipc(self.host_ipc.clone());
        if let Some(locales) = &self.locales {
            host_state = host_state.with_locales(locales.clone());
        }
        if let Some(health) = &self.health {
            host_state = host_state.with_plugin_health(health.clone());
        }
        let instance = match PluginInstance::new(&wasm_path, host_state, &id, &self.limits) {
            Ok(instance) => instance,
            Err(e) => {
                let reason = e.to_string();
                issues.push((wasm_path.clone(), reason.clone()));
                return LibraryPlugin::failed(dir, manifest, origin, reason, PluginFormat::Wasm);
            }
        };

        // enforce the manifest's contract: plugin_main must exist
        match instance.function_exists(manifest.plugin_main()) {
            Ok(true) => {
                if manifest.has_locales() {
                    self.register_plugin_locales(&id, &dir, issues);
                }
                LibraryPlugin::new(dir, manifest, origin, instance, PluginFormat::Wasm)
            }
            Ok(false) => {
                let reason = format!(
                    "plugin `{id}` declares plugin_main `{}` but the wasm module does not export it",
                    manifest.plugin_main()
                );
                // the instance is useless to us; dropping it closes the worker
                drop(instance);
                issues.push((dir.clone(), reason.clone()));
                LibraryPlugin::failed(dir, manifest, origin, reason, PluginFormat::Wasm)
            }
            Err(e) => {
                let reason = e.to_string();
                issues.push((dir.clone(), reason.clone()));
                LibraryPlugin::failed(dir, manifest, origin, reason, PluginFormat::Wasm)
            }
        }
    }

    /// Register the plugin's `locales/` directory into the shared i18n
    /// machines, when this loader carries one and the manifest declares
    /// `[locales]`. A registration problem is reported as an issue, never
    /// as a load failure — the plugin runs fine without its strings, the
    /// keys just fall back.
    fn register_plugin_locales(&self, id: &str, dir: &Path, issues: &mut Vec<LoadIssue>) {
        let Some(locales) = &self.locales else {
            issues.push((
                dir.join("locales"),
                format!(
                    "plugin `{id}` declares [locales] but no shared i18n machine was provided to the loader; plugin keys will resolve against embedded defaults only"
                ),
            ));
            return;
        };
        let locales_dir = dir.join("locales");
        if let Err(error) = locales.register_plugin_locale(id, &locales_dir) {
            issues.push((
                locales_dir,
                format!("plugin `{id}` locale files not registered: {error}"),
            ));
        }
    }

    fn apply_cache_quotas(
        &self,
        plugin_id: &str,
        metadata: &candyriya_plugin_api::PluginMetadata,
    ) -> Result<(), String> {
        let needs_bytes = metadata.cache_quota_bytes.is_some();
        let needs_resources = metadata.cache_quota_resources.is_some();
        if !needs_bytes && !needs_resources {
            return Ok(());
        }

        let namespace = self
            .cache
            .plugin_cache(plugin_id)
            .map_err(|e| format!("failed to open cache namespace for plugin `{plugin_id}`: {e}"))?;
        if let Some(bytes) = metadata.cache_quota_bytes {
            self.cache.set_quota_bytes(&namespace, bytes);
        }
        if let Some(resources) = metadata.cache_quota_resources {
            self.cache.set_quota_resources(&namespace, resources);
        }
        Ok(())
    }
}

/// One discovery candidate that survived manifest validation.
struct Candidate {
    dir: PathBuf,
    manifest: PluginManifest,
    origin: PluginOrigin,
    /// Which flavor won discovery; the native one wins when both sit next
    /// to the manifest (OQ13).
    flavor: PluginFormat,
    /// The wasm module, when one was found. Always `Some` for the wasm
    /// flavor; for the native flavor it names the module that was found
    /// but will not be loaded.
    wasm_path: Option<PathBuf>,
    /// The native library that won the probe; `Some` exactly when the
    /// flavor is native.
    native_path: Option<PathBuf>,
}

/// Read the entries of `dir`, sorted by file name for deterministic order.
fn read_entries(dir: &Path) -> Result<Vec<std::fs::DirEntry>, String> {
    if !dir.exists() {
        return Err("directory does not exist".to_owned());
    }
    if !dir.is_dir() {
        return Err("not a directory".to_owned());
    }
    let entries = std::fs::read_dir(dir).map_err(|e| format!("failed to read directory: {e}"))?;
    let mut entries: Vec<std::fs::DirEntry> = entries
        .collect::<Result<_, _>>()
        .map_err(|e| format!("failed to read directory entry: {e}"))?;
    entries.sort_by_key(|e| e.file_name());
    Ok(entries)
}

fn is_wasm(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("wasm"))
}

/// Is this file something a native plugin could be? Used for the stray
/// reporting in the roots; the sibling-of-`plugin.toml` probe does its own
/// name-based matching in [`find_native_library`].
fn is_native_library_file(path: &Path) -> bool {
    path.extension().is_some_and(|e| {
        e.eq_ignore_ascii_case("so")
            || e.eq_ignore_ascii_case("dll")
            || e.eq_ignore_ascii_case("dylib")
    })
}

// the one cfg block for the preferred native library extension: `dll` on
// Windows, `dylib` on macOS/iOS, `so` everywhere else (Linux, Android).
// everything else about the fallback mapping lives in `native_library_names`
#[cfg(windows)]
const PREFERRED_NATIVE_EXT: &str = "dll";
#[cfg(any(target_os = "macos", target_os = "ios"))]
const PREFERRED_NATIVE_EXT: &str = "dylib";
#[cfg(not(any(windows, target_os = "macos", target_os = "ios")))]
const PREFERRED_NATIVE_EXT: &str = "so";

/// Candidate file names for a plugin's native library, in probe order:
/// `<id>.<ext>` with this platform's preferred extension first, then cargo's
/// cdylib default `lib<id>.<ext>` on unix (windows has no `lib` prefix). A
/// stock `cargo build -p greeting` drop-in is found without a rename.
fn native_library_names(id: &str) -> Vec<String> {
    let mut names = vec![format!("{id}.{PREFERRED_NATIVE_EXT}")];
    if cfg!(not(windows)) {
        names.push(format!("lib{id}.{PREFERRED_NATIVE_EXT}"));
    }
    names
}

/// The native library sitting directly inside `dir`, in probe order.
/// A stat per candidate, never an open: the library is never mapped,
/// executed or read here. A candidate that exists but is not a regular
/// file is not a candidate at all.
fn find_native_library(dir: &Path, id: &str) -> Option<PathBuf> {
    native_library_names(id)
        .into_iter()
        .map(|name| dir.join(name))
        .find(|path| path.is_file())
}

/// First native-looking file directly inside `dir`, if any (used to detect
/// manifest-less plugin directories).
fn find_native_library_file(dir: &Path) -> Option<PathBuf> {
    let mut matches: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file() && is_native_library_file(p))
        .collect();
    matches.sort();
    matches.into_iter().next()
}

/// First `.wasm` file directly inside `dir`, if any (used to detect
/// manifest-less plugin directories).
fn find_wasm(dir: &Path) -> Option<PathBuf> {
    let mut matches: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file() && is_wasm(p))
        .collect();
    matches.sort();
    matches.into_iter().next()
}

/// Pick the wasm module for a plugin: `<id>.wasm`, else `lib.wasm`.
pub(crate) fn select_wasm(dir: &Path, id: &str) -> Result<PathBuf, String> {
    let id_wasm = dir.join(format!("{id}.wasm"));
    if id_wasm.is_file() {
        return Ok(id_wasm);
    }
    let lib_wasm = dir.join("lib.wasm");
    if lib_wasm.is_file() {
        return Ok(lib_wasm);
    }
    Err(format!(
        "no wasm module found for plugin `{id}` in {} (expected `{id}.wasm`, or `lib.wasm`)",
        dir.display()
    ))
}

#[cfg(test)]
mod tests {
    //! flavor detection and native bring-up. discovery is stat-only; the
    //! bring-up test dlopens the real greeting cdylib and is `#[ignore]`d
    //! by default (build it first with `cargo build --release -p greeting`).
    //! the dummy native libraries written here are empty files — the
    //! detection tests never open them; only the bring-up tests map a
    //! real library (the greeting cdylib, or a deliberately bogus file
    //! for the failure path).
    use super::*;

    use std::fs;
    use std::sync::Arc;

    use crate::library_plugin::PluginState;
    use crate::{PluginRegistry, PluginWireState};
    use candyriya_cache::CacheManager;
    use tempfile::TempDir;

    const GREET_WAT: &str = r#"
(module
  (import "extism:host/env" "alloc" (func $alloc (param i64) (result i64)))
  (import "extism:host/env" "output_set" (func $output_set (param i64 i64)))
  (import "extism:host/env" "store_u8" (func $store_u8 (param i64 i32)))
  (memory (export "memory") 1)
  (func (export "greet") (result i32)
    (local $p i64)
    (local.set $p (call $alloc (i64.const 5)))
    (call $store_u8 (local.get $p) (i32.const 104))
    (call $store_u8 (i64.add (local.get $p) (i64.const 1)) (i32.const 101))
    (call $store_u8 (i64.add (local.get $p) (i64.const 2)) (i32.const 108))
    (call $store_u8 (i64.add (local.get $p) (i64.const 3)) (i32.const 108))
    (call $store_u8 (i64.add (local.get $p) (i64.const 4)) (i32.const 111))
    (call $output_set (local.get $p) (i64.const 5))
    (i32.const 0)))
"#;

    struct Fixture {
        _root: TempDir,
        plugins_dir: PathBuf,
        libraries_dir: PathBuf,
        cache_root: PathBuf,
        cache: CacheManager,
    }

    impl Fixture {
        fn new() -> Self {
            let root = TempDir::new().expect("tempdir");
            let plugins_dir = root.path().join("plugins");
            let libraries_dir = root.path().join("libraries");
            let cache_root = root.path().join("data");
            fs::create_dir_all(&plugins_dir).expect("plugins dir");
            fs::create_dir_all(&libraries_dir).expect("libraries dir");
            fs::create_dir_all(&cache_root).expect("cache root");
            let cache = CacheManager::new(&cache_root).expect("cache manager");
            Self {
                _root: root,
                plugins_dir,
                libraries_dir,
                cache_root,
                cache,
            }
        }

        fn load(&self) -> LoadReport {
            PluginLoader::new(
                self.plugins_dir.clone(),
                self.libraries_dir.clone(),
                &self.cache,
            )
            .load()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = self.cache.shutdown();
        }
    }

    fn manifest(id: &str, plugin_main: &str) -> String {
        format!(
            "id = \"{id}\"\nversion = \"0.1.0\"\nname = \"Test plugin {id}\"\nplugin_main = \"{plugin_main}\"\nplugin_api = \"0.1\"\n"
        )
    }

    fn greet_wasm() -> Vec<u8> {
        wat::parse_str(GREET_WAT).expect("greet wat compiles")
    }

    fn write_manifest(dir: &Path, id: &str) {
        fs::create_dir_all(dir.join(id)).expect("plugin dir");
        fs::write(dir.join(id).join("plugin.toml"), manifest(id, "greet")).expect("manifest");
    }

    /// the empty dummy library next to the manifest; the name is whatever
    /// this platform probes first, so the file is always the winner
    fn dummy_native_library(dir: &Path, id: &str) -> PathBuf {
        let name = native_library_names(id)
            .into_iter()
            .next()
            .expect("at least one candidate name");
        let path = dir.join(id).join(name);
        fs::write(&path, b"").expect("dummy native library");
        path
    }

    /// events collected by the capture subscriber; the state is shared
    /// through an Arc because `with_default` consumes the subscriber
    #[derive(Clone, Default)]
    struct CapturedEvents {
        events: Arc<std::sync::Mutex<Vec<(tracing::Level, String)>>>,
    }

    impl CapturedEvents {
        fn warn_message(&self) -> Option<String> {
            self.events
                .lock()
                .expect("captured events")
                .iter()
                .find(|(level, _)| *level == tracing::Level::WARN)
                .map(|(_, message)| message.clone())
        }
    }

    impl tracing::Subscriber for CapturedEvents {
        fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            let mut visitor = MessageVisitor::default();
            event.record(&mut visitor);
            self.events
                .lock()
                .expect("captured events")
                .push((*event.metadata().level(), visitor.message));
        }

        fn enter(&self, _span: &tracing::span::Id) {}

        fn exit(&self, _span: &tracing::span::Id) {}
    }

    #[derive(Default)]
    struct MessageVisitor {
        message: String,
    }

    impl tracing::field::Visit for MessageVisitor {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            use std::fmt::Write as _;
            let _ = write!(self.message, "{}={value:?};", field.name());
        }

        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            use std::fmt::Write as _;
            let _ = write!(self.message, "{}={value};", field.name());
        }
    }

    #[test]
    fn native_library_names_probe_order() {
        let names = native_library_names("widget");
        #[cfg(windows)]
        assert_eq!(names, ["widget.dll".to_owned()]);
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        assert_eq!(
            names,
            ["widget.dylib".to_owned(), "libwidget.dylib".to_owned()]
        );
        #[cfg(not(any(windows, target_os = "macos", target_os = "ios")))]
        assert_eq!(names, ["widget.so".to_owned(), "libwidget.so".to_owned()]);
    }

    #[test]
    fn wasm_only_plugin_stays_wasm_flavor_and_loads() {
        let fx = Fixture::new();
        write_manifest(&fx.plugins_dir, "wasm-only");
        fs::write(
            fx.plugins_dir.join("wasm-only").join("wasm-only.wasm"),
            greet_wasm(),
        )
        .expect("wasm");

        let report = fx.load();

        assert_eq!(report.plugins.len(), 1);
        let plugin = &report.plugins[0];
        assert_eq!(plugin.flavor(), PluginFormat::Wasm);
        assert!(plugin.state().is_loaded(), "issues: {:?}", report.issues);
        assert!(report.issues.is_empty());
    }

    #[test]
    fn native_only_plugin_is_a_failed_record_with_reason() {
        let fx = Fixture::new();
        write_manifest(&fx.plugins_dir, "native-only");
        let native_path = dummy_native_library(&fx.plugins_dir, "native-only");

        let report = fx.load();

        assert_eq!(report.plugins.len(), 1);
        let plugin = &report.plugins[0];
        assert_eq!(plugin.flavor(), PluginFormat::Native);
        // without a controller the native candidate is never brought up:
        // no instance, no mapping, the record is Failed
        assert!(plugin.instance().is_none());
        let native = native_path.display().to_string();
        match plugin.state() {
            PluginState::Failed(reason) => {
                assert!(
                    reason.contains(&native),
                    "reason names the library: {reason}"
                )
            }
            other => panic!("native-only plugin must be Failed, got {other:?}"),
        }
        // the same reason is listed in issues, visible-not-silent
        assert!(
            report.issues.iter().any(|(_, why)| why.contains(&native)),
            "issues name the native library: {:?}",
            report.issues
        );
    }

    #[test]
    fn native_only_plugin_creates_no_data_dirs() {
        let fx = Fixture::new();
        write_manifest(&fx.plugins_dir, "side-effects");
        dummy_native_library(&fx.plugins_dir, "side-effects");

        let report = fx.load();
        assert_eq!(report.plugins.len(), 1);

        // without a controller the native candidate is never prepared:
        // no config/cache dirs for a plugin this loader never brought up
        assert!(!fx.cache_root.join("config").join("side-effects").exists());
        assert!(!fx.cache_root.join("cache").join("side-effects").exists());
    }

    #[test]
    fn both_present_native_wins_and_warn_names_id_and_both_paths() {
        let fx = Fixture::new();
        write_manifest(&fx.plugins_dir, "dual");
        let wasm_path = fx.plugins_dir.join("dual").join("dual.wasm");
        let wasm_bytes = greet_wasm();
        fs::write(&wasm_path, &wasm_bytes).expect("wasm");
        let native_path = dummy_native_library(&fx.plugins_dir, "dual");

        let captured = CapturedEvents::default();
        let probe = captured.clone();
        let report = tracing::subscriber::with_default(captured, || fx.load());

        assert_eq!(report.plugins.len(), 1);
        let plugin = &report.plugins[0];
        assert_eq!(plugin.flavor(), PluginFormat::Native);
        assert!(plugin.instance().is_none());
        let wasm = wasm_path.display().to_string();
        let native = native_path.display().to_string();
        match plugin.state() {
            PluginState::Failed(reason) => {
                assert!(
                    reason.contains(&wasm),
                    "reason names the wasm module: {reason}"
                );
                assert!(
                    reason.contains(&native),
                    "reason names the native library: {reason}"
                );
            }
            other => panic!("both-present plugin must be Failed, got {other:?}"),
        }
        // the wasm module is untouched on disk
        assert_eq!(fs::read(&wasm_path).expect("wasm readable"), wasm_bytes);
        // one WARN, naming the plugin id and both paths
        let warn = probe.warn_message().expect("an OQ13 warning is logged");
        assert!(warn.contains("dual"), "warn names the plugin id: {warn}");
        assert!(warn.contains(&wasm), "warn names the wasm module: {warn}");
        assert!(
            warn.contains(&native),
            "warn names the native library: {warn}"
        );
    }

    #[test]
    fn manifest_without_native_library_stays_wasm_flavor() {
        let fx = Fixture::new();
        // nothing next to the manifest at all: today's behavior, unchanged
        write_manifest(&fx.plugins_dir, "empty-manifest");
        // a garbage wasm module: today's behavior, now tagged with its flavor
        write_manifest(&fx.plugins_dir, "garbage");
        fs::write(
            fx.plugins_dir.join("garbage").join("garbage.wasm"),
            b"\x00asm\xde\xad not wasm",
        )
        .expect("garbage wasm");

        let report = fx.load();

        // no wasm and no native library: no record, today's issue text
        assert!(
            !report.plugins.iter().any(|p| p.id() == "empty-manifest"),
            "no manifest-only record is produced"
        );
        assert!(
            report
                .issues
                .iter()
                .any(|(_, why)| why.contains("no wasm module found")),
            "today's issue text is preserved: {:?}",
            report.issues
        );
        // garbage wasm: Failed as today, flavor Wasm
        let garbage = report
            .plugins
            .iter()
            .find(|p| p.id() == "garbage")
            .expect("garbage record");
        assert_eq!(garbage.flavor(), PluginFormat::Wasm);
        assert!(matches!(garbage.state(), PluginState::Failed(_)));
    }

    #[test]
    fn native_candidate_that_is_a_directory_degrades_to_todays_behavior() {
        let fx = Fixture::new();
        // malformed layout: <id>.<ext> exists but is a directory, and there
        // is no wasm module -> exactly what the loader reported before
        // the native probe existed
        write_manifest(&fx.plugins_dir, "broken");
        let name = native_library_names("broken")
            .into_iter()
            .next()
            .expect("at least one candidate name");
        fs::create_dir_all(fx.plugins_dir.join("broken").join(name)).expect("candidate as dir");

        let report = fx.load();

        assert!(report.plugins.is_empty());
        assert!(
            report
                .issues
                .iter()
                .any(|(_, why)| why.contains("no wasm module found")),
            "malformed native layout falls back to today's issue: {:?}",
            report.issues
        );
    }

    #[test]
    fn native_detection_ignores_library_contents() {
        let fx = Fixture::new();
        write_manifest(&fx.plugins_dir, "garbage-native");
        let name = native_library_names("garbage-native")
            .into_iter()
            .next()
            .expect("at least one candidate name");
        // bytes that are not a dynamic library anywhere; detection never parses them
        fs::write(
            fx.plugins_dir.join("garbage-native").join(name),
            b"\x00\x01\x02definitely not a library\xff\xfe",
        )
        .expect("garbage library");

        let report = fx.load();

        assert_eq!(report.plugins.len(), 1);
        assert_eq!(report.plugins[0].flavor(), PluginFormat::Native);
    }

    #[cfg(not(windows))]
    #[test]
    fn cargo_default_lib_prefix_is_probed_after_preferred() {
        let fx = Fixture::new();
        write_manifest(&fx.plugins_dir, "prefixed");
        // only the cargo cdylib default (`lib<id>.<ext>`) exists
        let names = native_library_names("prefixed");
        assert_eq!(
            names.len(),
            2,
            "unix probes the bare and the lib-prefixed name"
        );
        fs::write(fx.plugins_dir.join("prefixed").join(&names[1]), b"")
            .expect("prefixed dummy library");

        let report = fx.load();

        assert_eq!(report.plugins.len(), 1);
        assert_eq!(report.plugins[0].flavor(), PluginFormat::Native);

        // the preferred name wins when both are present
        fs::write(fx.plugins_dir.join("prefixed").join(&names[0]), b"")
            .expect("preferred dummy library");
        let report = fx.load();
        assert_eq!(report.plugins.len(), 1);
        let plugin = &report.plugins[0];
        assert_eq!(plugin.flavor(), PluginFormat::Native);
        let preferred = fx
            .plugins_dir
            .join("prefixed")
            .join(&names[0])
            .display()
            .to_string();
        assert!(
            report
                .issues
                .iter()
                .any(|(_, why)| why.contains(&preferred)),
            "the preferred probe wins: {:?}",
            report.issues
        );
    }

    #[test]
    fn stray_native_library_without_manifest_is_reported() {
        let fx = Fixture::new();
        // a directory carrying only a native library, no plugin.toml
        let stray = fx.plugins_dir.join("stray-native");
        fs::create_dir_all(&stray).expect("plugin dir");
        let name = native_library_names("stray-native")
            .into_iter()
            .next()
            .expect("at least one candidate name");
        let stray_file = stray.join(name);
        fs::write(&stray_file, b"").expect("stray library");

        let report = fx.load();

        assert!(report.plugins.is_empty());
        assert!(
            report
                .issues
                .iter()
                .any(|(where_, why)| { *where_ == stray_file && why.contains("no plugin.toml") }),
            "the stray is reported: {:?}",
            report.issues
        );
    }

    #[test]
    fn stray_native_library_directly_in_root_is_reported() {
        let fx = Fixture::new();
        let name = native_library_names("loose")
            .into_iter()
            .next()
            .expect("a name");
        let stray_file = fx.plugins_dir.join(name);
        fs::write(&stray_file, b"").expect("loose library");

        let report = fx.load();

        assert!(
            report.issues.iter().any(|(where_, why)| {
                *where_ == stray_file && why.contains("directly in this directory")
            }),
            "the loose library is reported: {:?}",
            report.issues
        );
    }

    #[test]
    fn discovery_order_is_preserved_with_mixed_flavors() {
        let fx = Fixture::new();
        // libraries: native-only alpha, wasm beta; plugins: native-only delta, wasm gamma
        write_manifest(&fx.libraries_dir, "alpha-native");
        dummy_native_library(&fx.libraries_dir, "alpha-native");
        write_manifest(&fx.libraries_dir, "beta-wasm");
        fs::write(
            fx.libraries_dir.join("beta-wasm").join("beta-wasm.wasm"),
            greet_wasm(),
        )
        .expect("wasm");
        write_manifest(&fx.plugins_dir, "delta-native");
        dummy_native_library(&fx.plugins_dir, "delta-native");
        write_manifest(&fx.plugins_dir, "gamma-wasm");
        fs::write(
            fx.plugins_dir.join("gamma-wasm").join("gamma-wasm.wasm"),
            greet_wasm(),
        )
        .expect("wasm");

        let report = fx.load();

        let ids: Vec<&str> = report.plugins.iter().map(|p| p.id()).collect();
        assert_eq!(
            ids,
            ["alpha-native", "beta-wasm", "delta-native", "gamma-wasm"]
        );
        let flavors: Vec<PluginFormat> = report.plugins.iter().map(|p| p.flavor()).collect();
        assert_eq!(
            flavors,
            [
                PluginFormat::Native,
                PluginFormat::Wasm,
                PluginFormat::Native,
                PluginFormat::Wasm,
            ]
        );
    }

    #[test]
    fn duplicate_id_across_roots_keeps_first_in_load_order() {
        let fx = Fixture::new();
        // the library root is scanned first: its native-only candidate wins
        // over the plugins-root wasm candidate with the same id
        write_manifest(&fx.libraries_dir, "dual");
        dummy_native_library(&fx.libraries_dir, "dual");
        write_manifest(&fx.plugins_dir, "dual");
        fs::write(fx.plugins_dir.join("dual").join("dual.wasm"), greet_wasm()).expect("wasm");

        let report = fx.load();

        assert_eq!(report.plugins.len(), 1);
        let keeper = &report.plugins[0];
        assert_eq!(keeper.origin(), PluginOrigin::Library);
        assert_eq!(keeper.flavor(), PluginFormat::Native);
        assert!(
            report.issues.iter().any(|(where_, why)| {
                *where_ == fx.plugins_dir.join("dual") && why.contains("duplicate")
            }),
            "the dropped copy is reported: {:?}",
            report.issues
        );
    }

    /// Locate the reference cdylib for the bring-up test.
    ///
    /// Workspace layout: `crates/plugin/candyriya-plugin`, the target
    /// directory is three levels up (or wherever `CARGO_TARGET_DIR`
    /// points). Release first, debug as a fallback — same lookup the
    /// `tests/native.rs` integration test uses.
    fn greeting_cdylib() -> PathBuf {
        let artifact = if cfg!(windows) {
            "greeting.dll"
        } else if cfg!(target_os = "macos") {
            "libgreeting.dylib"
        } else {
            "libgreeting.so"
        };
        let manifest = env!("CARGO_MANIFEST_DIR");
        let target_dir = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(manifest).join("../../..").join("target"));
        for profile in ["release", "debug"] {
            let candidate = target_dir.join(profile).join(artifact);
            if candidate.exists() {
                return candidate;
            }
        }
        panic!(
            "reference cdylib `{artifact}` not found under {}; build it first with `cargo build --release -p greeting`",
            target_dir.display()
        );
    }

    #[test]
    #[ignore = "requires the greeting cdylib: cargo build --release -p greeting"]
    fn native_plugin_brings_up_through_the_loader() {
        let fx = Fixture::new();
        write_manifest(&fx.plugins_dir, "greeting");
        // the fixture layout discovery probes: <id>.<ext> for this
        // platform, i.e. exactly what `find_native_library` looks for
        let artifact = greeting_cdylib();
        let preferred = native_library_names("greeting")
            .into_iter()
            .next()
            .expect("at least one candidate name");
        std::fs::copy(&artifact, fx.plugins_dir.join("greeting").join(preferred))
            .expect("copy the cdylib into the fixture");

        let registry = Arc::new(PluginRegistry::new());
        let report = PluginLoader::new(fx.plugins_dir.clone(), fx.libraries_dir.clone(), &fx.cache)
            .with_plugin_controller(Arc::clone(&registry))
            .load();

        assert_eq!(report.plugins.len(), 1);
        let plugin = &report.plugins[0];
        assert_eq!(plugin.flavor(), PluginFormat::Native);
        assert!(plugin.state().is_loaded(), "issues: {:?}", report.issues);
        let instance = plugin.instance().expect("a live instance");
        assert_eq!(
            instance.descriptor().wasm_or_native_format,
            PluginFormat::Native
        );
        // the handshake completed and the plugin echoed ABI v1
        assert_eq!(instance.descriptor().abi_version, Some(1));
        // a clean bring-up is silent on the issues list
        assert!(report.issues.is_empty(), "issues: {:?}", report.issues);
        // the data dirs were created before the bring-up, like the wasm path
        assert!(fx.cache_root.join("config").join("greeting").is_dir());
        assert!(fx.cache_root.join("cache").join("greeting").is_dir());
        // the shared registry walked the ladder to RUNNING
        let record = registry.get("greeting").expect("the bring-up was tracked");
        assert_eq!(record.state(), PluginWireState::Running);
        assert_eq!(record.reason, None);
    }

    #[test]
    fn native_without_a_controller_is_a_failed_record_nothing_mapped() {
        let fx = Fixture::new();
        write_manifest(&fx.plugins_dir, "no-controller");
        let native_path = dummy_native_library(&fx.plugins_dir, "no-controller");

        // deliberately built WITHOUT with_plugin_controller
        let registry = Arc::new(PluginRegistry::new());
        let report = fx.load();

        assert_eq!(report.plugins.len(), 1);
        let plugin = &report.plugins[0];
        assert_eq!(plugin.flavor(), PluginFormat::Native);
        assert!(plugin.instance().is_none());
        let reason = match plugin.state() {
            PluginState::Failed(reason) => reason.clone(),
            other => {
                panic!("a native candidate without a controller must be Failed, got {other:?}")
            }
        };
        let native = native_path.display().to_string();
        assert!(
            reason.contains(&native),
            "the reason names the library: {reason}"
        );
        assert!(
            reason.contains("not wired"),
            "the reason says bring-up is not wired: {reason}"
        );
        assert!(
            report
                .issues
                .iter()
                .any(|(_, why)| why.contains("not wired")),
            "issues name the not-wired reason: {:?}",
            report.issues
        );
        // nothing was mapped, no dirs were created, the registry stayed empty
        assert!(!fx.cache_root.join("config").join("no-controller").exists());
        assert!(!fx.cache_root.join("cache").join("no-controller").exists());
        assert!(registry.is_empty());
    }

    #[test]
    fn native_with_a_bogus_library_fails_at_open() {
        let fx = Fixture::new();
        write_manifest(&fx.plugins_dir, "bogus");
        let name = native_library_names("bogus")
            .into_iter()
            .next()
            .expect("at least one candidate name");
        let bogus = fx.plugins_dir.join("bogus").join(&name);
        // never a loadable library on any platform — the OS refuses it at the open step
        fs::write(&bogus, b"this is not a dynamic library").expect("bogus library");

        let registry = Arc::new(PluginRegistry::new());
        let report = PluginLoader::new(fx.plugins_dir.clone(), fx.libraries_dir.clone(), &fx.cache)
            .with_plugin_controller(Arc::clone(&registry))
            .load();

        assert_eq!(report.plugins.len(), 1);
        let plugin = &report.plugins[0];
        assert_eq!(plugin.flavor(), PluginFormat::Native);
        assert!(plugin.instance().is_none());
        let reason = match plugin.state() {
            PluginState::Failed(reason) => reason.clone(),
            other => panic!("a bogus library must be Failed, got {other:?}"),
        };
        assert!(
            reason.contains("failed to load"),
            "the reason names the open step that failed: {reason}"
        );
        assert!(!plugin.state().is_loaded());
        // the bring-up was attempted and tracked: terminal UNLOADED with the
        // reason preserved, never RUNNING
        let record = registry.get("bogus").expect("the attempt was tracked");
        assert_eq!(record.state(), PluginWireState::Unloaded);
        assert!(record.reason.is_some());
        // the data dirs were created before the bring-up, same as the wasm path
        assert!(fx.cache_root.join("config").join("bogus").is_dir());
        assert!(fx.cache_root.join("cache").join("bogus").is_dir());
        // the refusal is listed in the issues, visible-not-silent
        assert!(
            report
                .issues
                .iter()
                .any(|(_, why)| why.contains("failed to load")),
            "issues name the open failure: {:?}",
            report.issues
        );
    }
}
