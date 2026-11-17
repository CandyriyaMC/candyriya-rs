//! Typed configuration schema.
//!
//! Every field is documented, deserialized from kebab-case TOML keys via
//! figment, and implemented with sensible [`Default`]s. Semantic checks
//! (port ranges, cross-field references, control characters) live in
//! [`crate::validate`], not here; this module only describes the shape.

use std::collections::HashMap;
use std::time::Duration;

use serde::Deserialize;

use crate::duration::deserialize_duration;
use crate::secret::ForwardingSecret;

/// The only configuration version this build understands.
pub const SUPPORTED_CONFIG_VERSION: &str = "0.1";

/// Player info forwarding mode towards backends.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
pub enum ForwardingMode {
    /// Modern forwarding: the backend pulls the player profile from the
    /// proxy over a plugin message channel, verified with an HMAC secret.
    /// Preferred; works for 1.13+ backends.
    #[default]
    #[serde(rename = "modern")]
    Modern,
    /// Like legacy mode, plus a `bungeeguard-token` property carrying the
    /// forwarding secret in the profile JSON. BungeeGuard-compatible.
    #[serde(rename = "bungeeguard")]
    BungeeGuard,
    /// Legacy forwarding: the client IP and profile are injected into the
    /// login plugin message / handshake address. No secret verification.
    #[serde(rename = "legacy")]
    Legacy,
    /// Forwarding disabled: backends see the proxy's own address.
    #[serde(rename = "disabled")]
    Disabled,
}

/// `[protocol]`: protocol registry handling.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ProtocolConfig {
    /// Refresh the embedded protocol registry from `update-url` on startup
    /// when a newer version is published. Defaults to `true`.
    pub auto_update: bool,
    /// Raw URL of the PrismarineJS `protocolVersions.json` data file.
    pub update_url: String,
}

impl Default for ProtocolConfig {
    fn default() -> Self {
        Self {
            auto_update: true,
            update_url: "https://raw.githubusercontent.com/PrismarineJS/minecraft-data/master/data/pc/common/protocolVersions.json"
                .to_string(),
        }
    }
}

/// `[servers]`: named backend servers and the fallback order.
///
/// The `try` key inside `[servers]` holds the names of the servers to try
/// in order. It is required whenever at least one server is configured and
/// must reference existing servers — enforced at validation time. The
/// server name `try` is reserved.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct ServersConfig {
    /// Ordered list of server names to try when the target server is
    /// unreachable or kicks the player. Required when [`ServersConfig::servers`]
    /// is non-empty.
    #[serde(rename = "try", default)]
    pub try_servers: Vec<String>,
    /// Configured backend servers as `name = "host:port"`.
    #[serde(flatten)]
    pub servers: HashMap<String, String>,
}

/// `[forced-hosts]` is represented by this bare map on the root config:
/// `hostname = ["server-a", "server-b"]`.
pub type ForcedHosts = HashMap<String, Vec<String>>;

/// `[advanced]`: knobs most operators never need to touch.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct AdvancedConfig {
    /// Packet size threshold for zlib compression, in bytes. `-1` disables
    /// compression. Defaults to `256`.
    pub compression_threshold: i32,
    /// zlib compression level, `-1..=9`. `-1` uses the zlib default.
    /// `9` is the strongest (slowest) compression. Defaults to `-1`.
    pub compression_level: i32,
    /// Milliseconds a login connection may be pending before it is
    /// disconnected. Defaults to `3000`.
    pub login_ratelimit: u32,
    /// Milliseconds without data before an established connection is
    /// dropped. Defaults to `3000`.
    pub connection_timeout: u32,
    /// Milliseconds to wait for a read before timing out. Defaults to
    /// `30000`.
    pub read_timeout: u32,
    /// Enable the HAProxy protocol for the first frame of every connection.
    /// Defaults to `false`.
    pub haproxy_protocol: bool,
    /// Enable TCP Fast Open where the OS supports it. Defaults to `false`.
    pub tcp_fast_open: bool,
    /// Answer the legacy BungeeCord plugin messaging channel
    /// (`bungeecord:main`). Defaults to `true`.
    pub bungee_plugin_message_channel: bool,
    /// Log every server-list ping request. Verbose; defaults to `false`.
    pub show_ping_requests: bool,
    /// Announce proxy commands (tab-complete) to clients. Defaults to
    /// `true`.
    pub announce_proxy_commands: bool,
    /// Move players to the next `try` server when they are kicked for an
    /// unexpected disconnect from the backend. Defaults to `true`.
    pub failover_on_unexpected_server_disconnect: bool,
    /// Log every command executed by players. Defaults to `false`.
    pub log_command_executions: bool,
    /// Log player connections and disconnections. Defaults to `true`.
    pub log_player_connections: bool,
    /// Accept `minecraft:transfer` intent from clients (transfers to other
    /// proxies). Defaults to `false`.
    pub accepts_transfers: bool,
    /// `SO_REUSEPORT` on the listener: kernel-level load balancing across
    /// threads, Linux only. Defaults to `false`.
    pub enable_reuse_port: bool,
    /// Milliseconds between accepted commands per player. Defaults to `50`.
    pub command_rate_limit: u32,
    /// Milliseconds between tab-complete requests per player. Defaults to
    /// `10`.
    pub tab_complete_rate_limit: u32,
    /// Global connection cap enforced with a semaphore. Defaults to `1024`.
    pub max_connections: u32,
    /// Milliseconds of silence on a handshake before it is dropped.
    /// Defaults to `5000`.
    pub handshake_timeout: u32,
    /// Maximum number of new connections accepted per second. `0` means
    /// unlimited. Defaults to `0`.
    pub max_new_connections_per_second: u32,
}

impl Default for AdvancedConfig {
    fn default() -> Self {
        Self {
            compression_threshold: 256,
            compression_level: -1,
            login_ratelimit: 3000,
            connection_timeout: 3000,
            read_timeout: 30000,
            haproxy_protocol: false,
            tcp_fast_open: false,
            bungee_plugin_message_channel: true,
            show_ping_requests: false,
            announce_proxy_commands: true,
            failover_on_unexpected_server_disconnect: true,
            log_command_executions: false,
            log_player_connections: true,
            accepts_transfers: false,
            enable_reuse_port: false,
            command_rate_limit: 50,
            tab_complete_rate_limit: 10,
            max_connections: 1024,
            handshake_timeout: 5000,
            max_new_connections_per_second: 0,
        }
    }
}

/// `[query]`: GameSpy4 query responses (`query.protocol` in server.properties).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct QueryConfig {
    /// Enable the query interface. Disabled by default; only enable for
    /// trusted networks.
    pub enabled: bool,
    /// Port for the query service. Defaults to `25565`, valid range
    /// `1..=65535`.
    pub port: u32,
    /// World map name reported to query clients. Defaults to `"candyriya"`.
    pub map: String,
    /// Report the plugin list through the full query response. Defaults to
    /// `false`.
    pub show_plugins: bool,
}

impl Default for QueryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: 25565,
            map: "candyriya".to_string(),
            show_plugins: false,
        }
    }
}

/// `[permissions]`: permission provider selection and static grants.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct PermissionsConfig {
    /// Which permission provider to use. `"config"` is the built-in file
    /// provider; plugins may swap in others. Defaults to `"config"`.
    pub provider: String,
    /// Group assigned to players with no explicit assignment. Defaults to
    /// `"default"`.
    pub default: String,
    /// Static group → permission list table. Structure-validated only:
    /// keys are group names, values are lists of permission strings.
    /// Semantics are up to the provider.
    #[serde(default)]
    pub grants: HashMap<String, Vec<String>>,
}

impl Default for PermissionsConfig {
    fn default() -> Self {
        Self {
            provider: "config".to_string(),
            default: "default".to_string(),
            grants: HashMap::new(),
        }
    }
}

/// `[plugins]`: WASM plugin sandbox limits.
///
/// Everything here is operator-facing; the plugin runtime maps this section
/// onto its own `Limits` knobs (see `LimitBuilder::from_config`). `0` means
/// "unlimited" for `max-fuel` and `max-calls`; byte/page caps must be at
/// least 1 when set, which validation enforces where a zero is unusable.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct PluginsConfig {
    /// Maximum linear-memory pages (64 KiB each) a plugin may use.
    /// `64` is 4 MiB. Must be at least `1`. Defaults to `64`.
    pub max_memory_pages: u32,
    /// Fuel per plugin call; `0` means unlimited. Defaults to `0`.
    pub max_fuel: u64,
    /// Maximum plugin calls per plugin per 5-second window; `0` means
    /// unlimited. Defaults to `2000`.
    pub max_calls: u32,
    /// Maximum wall-clock time for a single plugin call, as a string like
    /// `"250ms"`, `"5s"`, `"2m"` or `"1h"`. Must be at least 1 ms.
    /// Defaults to `"5s"`.
    #[serde(deserialize_with = "deserialize_duration")]
    pub max_call_timeout: Duration,
    /// Maximum bytes of output one plugin call may return. Defaults to
    /// `1048576` (1 MiB).
    pub max_output_bytes: usize,
    /// Maximum bytes the host may send into one plugin call. Defaults to
    /// `16777216` (16 MiB).
    pub max_input_bytes: usize,
    /// Maximum bytes of plugin log output per invocation. Defaults to
    /// `1048576` (1 MiB).
    pub max_log_bytes: usize,
}

impl Default for PluginsConfig {
    fn default() -> Self {
        Self {
            max_memory_pages: 64,
            max_fuel: 0,
            max_calls: 2000,
            max_call_timeout: Duration::from_secs(5),
            max_output_bytes: 1_048_576,
            max_input_bytes: 16_777_216,
            max_log_bytes: 1_048_576,
        }
    }
}

/// `[metrics]`: metrics export.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct MetricsConfig {
    /// Enable the Prometheus exporter. Defaults to `false`.
    pub prometheus: bool,
    /// Listen address (`host:port`) for the Prometheus endpoint. Defaults
    /// to `"0.0.0.0:9124"`.
    pub prometheus_address: String,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            prometheus: false,
            prometheus_address: "0.0.0.0:9124".to_string(),
        }
    }
}

fn default_worker_threads() -> usize {
    num_cpus::get()
}

/// `[runtime]`: thread pool sizes. Both default to the number of logical
/// CPUs; pinning them is an operator decision.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct RuntimeConfig {
    /// Tokio worker threads. Defaults to `num_cpus::get()`.
    #[serde(default = "default_worker_threads")]
    pub worker_threads: usize,
    /// Blocking (CPU-bound) threads for the rayon pool. Defaults to
    /// `num_cpus::get()`.
    #[serde(default = "default_worker_threads")]
    pub blocking_threads: usize,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            worker_threads: default_worker_threads(),
            blocking_threads: default_worker_threads(),
        }
    }
}

/// The root configuration.
///
/// Constructed via [`crate::Config::load`] (preferred) or [`Default::default`]
/// (embedded defaults; the forwarding secret is an empty placeholder until
/// loaded from disk). Cloning is cheap relative to disk work but copies the
/// secret; prefer sharing via `Arc` (see [`crate::ConfigStore`]).
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Config {
    /// Schema version of the configuration file. Must be `"0.1"`.
    #[serde(rename = "config-version")]
    pub config_version: String,
    /// Address the proxy listens on, `host:port`. IPv6 hosts must be
    /// bracketed, e.g. `[::1]:25565`. Defaults to `"0.0.0.0:25565"`;
    /// the port must be in `1..=65535`.
    pub bind: String,
    /// Message shown in the server list. MiniMessage formatting only; never
    /// §-codes. Defaults to `"<aqua>A Candyriya network"`.
    pub motd: String,
    /// `max` value reported in the server list ping. Defaults to `100`.
    ///
    /// Note: despite the name this is not a hard connection cap; see
    /// `[advanced] max-connections` for that.
    pub show_max_players: u32,
    /// Path of the PNG favicon served in the server list ping. Relative
    /// paths are resolved against the process working directory. Defaults
    /// to `"assets/favicon.png"`; a missing or unreadable file is not an
    /// error — the proxy serves an embedded transparent 64x64 icon instead.
    pub motd_favicon: String,
    /// Online mode: verify player identities against Mojang session
    /// servers. Setting this to `false` is required for offline accounts
    /// and must be combined with backend `online-mode=false`. Defaults to
    /// `true`.
    pub online_mode: bool,
    /// Require signed player keys on login (1.19+ velocity modernity
    /// checks). Defaults to `true`.
    pub force_key_authentication: bool,
    /// How the proxy proves itself to backends. Defaults to `"modern"`.
    pub player_info_forwarding_mode: ForwardingMode,
    /// Path of the forwarding secret file. Relative paths are resolved
    /// against the directory containing the configuration file. The file is
    /// created with a fresh 32-byte secret on first use when missing.
    /// Defaults to `"forwarding.secret"`.
    pub forwarding_secret_file: String,
    /// Drops connections from players whose handshake address looks like a
    /// proxied Bungee address rather than a real client. Defaults to
    /// `false`.
    pub prevent_client_proxy_connections: bool,
    /// Log the real player addresses (and X-Forwarded-For style headers
    /// from other proxies). Defaults to `true`.
    pub enable_player_address_logging: bool,
    /// Announce Forge clients can connect. Defaults to `false`.
    pub announce_forge: bool,
    /// Kick existing players when a new connection claims the same
    /// identity. Defaults to `false`.
    pub kick_existing_players: bool,
    /// Locale for server-generated messages (e.g. `"en"`). Defaults to
    /// `"en"`.
    pub locale: String,
    /// `[protocol]` section.
    #[serde(default)]
    pub protocol: ProtocolConfig,
    /// `[servers]` section: backend names, addresses and the `try` order.
    #[serde(default)]
    pub servers: ServersConfig,
    /// `[forced-hosts]` section: hostname → server names map.
    #[serde(rename = "forced-hosts", default)]
    pub forced_hosts: ForcedHosts,
    /// `[advanced]` section.
    #[serde(default)]
    pub advanced: AdvancedConfig,
    /// `[query]` section.
    #[serde(default)]
    pub query: QueryConfig,
    /// `[permissions]` section.
    #[serde(default)]
    pub permissions: PermissionsConfig,
    /// `[metrics]` section.
    #[serde(default)]
    pub metrics: MetricsConfig,
    /// `[plugins]` section.
    #[serde(default)]
    pub plugins: PluginsConfig,
    /// `[runtime]` section.
    #[serde(default)]
    pub runtime: RuntimeConfig,
    /// The forwarding secret resolved from disk (or generated) at load
    /// time. Never serialized, never printed; `Debug` shows `***`.
    #[serde(skip)]
    pub forwarding_secret: ForwardingSecret,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            config_version: SUPPORTED_CONFIG_VERSION.to_string(),
            bind: "0.0.0.0:25565".to_string(),
            motd: "<aqua>A Candyriya network".to_string(),
            show_max_players: 100,
            motd_favicon: "assets/favicon.png".to_string(),
            online_mode: true,
            force_key_authentication: true,
            player_info_forwarding_mode: ForwardingMode::Modern,
            forwarding_secret_file: "forwarding.secret".to_string(),
            prevent_client_proxy_connections: false,
            enable_player_address_logging: true,
            announce_forge: false,
            kick_existing_players: false,
            locale: "en".to_string(),
            protocol: ProtocolConfig::default(),
            servers: ServersConfig::default(),
            forced_hosts: ForcedHosts::new(),
            advanced: AdvancedConfig::default(),
            query: QueryConfig::default(),
            permissions: PermissionsConfig::default(),
            metrics: MetricsConfig::default(),
            plugins: PluginsConfig::default(),
            runtime: RuntimeConfig::default(),
            forwarding_secret: ForwardingSecret::default(),
        }
    }
}
