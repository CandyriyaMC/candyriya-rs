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
//! # Failure policy
//!
//! Discovery never fails the whole load. Everything that is wrong with one
//! plugin — a stray `.wasm` with no manifest, an unreadable or invalid
//! `plugin.toml`, an id that does not match its directory, a duplicate id,
//! a missing module — is collected as a [`LoadIssue`] `(where, why)` tuple.
//! A plugin whose wasm compiles but fails preparation (missing `plugin_main`
//! export, uncreatable data directory, cache quota rejected) is recorded as
//! a [`LibraryPlugin`] in [`PluginState::Failed`] — visible, not silent — and
//! its reason is also listed in `issues`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use candyriya_cache::CacheManager;

use crate::host_state::HostState;
use crate::instance::PluginInstance;
use crate::library_plugin::{LibraryPlugin, PluginOrigin};
use crate::limits::{InstanceSlots, Limits};
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
/// Host functions see the same [`crate::ServiceRegistry`] the loader is built with;
/// use [`PluginLoader::with_services`] to share one with the rest of the
/// proxy (the dispatcher is required for `candyriya:callApi` to work).
pub struct PluginLoader<'a> {
    plugins_dir: PathBuf,
    libraries_dir: PathBuf,
    cache: &'a CacheManager,
    limits: Limits,
    services: crate::service_registry::ServiceRegistry,
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
        }
    }

    /// The service registry this loader hands to every plugin's host state.
    ///
    /// Host functions resolve cross-subsystem handles (the event dispatcher
    /// for `candyriya:callApi`, the player roster, ...) through this
    /// registry. The default is a fresh, empty registry — the composition
    /// root should share its own so the plugins see the same services as
    /// the rest of the proxy.
    #[must_use]
    pub fn with_services(mut self, services: crate::service_registry::ServiceRegistry) -> Self {
        self.services = services;
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
            // no wasm either: not a plugin directory, ignore silently
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

        let wasm_path = match select_wasm(dir, manifest.id()) {
            Ok(path) => path,
            Err(why) => {
                issues.push((dir.to_path_buf(), why));
                return;
            }
        };

        candidates.push(Candidate {
            dir: dir.to_path_buf(),
            manifest,
            origin,
            wasm_path,
        });
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
            wasm_path,
        } = candidate;
        let id = manifest.id().to_owned();

        // every plugin owns <data_dir>/config/<id>/ and <data_dir>/cache/<id>/
        let data_root = self.cache.root().to_path_buf();
        let config_dir = data_root.join("config").join(&id);
        if let Err(e) = std::fs::create_dir_all(&config_dir) {
            let reason = format!(
                "failed to create plugin config directory {}: {e}",
                config_dir.display()
            );
            issues.push((config_dir.clone(), reason.clone()));
            return LibraryPlugin::failed(dir, manifest, origin, reason);
        }
        let cache_dir = data_root.join("cache").join(&id);
        if let Err(e) = std::fs::create_dir_all(&cache_dir) {
            let reason = format!(
                "failed to create plugin cache directory {}: {e}",
                cache_dir.display()
            );
            issues.push((cache_dir.clone(), reason.clone()));
            return LibraryPlugin::failed(dir, manifest, origin, reason);
        }

        // honor the cache quotas the manifest declares, if any
        if let Err(why) = self.apply_cache_quotas(&id, manifest.metadata()) {
            issues.push((dir.clone(), why.clone()));
            return LibraryPlugin::failed(dir, manifest, origin, why);
        }

        let host_state = HostState::with_instance_slots(data_root, Arc::clone(instance_slots), &id)
            .with_services(Arc::new(self.services.clone()));
        let instance = match PluginInstance::new(&wasm_path, host_state, &id, &self.limits) {
            Ok(instance) => instance,
            Err(e) => {
                let reason = e.to_string();
                issues.push((wasm_path.clone(), reason.clone()));
                return LibraryPlugin::failed(dir, manifest, origin, reason);
            }
        };

        // enforce the manifest's contract: plugin_main must exist
        match instance.function_exists(manifest.plugin_main()) {
            Ok(true) => LibraryPlugin::new(dir, manifest, origin, instance),
            Ok(false) => {
                let reason = format!(
                    "plugin `{id}` declares plugin_main `{}` but the wasm module does not export it",
                    manifest.plugin_main()
                );
                // the instance is useless to us; dropping it closes the worker
                drop(instance);
                issues.push((dir.clone(), reason.clone()));
                LibraryPlugin::failed(dir, manifest, origin, reason)
            }
            Err(e) => {
                let reason = e.to_string();
                issues.push((dir.clone(), reason.clone()));
                LibraryPlugin::failed(dir, manifest, origin, reason)
            }
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
    wasm_path: PathBuf,
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
