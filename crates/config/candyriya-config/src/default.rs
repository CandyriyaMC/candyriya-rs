//! The embedded default configuration and its commented template.
//!
//! [`DEFAULT_CONFIG_TOML`] is both (a) the base layer for every load, so
//! omitted keys inherit these values, and (b) the template written for
//! operators to copy. It must always extract into exactly
//! [`Config::default`](crate::Config::default) — there is a test for that.
//! Keep the two in sync when editing either side.

/// The embedded default configuration file.
///
/// Real content, fully commented, in the style of the classic Minecraft
/// proxy default file. Layers: these values are the base; the user's file
/// (if any) overrides them key by key. The two `[runtime]` knobs are
/// commented out because their defaults are the number of logical CPUs,
/// which cannot be expressed in a static file.
pub const DEFAULT_CONFIG_TOML: &str = r#"# candyriya configuration
# Documentation: https://bejiihiu.github.io/candyriya-rs
#
# The proxy ships sane defaults; uncommented keys below show them.
# Values in this file are the base layer — your candyriya.toml overrides
# only the keys it mentions. Unknown keys are ignored with a warning, never
# an error. A missing file is fine: these defaults are used and a forwarding
# secret is generated on first run.

# Schema version of this file. Must be "0.1".
config-version = "0.1"

# The address the proxy binds to, as host:port.
# Use 0.0.0.0 (or [::]) to listen on all interfaces, or a specific IP to
# pin one. The port must be in 1..=65535.
bind = "0.0.0.0:25565"

# Message shown in the server list ("multiplayer" screen).
# MiniMessage formatting only, never §-codes. Newlines need the TOML escape.
motd = "<aqua>A Candyriya network"

# The "max players" value shown in the ping response. This is cosmetic;
# the real connection cap is [advanced] max-connections.
show-max-players = 100

# PNG image shown in the server list next to the MOTD. Relative paths
# resolve against the process working directory. If the file is missing,
# unreadable or not a PNG, the proxy serves an embedded transparent icon.
motd-favicon = "assets/favicon.png"

# Online mode against Mojang session servers: true = premium players only.
# If you set this to false you must also set online-mode=false on every
# backend, or the backends will reject proxied connections.
online-mode = true

# Require modern signed chat/profile keys from authenticated clients.
# Turn off only if you know what you're doing (offline networks should).
force-key-authentication = true

# How this proxy proves to backends that it is the one talking to them:
#   "modern"      — backend pulls the profile over a plugin messaging channel,
#                   authenticated with the forwarding secret (recommended)
#   "bungeeguard" — legacy mode plus a bungeeguard-token plugin message field
#   "legacy"      — unauthenticated legacy forwarding (IP + profile JSON)
#   "disabled"    — no forwarding; backends see the proxy's address
player-info-forwarding-mode = "modern"

# File holding the 32-byte forwarding secret, base64-encoded. Relative
# paths resolve against the directory containing this configuration file.
# If the file is missing it is created with a freshly generated secret and
# mode 0600 (unix). Keep this file private — it authenticates this proxy
# to every backend. Rotate by deleting it and restarting.
forwarding-secret-file = "forwarding.secret"

# Drop connections whose handshake carries a proxied Bungee/Velocity address
# from someone who isn't a trusted proxy (cheap anti-abuse).
prevent-client-proxy-connections = false

# Log the real client address on connect (instead of the proxy's view).
enable-player-address-logging = true

# Announce Forge server affinity: lets Forge clients see the proxy as an
# acceptable endpoint instead of bailing on version checks.
announce-forge = false

# Kick an already-connected player when a new connection claims the same
# identity (otherwise the new login is rejected).
kick-existing-players = false

# Locale used for server-generated messages. Only "en" ships by default.
locale = "en"

[protocol]
# Automatically refresh the embedded protocol registry from update-url at
# startup when a newer PrismarineJS protocolVersions.json is published.
# Fetching is best effort: on network failure the embedded registry is
# used and a warning is logged.
auto-update = true

# Raw URL of the PrismarineJS protocolVersions.json. Change with care —
# the payload is validated before it replaces the embedded registry.
update-url = "https://raw.githubusercontent.com/PrismarineJS/minecraft-data/master/data/pc/common/protocolVersions.json"

[servers]
# Ordered fallback list — the server names a player is switched to when the
# current one is unreachable or kicks them. REQUIRED when at least one
# backend is configured; every name must exist below.
try = []

# Backends, name → host:port:
# lobby = "127.0.0.1:25565"
# minigames = "127.0.0.1:25566"

[forced-hosts]
# Hostname → ordered server names forced for clients connecting to that
# exact host. Wildcards are NOT expanded here (see docs for wildcard rules).
# Every server name must exist under [servers].
# "play.example.com" = ["lobby"]
# "minigames.example.com" = ["minigames", "lobby"]

[advanced]
# Packets smaller than this are sent uncompressed. -1 disables compression
# entirely (not recommended for real networks).
compression-threshold = 256
# zlib compression level, -1..=9. -1 = zlib default (6). 9 is smallest and
# slowest.
compression-level = -1
# Milliseconds a login may stay pending before it is dropped.
login-ratelimit = 3000
# Milliseconds without traffic on an established connection before it is
# dropped. Vanilla-ish value; lower for twitchy DDoS protection.
connection-timeout = 3000
# Milliseconds a single read may take before the connection is dropped.
read-timeout = 30000
# Wrap the first connection frame in the HAProxy PROXY protocol. Only
# enable behind a load balancer that speaks it, or you will lock yourself
# out.
haproxy-protocol = false
# TCP Fast Open for the listener (Linux 3.7+; elsewhere a no-op).
tcp-fast-open = false
# Answer the legacy BungeeCord plugin messaging channel (bungeecord:main)
# so old backends can talk to the proxy. Disable for strict networks.
bungee-plugin-message-channel = true
# Log every server-list ping (verbose).
show-ping-requests = false
# Announce the proxy command set to clients (tab-complete).
announce-proxy-commands = true
# When the backend disconnects a player unexpectedly, move them to the
# next server in `try` instead of kicking them.
failover-on-unexpected-server-disconnect = true
# Log every command executed by players (audit trail; noisy).
log-command-executions = false
# Log player connects and disconnects with real addresses.
log-player-connections = true
# Accept the minecraft:transfer intent, letting colleagues proxies hand
# players off to us. Off by default — enable only in trusted fleets.
accepts-transfers = false
# SO_REUSEPORT on the listener: kernel spreads new connections across
# threads. Linux only; ignored elsewhere.
enable-reuse-port = false
# Milliseconds between accepted commands per player.
command-rate-limit = 50
# Milliseconds between accepted tab-complete requests per player.
tab-complete-rate-limit = 10
# Global connection cap enforced with a semaphore.
max-connections = 1024
# Milliseconds of silence on a handshake before it is dropped.
handshake-timeout = 5000
# New connections accepted per second. 0 = unlimited (the default).
max-new-connections-per-second = 0

[query]
# GameSpy4 query (what "query.protocol" in server.properties exposes).
# Only enable on trusted networks — it leaks player counts and plugins.
enabled = false
port = 25565
map = "candyriya"
show-plugins = false

[permissions]
# Built-in static provider. Third-party providers (database-backed) may be
# installed by plugins and named here instead.
provider = "config"
# Group assigned to players with no explicit assignment.
default = "default"
# Static grants: group name → list of permission nodes. This table is only
# structure-checked (keys are group names, values are string lists); the
# semantics belong to the provider. Example:
#   admin = ["*"]
#   moderator = ["candyriya.command.*", "candyriya.kick"]
grants = {}

[metrics]
# Prometheus exporter. Scraped by whatever your monitoring is — default off.
prometheus = false
prometheus-address = "0.0.0.0:9124"

[runtime]
# Thread pool sizes. Both default to the number of logical CPUs, which is
# the right answer for almost everyone — that's why these are not in the
# static file. Uncomment to pin them explicitly:
# worker-threads = 8
# blocking-threads = 8
"#;

/// Asserts at compile time that the template exists and is not accidentally
/// emptied; the extraction check lives in unit tests.
const _: () = assert!(!DEFAULT_CONFIG_TOML.is_empty());

/// Extracts the embedded defaults without touching disk.
///
/// Test-only: loaders layer [`DEFAULT_CONFIG_TOML`] through figment instead
/// of this shortcut. Test code is allowed to abort on drift between the
/// template and the schema — that is exactly what the test is for.
#[cfg(test)]
pub(crate) fn embedded_defaults() -> crate::config::Config {
    figment::Figment::new()
        .merge({
            use figment::providers::Format as _;
            figment::providers::Toml::string(DEFAULT_CONFIG_TOML)
        })
        .extract::<crate::config::Config>()
        .unwrap_or_else(|error| panic!("embedded default template failed to extract: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_extracts_to_config_default() {
        assert_eq!(embedded_defaults(), crate::config::Config::default());
    }
}
