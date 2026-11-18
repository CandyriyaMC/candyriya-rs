//! Host-side state owned by each plugin instance.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;

use candyriya_plugin_api::OutcomeReport;

use crate::health::{PluginHealth, PluginHealthSet};
use crate::host_ipc::HostIpc;
use crate::limits::InstanceSlots;
use crate::locales::SharedLocales;
use crate::service_registry::ServiceRegistry;

/// State the host keeps per plugin and hands to the host functions.
///
/// The plugin's registration facts live here: its id, the shared
/// [`ServiceRegistry`] it looks other subsystems up in, the active i18n
/// locale and the two directories it owns outright. Host functions read
/// the plugin's identity *from here* — never from anything the plugin
/// sends over the ABI — so a plugin cannot act on behalf of another.
///
/// # Directories
///
/// `data_dir` is the directory this plugin owns: `<data_dir>/config/<plugin-id>/`
/// in the default layout. `cache_dir` (`<data_dir>/cache/<plugin-id>/`) and
/// `config_dir` (the same path as `data_dir`) are split out so host
/// functions do not have to reconstruct them; the loader creates both
/// before instantiation and a plugin whose directories cannot be created is
/// rejected before it ever reaches wasm.
///
/// # Caller identity
///
/// `caller_plugin_id` is set by the host every time it invokes an export on
/// the plugin (event dispatch, or `callApi` from another plugin) and restored
/// afterwards. Host functions use it to let the *callee* authorize the
/// *caller* — at the top level of a dispatch it is `None` (the proxy itself
/// is calling).
///
/// `subscribers` is the set of event kinds the plugin opted into via the
/// `candyriya:subscribeToEvents` host function; an empty set means *every*
/// kind (the default). `pending_outcome` carries a cancellation recorded
/// through `candyriya:cancelEvent` for the call currently in flight; the
/// caller clears it before the call and merges it into the reported
/// [`OutcomeReport`] afterwards.
///
/// `instance_slots` is the live-instance registry [`crate::PluginInstance`]
/// checks for the `max-instances` limit. [`HostState::new`] creates a fresh,
/// private registry (standalone host states and hot-reload staging each get
/// their own); [`HostState::with_instance_slots`] shares one registry across
/// every host state created for a single scope (e.g. one loader run). The
/// loader uses the shared form so its whole run enforces the cap
/// collectively.
///
/// `locales` carries the shared pair of i18n machines `candyriya:i18nT`
/// resolves through. [`HostState::new`] binds a private embedded-defaults
/// pair so standalone host states and tests still resolve core keys; the
/// loader and reloader replace it with the proxy's shared pair via
/// [`HostState::with_locales`], which is what makes plugin locale
/// registrations and `/candyriya reload` visible to the host functions.
///
/// `host_ipc` is the shared link to the supervised `host` unit, which
/// serves the cross-subsystem host functions (`callApi`,
/// `getPlayerRoster`, `listServers`, `pingPlayer`). It starts empty in a
/// fresh host state; the composition root installs the handles once the
/// unit exists, and every host state cloned from the same slot sees them.
/// A host function called before the installation answers with the honest
/// `host unit link is not installed` error instead of a fabricated value.
///
/// `health` is the shared registry of per-plugin health cells the wasm
/// worker flags on genuinely abnormal outcomes (fuel exhaustion,
/// manifest-timeout interruption, host-side panic). The composition root
/// installs one process-wide registry; every fresh instance build clears
/// the plugin's conviction there — a restage is a request for a clean
/// slate. See [`crate::PluginHealthSet`].
#[derive(Clone)]
pub struct HostState {
    /// The directory this plugin owns (the same path as `config_dir`).
    /// Created by the loader if missing.
    pub data_dir: PathBuf,
    /// Services shared across one load scope; registered during plugin
    /// load and consulted by host functions instead of touching subsystems
    /// directly.
    pub services: Arc<ServiceRegistry>,
    /// The plugin's id — the identity host functions authorize against.
    pub plugin_id: String,
    /// The active locale for the plugin (e.g. `en`, `ex_girl`). Defaults to
    /// `en`; the loader may set it from the proxy configuration.
    pub i18n_locale: Arc<Mutex<String>>,
    /// `<data_dir>/cache/<plugin-id>/` — the plugin's cache namespace root.
    pub cache_dir: PathBuf,
    /// `<data_dir>/config/<plugin-id>/` — the plugin's configuration root.
    pub config_dir: PathBuf,
    /// Event kinds the plugin subscribed to. Empty means all kinds.
    pub subscribers: Arc<Mutex<HashSet<String>>>,
    /// The plugin that initiated the call currently in flight, if any;
    /// `None` when the proxy itself is the caller.
    pub caller_plugin_id: Arc<Mutex<Option<String>>>,
    /// The shared pair of i18n machines `candyriya:i18nT` resolves
    /// through. `None` only when even the embedded tables fail to load
    /// (a crate build defect); the host function then answers with the
    /// key itself, like any other missing key.
    pub locales: Option<SharedLocales>,
    /// Cancellation recorded via `candyriya:cancelEvent` for the call in
    /// flight. Cleared by the caller before the call and read back after.
    pub(crate) pending_outcome: Arc<Mutex<Option<OutcomeReport>>>,
    /// The shared link to the supervised `host` unit; see the module docs.
    pub host_ipc: HostIpc,
    instance_slots: Arc<InstanceSlots>,
    /// The shared per-plugin health registry, when the host opted into
    /// supervision. `None` for standalone host states and tests.
    health: Option<PluginHealthSet>,
}

impl std::fmt::Debug for HostState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostState")
            .field("data_dir", &self.data_dir)
            .field("plugin_id", &self.plugin_id)
            .field("cache_dir", &self.cache_dir)
            .field("config_dir", &self.config_dir)
            .field("i18n_locale", &self.i18n_locale.lock())
            .finish_non_exhaustive()
    }
}

impl HostState {
    /// Wrap the proxy's data root in host state for the plugin `plugin_id`,
    /// with a fresh, private instance registry.
    ///
    /// The plugin's own directories are derived from `data_dir` (the proxy's
    /// data root): `<data_dir>/config/<plugin_id>/` and `<data_dir>/cache/<plugin_id>/`.
    pub fn new(data_dir: PathBuf, plugin_id: &str) -> Self {
        Self::with_instance_slots(data_dir, Arc::new(InstanceSlots::new()), plugin_id)
    }

    /// Wrap the proxy's data root in host state sharing an existing instance
    /// registry.
    pub fn with_instance_slots(
        data_dir: PathBuf,
        instance_slots: Arc<InstanceSlots>,
        plugin_id: &str,
    ) -> Self {
        let config_dir = data_dir.join("config").join(plugin_id);
        let cache_dir = data_dir.join("cache").join(plugin_id);
        Self {
            data_dir: config_dir.clone(),
            services: Arc::new(ServiceRegistry::new()),
            plugin_id: plugin_id.to_owned(),
            i18n_locale: Arc::new(Mutex::new("en".to_owned())),
            cache_dir,
            config_dir,
            subscribers: Arc::new(Mutex::new(HashSet::new())),
            caller_plugin_id: Arc::new(Mutex::new(None)),
            pending_outcome: Arc::new(Mutex::new(None)),
            host_ipc: HostIpc::new(),
            locales: SharedLocales::embedded().ok(),
            instance_slots,
            health: None,
        }
    }

    /// Point the instance registry at the same counts as `services`' scope.
    ///
    /// Consumes and returns `self`; used by the loader so the whole run
    /// enforces `max-instances` collectively.
    pub fn with_services(mut self, services: Arc<ServiceRegistry>) -> Self {
        self.services = services;
        self
    }

    /// Override the active locale for this plugin (default `en`).
    #[must_use]
    pub fn with_locale(self, locale: &str) -> Self {
        *self.i18n_locale.lock() = locale.to_owned();
        self
    }

    /// Install the shared per-plugin health registry.
    ///
    /// The loader, reloader and supervision units all clone the same
    /// registry; every fresh instance build then clears the plugin's
    /// conviction in it (see the module docs). Without this call the
    /// worker never flags a health cell and the `plugin-<id>` unit parks
    /// on a flag nothing can raise.
    #[must_use]
    pub fn with_plugin_health(mut self, health: PluginHealthSet) -> Self {
        self.health = Some(health);
        self
    }

    /// Resolve this build's health cell: get-or-create, then clear any
    /// conviction left by a previous instance. `None` only when no
    /// registry was installed — the worker then behaves as if the cell
    /// does not exist.
    pub(crate) fn health_cell(&self, plugin_id: &str) -> Option<Arc<PluginHealth>> {
        self.health.as_ref().map(|set| set.fresh_cell(plugin_id))
    }

    /// Point this host state at the shared link to the supervised `host`
    /// unit. The loader and reloader do this with one process-wide slot
    /// so every plugin instance, startup-loaded or reloaded, reaches the
    /// same unit.
    #[must_use]
    pub fn with_host_ipc(mut self, host_ipc: HostIpc) -> Self {
        self.host_ipc = host_ipc;
        self
    }

    /// Resolve plugin strings through `locales` instead of the private
    /// embedded-defaults pair [`HostState::new`] binds.
    ///
    /// The loader and reloader call this with the proxy's shared pair so
    /// plugin locale registrations and `/candyriya reload` are visible
    /// to the host functions.
    #[must_use]
    pub fn with_locales(mut self, locales: SharedLocales) -> Self {
        self.locales = Some(locales);
        self
    }

    /// The live-instance registry this state participates in. Cloning the
    /// `Arc` shares the same counts.
    pub fn instance_slots(&self) -> Arc<InstanceSlots> {
        Arc::clone(&self.instance_slots)
    }
}
